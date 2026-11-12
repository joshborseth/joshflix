//! Subtitles from Stremio's keyless OpenSubtitles v3 addon.

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::http::Http;
use crate::ids::VideoRef;

const SERVICE: &str = "OpenSubtitles";
pub const DEFAULT_BASE: &str = "https://opensubtitles-v3.strem.io";

/// One downloadable subtitle file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subtitle {
    pub id: String,
    pub url: String,
    /// ISO 639-2 code (`eng`, `spa`…).
    pub lang: String,
}

impl Subtitle {
    pub fn language_name(&self) -> &'static str {
        language_name(&self.lang)
    }
}

/// Languages offered in Settings, as (ISO 639-2 code, English name).
pub const LANGUAGES: &[(&str, &str)] = &[
    ("eng", "English"),
    ("spa", "Spanish"),
    ("fre", "French"),
    ("ger", "German"),
    ("ita", "Italian"),
    ("por", "Portuguese"),
    ("pob", "Portuguese (Brazil)"),
    ("dut", "Dutch"),
    ("swe", "Swedish"),
    ("nor", "Norwegian"),
    ("dan", "Danish"),
    ("fin", "Finnish"),
    ("pol", "Polish"),
    ("rus", "Russian"),
    ("ukr", "Ukrainian"),
    ("tur", "Turkish"),
    ("gre", "Greek"),
    ("heb", "Hebrew"),
    ("ara", "Arabic"),
    ("hin", "Hindi"),
    ("jpn", "Japanese"),
    ("kor", "Korean"),
    ("chi", "Chinese"),
    ("vie", "Vietnamese"),
    ("tha", "Thai"),
    ("ind", "Indonesian"),
];

pub fn language_name(code: &str) -> &'static str {
    LANGUAGES
        .iter()
        .find(|(c, _)| *c == code)
        .map_or("Other", |(_, name)| name)
}

#[derive(Clone)]
pub struct OpenSubtitles {
    http: Http,
    base: String,
}

impl OpenSubtitles {
    pub fn new(http: Http) -> Self {
        Self::with_base(http, DEFAULT_BASE)
    }

    pub fn with_base(http: Http, base: impl Into<String>) -> Self {
        Self {
            http,
            base: base.into().trim_end_matches('/').to_owned(),
        }
    }

    /// Subtitles in `languages` (preference order), at most `per_language` each.
    pub async fn find(&self, video: &VideoRef, languages: &[String], per_language: usize) -> Result<Vec<Subtitle>> {
        let url = format!("{}/subtitles/{}/{}.json", self.base, video.kind.as_str(), video.video_id());
        let raw: RawResponse = self.http.get_json(SERVICE, &url).await?;
        Ok(pick(raw.subtitles, languages, per_language))
    }
}

fn pick(all: Vec<Subtitle>, languages: &[String], per_language: usize) -> Vec<Subtitle> {
    let mut picked = Vec::new();
    for lang in languages {
        picked.extend(all.iter().filter(|s| &s.lang == lang).take(per_language).cloned());
    }
    picked
}

#[derive(Deserialize)]
struct RawResponse {
    #[serde(default)]
    subtitles: Vec<Subtitle>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_preferred_languages_in_order() {
        let sub = |id: &str, lang: &str| Subtitle { id: id.into(), url: format!("https://s/{id}"), lang: lang.into() };
        let all = vec![sub("1", "eng"), sub("2", "spa"), sub("3", "eng"), sub("4", "eng"), sub("5", "fre")];
        let picked = pick(all, &["spa".into(), "eng".into()], 2);
        let ids: Vec<_> = picked.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["2", "1", "3"]);
        assert_eq!(picked[0].language_name(), "Spanish");
    }
}
