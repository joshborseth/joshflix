//! Release-name parsing, filtering and ranking.
//!
//! Torrentio describes each source with a short `name` ("Torrentio\n4k DV | HDR")
//! and a multi-line `title`: the release name, an optional file path inside a
//! pack, a stats line ("👤 568 💾 29.26 GB ⚙️ TorrentGalaxy") and an optional
//! language line ("🇬🇧 / 🇷🇺"). [`ReleaseInfo::parse`] turns that into fields the
//! UI can show as badges and the ranker can score.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Resolution {
    Unknown,
    R480,
    R576,
    R720,
    R1080,
    R1440,
    R2160,
}

impl Resolution {
    pub fn height(self) -> Option<u32> {
        Some(match self {
            Self::Unknown => return None,
            Self::R480 => 480,
            Self::R576 => 576,
            Self::R720 => 720,
            Self::R1080 => 1080,
            Self::R1440 => 1440,
            Self::R2160 => 2160,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "SD",
            Self::R480 => "480p",
            Self::R576 => "576p",
            Self::R720 => "720p",
            Self::R1080 => "1080p",
            Self::R1440 => "1440p",
            Self::R2160 => "4K",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Source {
    Remux,
    BluRay,
    WebDl,
    WebRip,
    Hdtv,
    Dvd,
    Cam,
    Telesync,
    Telecine,
    Screener,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Self::Remux => "REMUX",
            Self::BluRay => "BluRay",
            Self::WebDl => "WEB-DL",
            Self::WebRip => "WEBRip",
            Self::Hdtv => "HDTV",
            Self::Dvd => "DVD",
            Self::Cam => "CAM",
            Self::Telesync => "TS",
            Self::Telecine => "TC",
            Self::Screener => "SCR",
        }
    }

    pub fn is_junk(self) -> bool {
        matches!(self, Self::Cam | Self::Telesync | Self::Telecine | Self::Screener)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VideoCodec {
    Hevc,
    Avc,
    Av1,
    Vp9,
    Xvid,
}

impl VideoCodec {
    pub fn label(self) -> &'static str {
        match self {
            Self::Hevc => "HEVC",
            Self::Avc => "H.264",
            Self::Av1 => "AV1",
            Self::Vp9 => "VP9",
            Self::Xvid => "XviD",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Audio {
    Atmos,
    TrueHd,
    DtsX,
    DtsHdMa,
    Dts,
    Ddp,
    Dd,
    Aac,
    Flac,
    Opus,
}

impl Audio {
    pub fn label(self) -> &'static str {
        match self {
            Self::Atmos => "Atmos",
            Self::TrueHd => "TrueHD",
            Self::DtsX => "DTS:X",
            Self::DtsHdMa => "DTS-HD MA",
            Self::Dts => "DTS",
            Self::Ddp => "DD+",
            Self::Dd => "DD",
            Self::Aac => "AAC",
            Self::Flac => "FLAC",
            Self::Opus => "Opus",
        }
    }
}

/// Everything we can learn from a Torrentio source description.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ReleaseInfo {
    /// First title line, e.g. `Dune.Part.Two.2024.2160p.WEB-DL…-FLUX[TGx]`.
    pub release_name: String,
    /// File inside a season pack, when Torrentio names one.
    pub file_path: Option<String>,
    pub resolution: Option<Resolution>,
    pub source: Option<Source>,
    pub codec: Option<VideoCodec>,
    pub audio: Vec<Audio>,
    /// "7.1", "5.1", "2.0".
    pub channels: Option<String>,
    pub dolby_vision: bool,
    pub hdr10_plus: bool,
    pub hdr10: bool,
    pub hlg: bool,
    pub ten_bit: bool,
    pub three_d: bool,
    pub upscaled: bool,
    pub size_bytes: Option<u64>,
    pub seeders: Option<u32>,
    pub tracker: Option<String>,
    pub group: Option<String>,
    /// Flag emojis and "Multi Audio" / "Dual Audio" from the language line.
    pub languages: Vec<String>,
}

impl ReleaseInfo {
    /// Parse a Torrentio-style `name` ("Torrentio\n4k DV | HDR") and `title`.
    pub fn parse(name: &str, title: &str) -> Self {
        let mut info = ReleaseInfo::default();
        let mut text_lines = Vec::new();
        for line in title.lines().map(str::trim).filter(|l| !l.is_empty()) {
            if line.contains('👤') || line.contains('💾') || line.contains('⚙') {
                parse_stats(line, &mut info);
            } else if is_language_line(line) {
                info.languages = split_languages(line);
            } else {
                text_lines.push(line);
            }
        }
        info.release_name = text_lines.first().copied().unwrap_or_default().to_owned();
        info.file_path = text_lines.get(1).map(|s| (*s).to_owned());
        // Tags from the name line are authoritative for resolution and HDR.
        let name_tags = name.lines().nth(1).unwrap_or_default().to_ascii_lowercase();
        let extra = format!("{} {}", info.file_path.clone().unwrap_or_default(), name_tags);
        info.analyze(&name_tags, &extra);
        info.group = detect_group(&info.release_name);
        info
    }

    /// Parse a Comet stream: `name` like "[RD⚡] Comet 2160p" and a description of
    /// emoji-tagged lines (📄 title, 📹 video, 🔊 audio, ⭐ quality, 🏷️ group,
    /// 👤 seeders 💾 size 🔎 tracker, 🌎 languages).
    pub fn parse_comet(name: &str, description: &str, filename: Option<&str>, video_size: Option<u64>) -> Self {
        let mut info = ReleaseInfo::default();
        let mut tags = Vec::new();
        for line in description.lines().map(str::trim).filter(|l| !l.is_empty()) {
            if let Some(title) = line.strip_prefix('📄') {
                info.release_name = title.trim().to_owned();
            } else if line.contains('👤') || line.contains('💾') || line.contains('🔎') {
                parse_stats(line, &mut info);
            } else if let Some(langs) = line.strip_prefix('🌎') {
                info.languages = split_languages(langs);
            } else if is_language_line(line) {
                info.languages = split_languages(line);
            } else {
                if let Some((_, group)) = line.split_once("🏷️") {
                    let group = group.split('|').next().unwrap_or_default().trim();
                    if !group.is_empty() {
                        info.group = Some(group.to_owned());
                    }
                }
                tags.push(line.replace('•', " ").replace('|', " "));
            }
        }
        if info.release_name.is_empty() {
            info.release_name = filename.unwrap_or_default().to_owned();
        }
        if info.size_bytes.is_none() {
            info.size_bytes = video_size.filter(|s| *s > 0);
        }
        let resolution_tag = name.split_whitespace().last().unwrap_or_default().to_ascii_lowercase();
        let extra = format!("{} {} {resolution_tag}", filename.unwrap_or_default(), tags.join(" "));
        info.analyze(&resolution_tag, &extra);
        if info.group.is_none() {
            info.group = detect_group(&info.release_name);
        }
        info
    }

    /// Fill the technical fields from the release name plus `extra` tag text.
    fn analyze(&mut self, resolution_hint: &str, extra: &str) {
        let haystack = format!("{} {extra}", self.release_name);
        let lower = haystack.to_ascii_lowercase();
        let tokens = tokenize(&haystack);
        let has = |needle: &str| tokens.iter().any(|t| t == needle);
        let any = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));

        self.resolution = detect_resolution(&tokens, resolution_hint);
        self.source = detect_source(&tokens, &lower);
        self.codec = if has("hevc") || any(&["x265", "h265", "h.265", "h 265"]) {
            Some(VideoCodec::Hevc)
        } else if has("av1") {
            Some(VideoCodec::Av1)
        } else if has("avc") || any(&["x264", "h264", "h.264", "h 264"]) {
            Some(VideoCodec::Avc)
        } else if has("vp9") {
            Some(VideoCodec::Vp9)
        } else if has("xvid") || has("divx") {
            Some(VideoCodec::Xvid)
        } else {
            None
        };
        self.dolby_vision = has("dv") || has("dovi") || lower.contains("dolby vision");
        self.hdr10_plus = has("hdr10+") || has("hdr10plus");
        self.hdr10 = self.hdr10_plus || has("hdr") || has("hdr10");
        self.hlg = has("hlg");
        self.ten_bit = has("10bit") || has("10-bit");
        self.three_d = has("3d") || has("hsbs") || has("sbs") || has("hou");
        self.upscaled = has("upscaled") || has("upscale") || has("upscaling");
        self.audio = detect_audio(&tokens);
        self.channels = ["7.1", "5.1", "2.0"]
            .into_iter()
            .find(|c| haystack.contains(c))
            .map(str::to_owned);
    }

    /// Short badges for the source list, most important first.
    pub fn badges(&self) -> Vec<String> {
        let mut badges = Vec::new();
        if self.dolby_vision {
            badges.push("Dolby Vision".to_owned());
        }
        if self.hdr10_plus {
            badges.push("HDR10+".to_owned());
        } else if self.hdr10 {
            badges.push("HDR".to_owned());
        } else if self.hlg {
            badges.push("HLG".to_owned());
        }
        if let Some(source) = self.source {
            badges.push(source.label().to_owned());
        }
        if let Some(codec) = self.codec {
            badges.push(codec.label().to_owned());
        }
        for audio in &self.audio {
            badges.push(audio.label().to_owned());
        }
        if let Some(channels) = &self.channels {
            badges.push(channels.clone());
        }
        if self.upscaled {
            badges.push("Upscaled".to_owned());
        }
        if self.three_d {
            badges.push("3D".to_owned());
        }
        badges
    }

    pub fn is_junk(&self) -> bool {
        self.source.is_some_and(Source::is_junk)
    }
}

/// What the screen can show. Desktop builds report everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayCaps {
    /// Tallest resolution the display mode list supports (1080, 2160…).
    pub max_height: u32,
    pub hdr10: bool,
    pub hdr10_plus: bool,
    pub dolby_vision: bool,
    pub hlg: bool,
}

impl DisplayCaps {
    pub const EVERYTHING: Self = Self {
        max_height: 2160,
        hdr10: true,
        hdr10_plus: true,
        dolby_vision: true,
        hlg: true,
    };

