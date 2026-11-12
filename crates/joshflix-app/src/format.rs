//! Text formatting for the UI.

use joshflix_core::cinemeta::MetaPreview;
use joshflix_core::release;
use joshflix_core::sources::Source;

/// `1:02:03` or `12:03`.
pub fn clock(secs: f64) -> String {
    let total = secs.max(0.0).round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// "42 min left" / "1 h 5 min left".
pub fn time_left(secs: f64) -> String {
    let minutes = (secs.max(0.0) / 60.0).ceil() as u64;
    if minutes >= 60 {
        format!("{} h {} min left", minutes / 60, minutes % 60)
    } else {
        format!("{minutes} min left")
    }
}

/// "105 min" → "1h 45m".
pub fn runtime(raw: &str) -> String {
    let minutes: Option<u64> = raw.split_whitespace().next().and_then(|n| n.parse().ok());
    match minutes {
        Some(m) if m >= 60 => format!("{}h {:02}m", m / 60, m % 60),
        Some(m) => format!("{m}m"),
        None => raw.to_owned(),
    }
}

/// "2024 · 2h 46m · Action, Sci-Fi"
pub fn meta_line(meta: &MetaPreview) -> String {
    let mut parts = Vec::new();
    if let Some(year) = &meta.release_info {
        parts.push(year.clone());
    }
    if let Some(rt) = &meta.runtime {
        parts.push(runtime(rt));
    }
    let genres: Vec<&str> = meta.genres.iter().take(3).map(String::as_str).collect();
    if !genres.is_empty() {
        parts.push(genres.join(", "));
    }
    parts.join("  ·  ")
}

/// "Jan 21, 2008" from an RFC 3339 date (no chrono needed).
pub fn air_date(rfc3339: &str) -> Option<String> {
    let date = rfc3339.get(..10)?;
    let mut parts = date.split('-');
    let (y, m, d) = (parts.next()?, parts.next()?.parse::<usize>().ok()?, parts.next()?.parse::<u32>().ok()?);
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    Some(format!("{} {d}, {y}", MONTHS.get(m.checked_sub(1)?)?))
}

/// Today's date as `YYYY-MM-DD` (UTC).
pub fn today() -> String {
    let days = joshflix_core::store::now() / 86_400_000;
    // Civil-from-days (Howard Hinnant).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn current_year() -> u32 {
    today()[..4].parse().unwrap_or(2026)
}

/// "64.3 GB · 42 seeders · StremThru · English/French"
pub fn source_info(source: &Source) -> String {
    let r = &source.release;
    let mut parts = Vec::new();
    if let Some(size) = r.size_bytes {
        parts.push(release::format_size(size));
    }
    if let Some(seeders) = r.seeders {
        parts.push(format!("{seeders} seeders"));
    }
    if let Some(tracker) = &r.tracker {
        parts.push(tracker.clone());
    }
    if !r.languages.is_empty() {
        parts.push(r.languages.join(" "));
    }
    parts.join("  ·  ")
}

pub fn quality_label(source: &Source) -> String {
    source
        .release
        .resolution
        .map(|r| r.label().to_owned())
        .unwrap_or_else(|| "SD".to_owned())
}

pub fn hidden_label(reason: release::Hidden) -> &'static str {
    match reason {
        release::Hidden::Junk => "CAM / screener",
        release::Hidden::TooSharpForDisplay => "Above TV resolution",
        release::Hidden::HdrUnsupported => "HDR not supported",
        release::Hidden::AboveMaxResolution => "Above max resolution",
        release::Hidden::TooLarge => "Too large",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(clock(3723.0), "1:02:03");
        assert_eq!(clock(63.4), "1:03");
        assert_eq!(time_left(3900.0), "1 h 5 min left");
        assert_eq!(runtime("105 min"), "1h 45m");
        assert_eq!(runtime("49 min"), "49m");
        assert_eq!(air_date("2008-01-21T05:00:00.000Z").unwrap(), "Jan 21, 2008");
        assert_eq!(today().len(), 10);
        assert!(current_year() >= 2026);
    }
}
