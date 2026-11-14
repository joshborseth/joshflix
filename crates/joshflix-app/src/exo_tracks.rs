//! The track list `ExoBridge` (Android) sends as JSON, as player tracks.

use joshflix_player::{Track, TrackKind};
use serde::Deserialize;

/// One entry of `ExoBridge`'s track list.
#[derive(Deserialize)]
struct JsonTrack {
    id: i64,
    #[serde(rename = "type")]
    kind: String,
    title: Option<String>,
    lang: Option<String>,
    codec: Option<String>,
    channels: Option<i64>,
    #[serde(default)]
    selected: bool,
    #[serde(default)]
    external: bool,
    #[serde(default)]
    default: bool,
    #[serde(default)]
    forced: bool,
}

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn parse_tracks(json: &str) -> std::result::Result<Vec<Track>, serde_json::Error> {
    let list: Vec<JsonTrack> = serde_json::from_str(json)?;
    Ok(list
        .into_iter()
        .filter_map(|t| {
            let kind = match t.kind.as_str() {
                "video" => TrackKind::Video,
                "audio" => TrackKind::Audio,
                "sub" => TrackKind::Subtitle,
                _ => return None,
            };
            Some(Track {
                id: t.id,
                kind,
                title: t.title,
                lang: t.lang,
                codec: t.codec,
                channels: t.channels.map(joshflix_player::channel_label),
                selected: t.selected,
                external: t.external,
                default: t.default,
                forced: t.forced,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_bridge_track_list() {
        let json = r#"[
            {"id":1000,"type":"video","codec":"hevc","selected":true,"external":false,"default":true,"forced":false},
            {"id":2000,"type":"audio","title":"Surround","lang":"en","codec":"truehd","channels":8,"selected":true,"external":false,"default":true,"forced":false},
            {"id":3001,"type":"sub","lang":"en","codec":"pgs","selected":false,"external":false,"default":false,"forced":true},
            {"id":1000000,"type":"sub","title":"OpenSubtitles 1","lang":"eng","codec":"subrip","selected":true,"external":true,"default":false,"forced":false},
            {"id":5,"type":"image","selected":false}
        ]"#;
        let tracks = parse_tracks(json).unwrap();
        assert_eq!(tracks.len(), 4);
        assert_eq!(tracks[1].label(), "English — Surround · TRUEHD 7.1");
        assert!(tracks[2].forced && tracks[2].kind == TrackKind::Subtitle);
        assert_eq!(tracks[3].label(), "English — OpenSubtitles 1");
        assert!(tracks[3].external && tracks[3].selected);
    }
}