    pub const SDR_1080: Self = Self {
        max_height: 1080,
        hdr10: false,
        hdr10_plus: false,
        dolby_vision: false,
        hlg: false,
    };
}

impl Default for DisplayCaps {
    fn default() -> Self {
        Self::EVERYTHING
    }
}

/// User-facing source filters.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamFilters {
    /// Hide CAM, telesync, telecine and screener copies.
    pub hide_junk: bool,
    /// Hide resolutions and HDR formats the TV can't display.
    pub match_display: bool,
    /// Highest resolution to show, regardless of the display (`None` = no cap).
    pub max_resolution: Option<Resolution>,
    /// Largest file in GB (`None` = no cap).
    pub max_size_gb: Option<f32>,
}

impl Default for StreamFilters {
    fn default() -> Self {
        Self {
            hide_junk: true,
            match_display: true,
            max_resolution: None,
            max_size_gb: None,
        }
    }
}

/// Why a source was hidden (used for the "N hidden" hint).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hidden {
    Junk,
    TooSharpForDisplay,
    HdrUnsupported,
    AboveMaxResolution,
    TooLarge,
}

/// Decide whether a source passes the filters.
pub fn check(info: &ReleaseInfo, filters: &StreamFilters, caps: &DisplayCaps) -> Result<(), Hidden> {
    if filters.hide_junk && info.is_junk() {
        return Err(Hidden::Junk);
    }
    let height = info.resolution.and_then(Resolution::height);
    if let (Some(max), Some(height)) = (filters.max_resolution.and_then(Resolution::height), height)
        && height > max
    {
        return Err(Hidden::AboveMaxResolution);
    }
    if let (Some(max_gb), Some(size)) = (filters.max_size_gb, info.size_bytes)
        && max_gb > 0.0
        && size as f64 > f64::from(max_gb) * 1e9
    {
        return Err(Hidden::TooLarge);
    }
    if filters.match_display {
        if let Some(height) = height
            && height > caps.max_height
        {
            return Err(Hidden::TooSharpForDisplay);
        }
        if !hdr_playable(info, caps) {
            return Err(Hidden::HdrUnsupported);
        }
    }
    Ok(())
}

