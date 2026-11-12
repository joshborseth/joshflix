//! Where an MP4/MOV file keeps each track's data.
//!
//! Some MP4s aren't interleaved: all the audio sits in one block, the video in
//! another and captions at the very end. A demuxer reading such a file over
//! one HTTP connection jumps between those blocks for nearly every packet,
//! and each jump is a new range request, so playback can't keep up (a Dance
//! Moms episode: about 77 requests per second, 4 s of video in 2 minutes).
//! [`probe`] reads only the file's `moov` box and reports which tracks lie
//! far from the video, so the player can read them over their own
//! connections.

use std::time::Duration;

use reqwest::header::{CONTENT_RANGE, RANGE};

use crate::error::{CoreError, Result};
use crate::http::Http;

const SERVICE: &str = "the stream";
/// Larger `moov` boxes aren't worth reading just to decide this.
const MAX_MOOV: u64 = 48 << 20;
/// Top-level boxes to step over before giving up on finding `moov`.
const MAX_TOP_LEVEL_BOXES: usize = 32;
/// The first request covers this much, which usually holds every top-level
/// header before `mdat` (and a front `moov` of a short file).
const FIRST_READ: u64 = 64 << 10;
/// A track is "far" when its data for the same moment sits this many bytes
/// from the video's, or [`FAR_SECONDS`] of the file's average bitrate if that
/// is more. Interleaved files stay within about one chunk (a second or less).
const FAR_BYTES: u64 = 8 << 20;
const FAR_SECONDS: f64 = 5.0;
/// Points along the timeline where the tracks' positions are compared.
const SAMPLE_POINTS: u32 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mp4TrackKind {
    Video,
    Audio,
    Subtitle,
    Other,
}

/// One track's chunks: where each starts in the file and when it starts.
#[derive(Debug, Clone, PartialEq)]
pub struct Mp4Track {
    /// `track_ID` from `tkhd` (FFmpeg's stream id, mpv's `src-id`).
    pub id: u32,
    pub kind: Mp4TrackKind,
    /// File offset of each chunk.
    offsets: Vec<u64>,
    /// Start time of each chunk, in the track's own time units.
    starts: Vec<u64>,
    /// Total duration in the same units.
    duration: u64,
    samples: u64,
}

impl Mp4Track {
    /// File offset of the chunk playing at `fraction` (0..=1) of the track.
    fn offset_at(&self, fraction: f64) -> u64 {
        let time = (self.duration as f64 * fraction) as u64;
        let i = self.starts.partition_point(|&start| start <= time).saturating_sub(1);
        self.offsets[i]
    }
}

/// The file's tracks and size.
#[derive(Debug, Clone, PartialEq)]
pub struct Mp4Layout {
    pub tracks: Vec<Mp4Track>,
    /// Movie duration in seconds, when the header gives one.
    pub duration_secs: Option<f64>,
    /// File size in bytes, when the server reports it.
    pub file_size: Option<u64>,
}

/// Track ids (`tkhd` track_ID) whose data is far from the video's, so they
/// should be read over their own connection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Separate {
    pub audio: Vec<u32>,
    pub subtitles: Vec<u32>,
}

impl Separate {
    pub fn any(&self) -> bool {
        !self.audio.is_empty() || !self.subtitles.is_empty()
    }
}

impl Mp4Layout {
    /// The main video track: the one with the most samples.
    fn video(&self) -> Option<&Mp4Track> {
        self.tracks.iter().filter(|t| t.kind == Mp4TrackKind::Video).max_by_key(|t| t.samples)
    }

    /// The largest distance, in bytes, between a track's data and the
    /// video's data for the same moment.
    pub fn distance_from_video(&self, track: &Mp4Track) -> Option<u64> {
        let video = self.video()?;
        (0..=SAMPLE_POINTS)
            .map(|i| {
                let fraction = f64::from(i) / f64::from(SAMPLE_POINTS);
                track.offset_at(fraction).abs_diff(video.offset_at(fraction))
            })
            .max()
    }

    /// Bytes beyond which a track counts as far from the video.
    pub fn far_threshold(&self) -> u64 {
        let by_bitrate = match (self.file_size, self.duration_secs) {
            (Some(size), Some(secs)) if secs > 0.0 => (size as f64 / secs * FAR_SECONDS) as u64,
            _ => 0,
        };
        FAR_BYTES.max(by_bitrate)
    }

