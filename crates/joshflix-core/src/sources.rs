//! Stream sources (from Comet) and their ranking.

use serde::{Deserialize, Serialize};

use crate::release::{self, DisplayCaps, Hidden, ReleaseInfo, StreamFilters};

/// One playable source for a movie or episode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Source {
    /// Lowercase 40-character BTIH.
    pub info_hash: String,
    /// Index of the file inside the torrent (0-based, torrent order), when known.
    pub file_idx: Option<u32>,
    /// File name hint for the playable file.
    pub filename: Option<String>,
    /// Binge group, used to pick the same release for the next episode.
    pub binge_group: Option<String>,
    /// Trackers for the magnet link (may be empty; Real-Debrid also uses DHT).
    #[serde(default)]
    pub trackers: Vec<String>,
    /// Already cached on Real-Debrid (Comet's ⚡): plays instantly.
    #[serde(default)]
    pub cached: bool,
    /// Comet playback URL that redirects to the Real-Debrid stream.
    #[serde(default)]
    pub playback_url: Option<String>,
    pub release: ReleaseInfo,
}

impl Source {
    /// Magnet link with the torrent's trackers.
    pub fn magnet(&self) -> String {
        let mut magnet = format!("magnet:?xt=urn:btih:{}", self.info_hash);
        for tracker in self.trackers.iter().take(12) {
            magnet.push_str("&tr=");
            magnet.extend(url::form_urlencoded::byte_serialize(tracker.as_bytes()));
        }
        magnet
    }
}

/// Sources after filtering and ranking.
#[derive(Debug, Clone, Default)]
pub struct RankedSources {
    /// Visible sources, best first.
    pub visible: Vec<Source>,
    /// Hidden by filters, best first (shown by "Show all sources").
    pub hidden: Vec<(Source, Hidden)>,
}

impl RankedSources {
    pub fn rank(streams: Vec<Source>, filters: &StreamFilters, caps: &DisplayCaps) -> Self {
        let mut scored: Vec<(i64, Source)> = streams
            .into_iter()
            .map(|s| (release::score(&s.release, caps), s))
            .collect();
        // Cached sources first (they start instantly), then quality and health.
        scored.sort_by(|(a_score, a), (b_score, b)| {
            b.cached
                .cmp(&a.cached)
                .then(b_score.cmp(a_score))
                .then(b.release.seeders.cmp(&a.release.seeders))
                .then(b.release.size_bytes.cmp(&a.release.size_bytes))
        });
        let mut ranked = Self::default();
        for (_, stream) in scored {
            match release::check(&stream.release, filters, caps) {
                Ok(()) => ranked.visible.push(stream),
                Err(reason) => ranked.hidden.push((stream, reason)),
            }
        }
        ranked
    }

    /// All sources (visible first), for "Show all sources".
    pub fn all(&self) -> Vec<Source> {
        self.visible
            .iter()
            .cloned()
            .chain(self.hidden.iter().map(|(s, _)| s.clone()))
            .collect()
    }
}

/// Pick the source for the next episode: same binge group if possible, then
/// same release group and resolution, then the best visible source.
pub fn pick_for_binge<'a>(
    ranked: &'a RankedSources,
    previous: Option<&Source>,
) -> Option<&'a Source> {
    let previous = match previous {
        Some(previous) => previous,
        None => return ranked.visible.first(),
    };
    let same_binge = |s: &&Source| {
        s.binge_group.is_some()
            && s.binge_group == previous.binge_group
            // Torrentio uses the info hash as the binge group for packs: same pack, same group.
            || s.info_hash == previous.info_hash
    };
    let same_release = |s: &&Source| {
        s.release.group.is_some()
            && s.release.group == previous.release.group
            && s.release.resolution == previous.release.resolution
    };
    ranked
        .visible
        .iter()
        .find(same_binge)
        .or_else(|| ranked.visible.iter().find(same_release))
        .or_else(|| ranked.visible.first())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn source(hash: char, name: &str, title: &str, cached: bool) -> Source {
        Source {
            info_hash: hash.to_string().repeat(40),
            file_idx: None,
            filename: None,
            binge_group: Some(format!("comet|realdebrid|{}", hash.to_string().repeat(40))),
            trackers: vec!["udp://tracker.opentrackr.org:1337/announce".into()],
            cached,
            playback_url: None,
            release: ReleaseInfo::parse(name, title),
        }
    }

    #[test]
    fn ranks_cached_first_and_filters() {
        let uhd = source('a', "Torrentio\n4k DV | HDR", "Dune.2024.2160p.WEB-DL.DV.HDR.H.265-FLUX\n👤 568 💾 29.26 GB ⚙️ T", false);
        let hd = source('b', "Torrentio\n1080p", "Dune.2024.1080p.WEB-DL.x264-FLUX\n👤 900 💾 6 GB ⚙️ X", true);
        let cam = source('c', "Torrentio\nCAM", "Dune.2024.CAM\n👤 9000 💾 1 GB ⚙️ X", true);
        assert!(uhd.magnet().contains("&tr=udp%3A%2F%2Ftracker.opentrackr.org"));

        let ranked = RankedSources::rank(vec![uhd.clone(), hd.clone(), cam.clone()], &StreamFilters::default(), &DisplayCaps::EVERYTHING);
        let order: Vec<_> = ranked.visible.iter().map(|s| s.info_hash.chars().next().unwrap()).collect();
        assert_eq!(order, ['b', 'a']);
        assert_eq!(ranked.hidden.len(), 1);

        let sdr = RankedSources::rank(vec![uhd, hd, cam], &StreamFilters::default(), &DisplayCaps::SDR_1080);
        assert_eq!(sdr.visible.len(), 1);
        assert_eq!(sdr.all().len(), 3);
    }

    #[test]
    fn binge_prefers_same_group() {
        let uhd = source('a', "Torrentio\n4k", "Show.S01E01.2160p.WEB-DL-NTb\n👤 50 💾 9 GB ⚙️ T", true);
        let hd = source('b', "Torrentio\n1080p", "Show.S01E02.1080p.WEB-DL.x264-FLUX\n👤 900 💾 2 GB ⚙️ X", true);
        let ranked = RankedSources::rank(vec![uhd.clone(), hd.clone()], &StreamFilters::default(), &DisplayCaps::EVERYTHING);
        let mut previous = source('z', "Torrentio\n1080p", "Show.S01E01.1080p.WEB-DL.x264-FLUX\n👤 9 💾 2 GB ⚙️ X", true);
        previous.binge_group = None;
        assert_eq!(pick_for_binge(&ranked, Some(&previous)).unwrap().info_hash, hd.info_hash);
        assert_eq!(pick_for_binge(&ranked, None).unwrap().info_hash, uhd.info_hash);
    }
}