/// HDR formats need display support; Dolby Vision files with an HDR10 base layer fall back to HDR10.
fn hdr_playable(info: &ReleaseInfo, caps: &DisplayCaps) -> bool {
    if info.dolby_vision && !caps.dolby_vision {
        // Profile 5 (DV only) shows wrong colours without DV; with an HDR10 layer it plays as HDR10.
        return info.hdr10 && caps.hdr10;
    }
    if (info.hdr10 || info.hdr10_plus) && !info.dolby_vision && !caps.hdr10 {
        return false;
    }
    if info.hlg && !(caps.hlg || caps.hdr10) {
        return false;
    }
    true
}

/// Ranking score: higher is better.
pub fn score(info: &ReleaseInfo, caps: &DisplayCaps) -> i64 {
    let mut score: i64 = match info.resolution {
        Some(Resolution::R2160) => 400,
        Some(Resolution::R1440) => 340,
        Some(Resolution::R1080) => 300,
        Some(Resolution::R720) => 200,
        Some(Resolution::R576) | Some(Resolution::R480) => 100,
        Some(Resolution::Unknown) | None => 150,
    };
    score += match info.source {
        Some(Source::Remux) => 60,
        Some(Source::BluRay) => 50,
        Some(Source::WebDl) => 45,
        Some(Source::WebRip) => 35,
        Some(Source::Hdtv) => 10,
        Some(Source::Dvd) => 0,
        Some(_) => -1000,
        None => 20,
    };
    if info.dolby_vision && caps.dolby_vision {
        score += 25;
    } else if info.hdr10 && caps.hdr10 {
        score += 20;
    }
    if info.audio.iter().any(|a| matches!(a, Audio::Atmos | Audio::TrueHd | Audio::DtsX)) {
        score += 10;
    } else if info.audio.contains(&Audio::DtsHdMa) {
        score += 8;
    } else if info.audio.contains(&Audio::Ddp) {
        score += 6;
    }
    if info.upscaled {
        score -= 80;
    }
    if info.three_d {
        score -= 200;
    }
    if info.codec == Some(VideoCodec::Xvid) {
        score -= 40;
    }
    let seeders = info.seeders.unwrap_or(0);
    score += (f64::from(seeders + 1).log2() * 8.0).min(80.0) as i64;
    if seeders == 0 {
        score -= 60;
    }
    score
}