    /// Audio and subtitle tracks whose data is far from the video's.
    pub fn separate(&self) -> Separate {
        let threshold = self.far_threshold();
        let far = |kind| {
            self.tracks
                .iter()
                .filter(|t| t.kind == kind && self.distance_from_video(t).is_some_and(|d| d > threshold))
                .map(|t| t.id)
                .collect()
        };
        Separate { audio: far(Mp4TrackKind::Audio), subtitles: far(Mp4TrackKind::Subtitle) }
    }

    /// Parse the payload of a `moov` box (without its 8- or 16-byte header).
    pub fn from_moov(moov: &[u8], file_size: Option<u64>) -> Result<Self> {
        let duration_secs = find(moov, b"mvhd").and_then(movie_duration);
        let tracks = boxes(moov)
            .filter(|(kind, _)| kind == b"trak")
            .filter_map(|(_, trak)| parse_track(trak))
            .collect();
        Ok(Self { tracks, duration_secs, file_size })
    }
}

/// Whether a stream URL names an MP4/MOV file (the only containers probed).
pub fn is_mp4_url(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or_default().to_ascii_lowercase();
    [".mp4", ".m4v", ".mov"].iter().any(|ext| path.ends_with(ext))
}

/// Read the file's `moov` box with range requests and work out its layout.
/// Returns `None` when the file has no usable `moov` (fragmented MP4, `moov`
/// too large, or a server without range support).
pub async fn probe(http: &Http, url: &str) -> Result<Option<Mp4Layout>> {
    let (head, file_size) = fetch(http, url, 0, FIRST_READ).await?;
    let mut offset = 0u64;
    for _ in 0..MAX_TOP_LEVEL_BOXES {
        if file_size.is_some_and(|size| offset + 8 > size) {
            return Ok(None);
        }
        let header = match head.get(offset as usize..).filter(|_| offset + 16 <= head.len() as u64) {
            Some(h) => h[..16].to_vec(),
            None => fetch(http, url, offset, 16).await?.0,
        };
        let Some((kind, size, header_len)) = box_header(&header) else { return Ok(None) };
        let size = match size {
            Some(size) if size >= header_len => size,
            Some(_) => return Ok(None),
            // Runs to the end of the file: nothing after it.
            None => match file_size {
                Some(file_size) if &kind == b"moov" && file_size - offset >= header_len => file_size - offset,
                _ => return Ok(None),
            },
        };
        if &kind == b"moov" {
            let len = size - header_len;
            if len > MAX_MOOV {
                return Ok(None);
            }
            let start = offset + header_len;
            let moov = match head.get(start as usize..(start + len) as usize) {
                Some(bytes) => bytes.to_vec(),
                None => fetch(http, url, start, len).await?.0,
            };
            if (moov.len() as u64) < len {
                return Ok(None);
            }
            return Mp4Layout::from_moov(&moov, file_size).map(Some);
        }
        offset += size;
    }
    Ok(None)
}

/// GET `len` bytes from `start`. Also returns the file size from `Content-Range`.
async fn fetch(http: &Http, url: &str, start: u64, len: u64) -> Result<(Vec<u8>, Option<u64>)> {
    let mut response = http
        .client()
        .get(url)
        .header(RANGE, format!("bytes={start}-{}", start + len - 1))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|source| CoreError::Network { service: SERVICE, source })?;
    let status = response.status().as_u16();
    if status != 206 {
        // A server that ignores ranges would send the whole file.
        return Err(CoreError::Status { service: SERVICE, status });
    }
    let file_size = response
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit('/').next())
        .and_then(|v| v.parse().ok());
    let mut body = Vec::with_capacity(len.min(MAX_MOOV) as usize);
    while let Some(chunk) = response.chunk().await.map_err(|source| CoreError::Network { service: SERVICE, source })? {
        body.extend_from_slice(&chunk);
        if body.len() as u64 >= len {
            body.truncate(len as usize);
            break;
        }
    }
    Ok((body, file_size))
}

/// A box header: type, total size (`None` = to the end of the file) and header length.
fn box_header(bytes: &[u8]) -> Option<([u8; 4], Option<u64>, u64)> {
    let size = u32::from_be_bytes(bytes.get(0..4)?.try_into().ok()?);
    let kind: [u8; 4] = bytes.get(4..8)?.try_into().ok()?;
    match size {
        0 => Some((kind, None, 8)),
        1 => Some((kind, Some(u64::from_be_bytes(bytes.get(8..16)?.try_into().ok()?)), 16)),
        n => Some((kind, Some(u64::from(n)), 8)),
    }
}

/// The child boxes of a container payload.
fn boxes(mut data: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    std::iter::from_fn(move || {
        let (kind, size, header_len) = box_header(data)?;
        let size = size.unwrap_or(data.len() as u64);
        if size < header_len || size > data.len() as u64 {
            return None;
        }
        let payload = &data[header_len as usize..size as usize];
        data = &data[size as usize..];
        Some((kind, payload))
    })
}