// ---- helpers ----

fn tokenize(text: &str) -> Vec<String> {
    text.to_ascii_lowercase()
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '+' || c == '-'))
        .flat_map(|t| {
            // Keep "web-dl"/"dts-hd" whole but also split hyphenated groups.
            let mut parts = vec![t.to_owned()];
            if t.contains('-') {
                parts.extend(t.split('-').map(str::to_owned));
            }
            parts
        })
        .filter(|t| !t.is_empty())
        .collect()
}

fn detect_resolution(tokens: &[String], name_tags: &str) -> Option<Resolution> {
    let tag = name_tags.split_whitespace().next().unwrap_or_default();
    let from = |t: &str| match t {
        "4k" | "2160p" | "uhd" => Some(Resolution::R2160),
        "1440p" => Some(Resolution::R1440),
        "1080p" | "1080i" => Some(Resolution::R1080),
        "720p" => Some(Resolution::R720),
        "576p" => Some(Resolution::R576),
        "480p" => Some(Resolution::R480),
        _ => None,
    };
    from(tag).or_else(|| tokens.iter().find_map(|t| from(t)))
}

fn detect_source(tokens: &[String], lower: &str) -> Option<Source> {
    let has = |needle: &str| tokens.iter().any(|t| t == needle);
    if has("remux") || lower.contains("bdremux") {
        Some(Source::Remux)
    } else if has("cam") || has("camrip") || has("hdcam") {
        Some(Source::Cam)
    } else if has("ts") || has("telesync") || has("hdts") || has("tsrip") {
        Some(Source::Telesync)
    } else if has("tc") || has("telecine") || has("hdtc") {
        Some(Source::Telecine)
    } else if has("scr") || has("screener") || has("dvdscr") || has("bdscr") {
        Some(Source::Screener)
    } else if has("bluray") || has("blu-ray") || has("bdrip") || has("brrip") || has("bd") {
        Some(Source::BluRay)
    } else if has("web-dl") || has("webdl") || (has("web") && has("dl")) {
        Some(Source::WebDl)
    } else if has("webrip") || has("web") {
        Some(Source::WebRip)
    } else if has("hdtv") || has("pdtv") {
        Some(Source::Hdtv)
    } else if has("dvd") || has("dvdrip") || has("dvd5") || has("dvd9") {
        Some(Source::Dvd)
    } else {
        None
    }
}

fn detect_audio(tokens: &[String]) -> Vec<Audio> {
    let has = |needle: &str| tokens.iter().any(|t| t == needle);
    let starts = |prefix: &str| tokens.iter().any(|t| t.starts_with(prefix));
    let mut audio = Vec::new();
    if has("atmos") {
        audio.push(Audio::Atmos);
    }
    if has("truehd") {
        audio.push(Audio::TrueHd);
    }
    if has("dts-x") || has("dtsx") {
        audio.push(Audio::DtsX);
    } else if has("dts-hd") || has("dtshd") || has("dts-hdma") {
        audio.push(Audio::DtsHdMa);
    } else if starts("dts") {
        audio.push(Audio::Dts);
    }
    if has("ddp") || has("eac3") || has("dd+") || starts("ddp") {
        audio.push(Audio::Ddp);
    } else if has("dd") || has("ac3") || starts("dd5") {
        audio.push(Audio::Dd);
    }
    if starts("aac") {
        audio.push(Audio::Aac);
    }
    if has("flac") {
        audio.push(Audio::Flac);
    }
    if has("opus") {
        audio.push(Audio::Opus);
    }
    audio
}

fn detect_group(release_name: &str) -> Option<String> {
    let name = release_name
        .trim_end_matches(|c: char| c == ']' || c == ')')
        .trim();
    // Strip trailing site tags like "[TGx]" and extensions.
    let name = name.split('[').next().unwrap_or(name).trim();
    let name = name
        .rsplit_once('.')
        .filter(|(_, ext)| matches!(ext.to_ascii_lowercase().as_str(), "mkv" | "mp4" | "avi"))
        .map_or(name, |(stem, _)| stem);
    let (_, group) = name.rsplit_once('-')?;
    let group = group.trim();
    (!group.is_empty()
        && group.len() <= 24
        && group.chars().all(|c| c.is_alphanumeric() || c == '_'))
    .then(|| group.to_owned())
}

fn parse_stats(line: &str, info: &mut ReleaseInfo) {
    let after = |marker: char| line.split(marker).nth(1).map(str::trim);
    if let Some(rest) = after('👤') {
        info.seeders = rest.split_whitespace().next().and_then(|n| n.parse().ok());
    }
    if let Some(rest) = after('💾') {
        let mut parts = rest.split_whitespace();
        if let (Some(number), Some(unit)) = (parts.next(), parts.next()) {
            info.size_bytes = parse_size(number, unit);
        }
    }
    if let Some(rest) = line.split('⚙').nth(1).or_else(|| line.split('🔎').nth(1)) {
        let tracker = rest.trim_start_matches('\u{fe0f}').trim();
        if !tracker.is_empty() {
            info.tracker = Some(tracker.to_owned());
        }
    }
}

fn parse_size(number: &str, unit: &str) -> Option<u64> {
    let value: f64 = number.replace(',', "").parse().ok()?;
    let multiplier = match unit.to_ascii_uppercase().as_str() {
        "TB" => 1e12,
        "GB" => 1e9,
        "MB" => 1e6,
        "KB" => 1e3,
        _ => return None,
    };
    Some((value * multiplier) as u64)
}