fn find<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(data).find(|(k, _)| k == kind).map(|(_, payload)| payload)
}

fn find_path<'a>(data: &'a [u8], path: &[&[u8; 4]]) -> Option<&'a [u8]> {
    path.iter().try_fold(data, |data, kind| find(data, kind))
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(data.get(at..at + 8)?.try_into().ok()?))
}

fn movie_duration(mvhd: &[u8]) -> Option<f64> {
    let (timescale, duration) = if mvhd.first()? == &1 {
        (u32_at(mvhd, 20)?, u64_at(mvhd, 24)?)
    } else {
        (u32_at(mvhd, 12)?, u64::from(u32_at(mvhd, 16)?))
    };
    (timescale > 0).then(|| duration as f64 / f64::from(timescale))
}

/// The `entry_count` of a full box and the bytes after it.
fn table(full_box: &[u8]) -> Option<(usize, &[u8])> {
    Some((u32_at(full_box, 4)? as usize, full_box.get(8..)?))
}

fn parse_track(trak: &[u8]) -> Option<Mp4Track> {
    let tkhd = find(trak, b"tkhd")?;
    let id = u32_at(tkhd, if tkhd.first()? == &1 { 20 } else { 12 })?;
    let mdia = find(trak, b"mdia")?;
    let handler: [u8; 4] = find(mdia, b"hdlr")?.get(8..12)?.try_into().ok()?;
    let kind = match &handler {
        b"vide" => Mp4TrackKind::Video,
        b"soun" => Mp4TrackKind::Audio,
        b"sbtl" | b"subt" | b"text" | b"clcp" => Mp4TrackKind::Subtitle,
        _ => Mp4TrackKind::Other,
    };
    let stbl = find_path(mdia, &[b"minf", b"stbl"])?;

    let be32 = |b: &[u8]| u64::from(u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    let offsets: Vec<u64> = if let Some(stco) = find(stbl, b"stco") {
        let (count, rows) = table(stco)?;
        rows.as_chunks::<4>().0.iter().take(count).map(|r| be32(r)).collect()
    } else {
        let (count, rows) = table(find(stbl, b"co64")?)?;
        rows.as_chunks::<8>().0.iter().take(count).map(|r| u64::from_be_bytes(*r)).collect()
    };
    if offsets.is_empty() {
        return None;
    }

    // Samples per chunk: runs of (first chunk, 1-based; samples in each).
    let (count, rows) = table(find(stbl, b"stsc")?)?;
    let runs: Vec<(u64, u64)> = rows.as_chunks::<12>().0.iter().take(count).map(|r| (be32(&r[0..]), be32(&r[4..]))).collect();
    // Sample durations: runs of (count, duration).
    let (count, rows) = table(find(stbl, b"stts")?)?;
    let durations: Vec<(u64, u64)> = rows.as_chunks::<8>().0.iter().take(count).map(|r| (be32(&r[0..]), be32(&r[4..]))).collect();

    // Walk the chunks, keeping the time at the first sample of each.
    let mut starts = Vec::with_capacity(offsets.len());
    let (mut run, mut stts) = (0usize, 0usize);
    let (mut stts_used, mut time, mut samples) = (0u64, 0u64, 0u64);
    for chunk in 1..=offsets.len() as u64 {
        while runs.get(run + 1).is_some_and(|&(first, _)| first <= chunk) {
            run += 1;
        }
        starts.push(time);
        let mut left = runs.get(run).map_or(0, |&(_, n)| n);
        samples += left;
        while left > 0 {
            let Some(&(n, duration)) = durations.get(stts) else { break };
            let take = left.min(n - stts_used);
            time += take * duration;
            left -= take;
            stts_used += take;
            if stts_used == n {
                stts += 1;
                stts_used = 0;
            }
        }
    }
    if samples == 0 {
        return None;
    }
    Some(Mp4Track { id, kind, offsets, starts, duration: time.max(1), samples })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mp4_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out
    }

    /// A full box with `entries` table rows of the given 32-bit words.
    fn full_box(kind: &[u8; 4], entries: u32, words: &[u32]) -> Vec<u8> {
        let mut payload = vec![0u8; 4];
        payload.extend_from_slice(&entries.to_be_bytes());
        for word in words {
            payload.extend_from_slice(&word.to_be_bytes());
        }
        mp4_box(kind, &payload)
    }

    fn hdlr(handler: &[u8; 4]) -> Vec<u8> {
        let mut payload = vec![0u8; 8];
        payload.extend_from_slice(handler);
        payload.extend_from_slice(&[0; 12]);
        mp4_box(b"hdlr", &payload)
    }

    /// A `trak` from its id, handler and sample tables.
    fn trak_with(id: u32, handler: &[u8; 4], stco: Vec<u8>, stsc: Vec<u8>, stts: Vec<u8>) -> Vec<u8> {
        let tkhd = full_box(b"tkhd", 0, &[0, id, 0, 0]);
        let stbl = mp4_box(b"stbl", &[stco, stsc, stts].concat());
        let mdia = mp4_box(b"mdia", &[hdlr(handler), mp4_box(b"minf", &stbl)].concat());
        mp4_box(b"trak", &[tkhd, mdia].concat())
    }

    /// A track with one sample per chunk, `duration` units each, at `offsets`.
    fn trak(id: u32, handler: &[u8; 4], offsets: &[u32], duration: u32) -> Vec<u8> {
        let count = offsets.len() as u32;
        trak_with(
            id,
            handler,
            full_box(b"stco", count, offsets),
            full_box(b"stsc", 1, &[1, 1, 1]),
            full_box(b"stts", 1, &[count, duration]),
        )
    }

    fn mvhd(timescale: u32, duration: u32) -> Vec<u8> {
        let mut payload = vec![0u8; 12];
        payload.extend_from_slice(&timescale.to_be_bytes());
        payload.extend_from_slice(&duration.to_be_bytes());
        payload.extend_from_slice(&[0; 80]);
        mp4_box(b"mvhd", &payload)
    }

    #[test]
    fn interleaved_file_stays_on_one_connection() {
        // 100 one-second chunks, audio right after each video chunk.
        let video: Vec<u32> = (0..100).map(|i| 1000 + i * 700_000).collect();
        let audio: Vec<u32> = video.iter().map(|o| o + 680_000).collect();
        let moov = [mvhd(1000, 100_000), trak(1, b"vide", &video, 1000), trak(2, b"soun", &audio, 48_000)].concat();
        let layout = Mp4Layout::from_moov(&moov, Some(70_000_000)).unwrap();
        assert_eq!(layout.tracks.len(), 2);
        assert_eq!(layout.separate(), Separate::default());
    }

    #[test]
    fn far_tracks_get_their_own_connection() {
        // All audio first (40 MB), then the video with a second, interleaved
        // audio track, then captions at the end.
        let audio: Vec<u32> = (0..100).map(|i| 200 + i * 400_000).collect();
        let video: Vec<u32> = (0..100).map(|i| 40_000_000 + i * 6_000_000).collect();
        let near_audio: Vec<u32> = video.iter().map(|o| o + 5_800_000).collect();
        let captions: Vec<u32> = (0..10).map(|i| 640_000_000 + i * 20).collect();
        let moov = [
            mvhd(1000, 100_000),
            trak(1, b"soun", &audio, 48_000),
            trak(2, b"vide", &video, 1000),
            trak(3, b"soun", &near_audio, 48_000),
            trak(4, b"clcp", &captions, 10_000),
        ]
        .concat();
        let layout = Mp4Layout::from_moov(&moov, Some(640_001_000)).unwrap();
        assert_eq!(layout.duration_secs, Some(100.0));
        assert_eq!(layout.separate(), Separate { audio: vec![1], subtitles: vec![4] });
    }

    #[test]
    fn chunk_times_follow_samples_per_chunk() {
        // Chunks 1-2 hold 2 samples each, chunk 3 onwards 1; every sample lasts 10.
        let trak = trak_with(
            7,
            b"soun",
            full_box(b"stco", 4, &[100, 200, 300, 400]),
            full_box(b"stsc", 2, &[1, 2, 1, 3, 1, 1]),
            full_box(b"stts", 1, &[6, 10]),
        );
        let layout = Mp4Layout::from_moov(&trak, None).unwrap();
        let track = &layout.tracks[0];
        assert_eq!(track.id, 7);
        assert_eq!(track.starts, vec![0, 20, 40, 50]);
        assert_eq!(track.samples, 6);
        assert_eq!(track.offset_at(0.45), 200);
    }

    #[test]
    fn recognizes_mp4_urls() {
        assert!(is_mp4_url("https://x.download.real-debrid.com/d/ABC/S02E07%20Bullets%20and%20Ballet.mp4"));
        assert!(is_mp4_url("https://host/file.M4V?token=1"));
        assert!(!is_mp4_url("https://host/file.mkv"));
    }
}