fn split_languages(line: &str) -> Vec<String> {
    line.split('/')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn is_language_line(line: &str) -> bool {
    let has_flag = line.chars().any(|c| ('\u{1F1E6}'..='\u{1F1FF}').contains(&c));
    has_flag || line.starts_with("Multi Audio") || line.starts_with("Dual Audio") || line.starts_with("Multi Subs")
}

/// Human-readable size: "29.3 GB", "350 MB".
pub fn format_size(bytes: u64) -> String {
    let gb = bytes as f64 / 1e9;
    if gb >= 1.0 {
        format!("{gb:.1} GB")
    } else {
        format!("{:.0} MB", bytes as f64 / 1e6)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUNE_NAME: &str = "Torrentio\n4k DV | HDR";
    const DUNE_TITLE: &str = "Dune.Part.Two.2024.2160p.WEB-DL.DDP5.1.Atmos.DV.HDR.H.265-FLUX[TGx]\n👤 568 💾 29.26 GB ⚙️ TorrentGalaxy";

    #[test]
    fn parses_a_4k_web_release() {
        let info = ReleaseInfo::parse(DUNE_NAME, DUNE_TITLE);
        assert_eq!(info.resolution, Some(Resolution::R2160));
        assert_eq!(info.source, Some(Source::WebDl));
        assert_eq!(info.codec, Some(VideoCodec::Hevc));
        assert!(info.dolby_vision && info.hdr10 && !info.hdr10_plus);
        assert_eq!(info.audio, vec![Audio::Atmos, Audio::Ddp]);
        assert_eq!(info.channels.as_deref(), Some("5.1"));
        assert_eq!(info.seeders, Some(568));
        assert_eq!(info.size_bytes, Some(29_260_000_000));
        assert_eq!(info.tracker.as_deref(), Some("TorrentGalaxy"));
        assert_eq!(info.group.as_deref(), Some("FLUX"));
        assert_eq!(info.badges()[..3], ["Dolby Vision", "HDR", "WEB-DL"]);
    }

    #[test]
    fn parses_pack_file_and_languages() {
        let info = ReleaseInfo::parse(
            "Torrentio\n1080p",
            "Breaking Bad Season 1 2 3 & 4 + Extras\nSeason 3/Breaking Bad Season 3 Episode 04 - Green Light.mkv\n👤 8 💾 349.76 MB ⚙️ MagnetDL\nMulti Audio / 🇬🇧 / 🇷🇺",
        );
        assert_eq!(info.release_name, "Breaking Bad Season 1 2 3 & 4 + Extras");
        assert!(info.file_path.unwrap().ends_with("Green Light.mkv"));
        assert_eq!(info.resolution, Some(Resolution::R1080));
        assert_eq!(info.size_bytes, Some(349_760_000));
        assert_eq!(info.languages, vec!["Multi Audio", "🇬🇧", "🇷🇺"]);
    }

    #[test]
    fn parses_comet_descriptions() {
        let info = ReleaseInfo::parse_comet(
            "[RD⚡] Comet 2160p",
            "📄 Dune Part Two 2024 Hybrid 2160p UHD Blu-ray Remux DoVi HDR10+ HEVC TrueHD 7.1 Atmos.mkv\n📹 hevc • DV • HDR10+ • 10bit | 🔊 Atmos • TrueHD • 7.1\n⭐ BluRay REMUX | 🏷️ seedpool\n👤 42 💾 64.3 GB 🔎 StremThru\n🌎 English/French",
            Some("Dune.mkv"),
            Some(69_054_203_359),
        );
        assert_eq!(info.resolution, Some(Resolution::R2160));
        assert_eq!(info.source, Some(Source::Remux));
        assert_eq!(info.codec, Some(VideoCodec::Hevc));
        assert!(info.dolby_vision && info.hdr10_plus && info.hdr10 && info.ten_bit);
        assert_eq!(info.audio, vec![Audio::Atmos, Audio::TrueHd]);
        assert_eq!(info.group.as_deref(), Some("seedpool"));
        assert_eq!(info.seeders, Some(42));
        assert_eq!(info.size_bytes, Some(64_300_000_000));
        assert_eq!(info.tracker.as_deref(), Some("StremThru"));
        assert_eq!(info.languages, vec!["English", "French"]);

        let bare = ReleaseInfo::parse_comet("[RD⬇️] Comet 1080p", "", Some("Show.S01E02.1080p.WEB-DL.x264-NTb.mkv"), Some(2_000_000_000));
        assert_eq!(bare.resolution, Some(Resolution::R1080));
        assert_eq!(bare.group.as_deref(), Some("NTb"));
        assert_eq!(bare.size_bytes, Some(2_000_000_000));
    }

    #[test]
    fn detects_junk_and_remux() {
        let cam = ReleaseInfo::parse("Torrentio\nCAM", "Movie.2026.HDCAM.x264-AAA\n👤 900 💾 1.2 GB ⚙️ X");
        assert!(cam.is_junk());
        let remux = ReleaseInfo::parse(
            "Torrentio\n4k DV",
            "Dune.Part.Two.2024.UHD.BluRay.2160p.TrueHD.Atmos.7.1.DV.HEVC.REMUX-FraMeSToR\n👤 329 💾 64.29 GB ⚙️ ThePirateBay",
        );
        assert_eq!(remux.source, Some(Source::Remux));
        assert!(remux.dolby_vision && !remux.hdr10);
        assert_eq!(remux.group.as_deref(), Some("FraMeSToR"));
        assert_eq!(remux.channels.as_deref(), Some("7.1"));
    }

    #[test]
    fn filters_by_display() {
        let dune = ReleaseInfo::parse(DUNE_NAME, DUNE_TITLE);
        let filters = StreamFilters::default();
        assert_eq!(check(&dune, &filters, &DisplayCaps::SDR_1080), Err(Hidden::TooSharpForDisplay));
        let hdr_4k = DisplayCaps { max_height: 2160, hdr10: true, ..DisplayCaps::SDR_1080 };
        // DV with an HDR10 base plays as HDR10.
        assert_eq!(check(&dune, &filters, &hdr_4k), Ok(()));
        let dv_only = ReleaseInfo::parse("Torrentio\n4k DV", "X.2160p.DV.HEVC-G\n👤 3 💾 9 GB ⚙️ T");
        assert_eq!(check(&dv_only, &filters, &hdr_4k), Err(Hidden::HdrUnsupported));
        let sdr_4k = DisplayCaps { max_height: 2160, ..DisplayCaps::SDR_1080 };
        assert_eq!(check(&dune, &filters, &sdr_4k), Err(Hidden::HdrUnsupported));
        let off = StreamFilters { match_display: false, ..filters };
        assert_eq!(check(&dune, &off, &DisplayCaps::SDR_1080), Ok(()));
        let capped = StreamFilters { max_size_gb: Some(20.0), ..off };
        assert_eq!(check(&dune, &capped, &DisplayCaps::SDR_1080), Err(Hidden::TooLarge));
    }

    #[test]
    fn ranks_quality_then_health() {
        let caps = DisplayCaps::EVERYTHING;
        let dune = ReleaseInfo::parse(DUNE_NAME, DUNE_TITLE);
        let hd = ReleaseInfo::parse("Torrentio\n1080p", "Dune.Part.Two.2024.1080p.WEB-DL.x264-AAA\n👤 2000 💾 5 GB ⚙️ X");
        let cam = ReleaseInfo::parse("Torrentio\nCAM", "Dune.Part.Two.2024.CAM-AAA\n👤 5000 💾 1 GB ⚙️ X");
        let dead = ReleaseInfo::parse("Torrentio\n4k", "Dune.Part.Two.2024.2160p.WEB-DL-BBB\n👤 0 💾 20 GB ⚙️ X");
        assert!(score(&dune, &caps) > score(&hd, &caps));
        assert!(score(&hd, &caps) > score(&cam, &caps));
        assert!(score(&dune, &caps) > score(&dead, &caps));
        assert_eq!(format_size(29_260_000_000), "29.3 GB");
        assert_eq!(format_size(349_760_000), "350 MB");
    }
}
