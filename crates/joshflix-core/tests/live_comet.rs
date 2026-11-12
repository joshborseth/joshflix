//! Live check against ElfHosted's Comet with a real Real-Debrid key.
//! Ignored by default; run with
//! `JOSHFLIX_LIVE_RD_KEY_FILE=<file> cargo test -p joshflix-core --test live_comet -- --ignored --nocapture`.

use joshflix_core::VideoRef;
use joshflix_core::comet::Comet;
use joshflix_core::http::Http;
use joshflix_core::release::{DisplayCaps, StreamFilters};
use joshflix_core::sources::RankedSources;

#[tokio::test]
#[ignore = "needs a real Real-Debrid key and network"]
async fn lists_and_resolves_sources() {
    let path = std::env::var("JOSHFLIX_LIVE_RD_KEY_FILE").expect("JOSHFLIX_LIVE_RD_KEY_FILE");
    let key = std::fs::read_to_string(path).unwrap().trim().to_owned();
    let comet = Comet::new(Http::new().unwrap(), &key);
    for video in [
        VideoRef::movie("tt0111161"),
        VideoRef::movie("tt15239678"),
        VideoRef::episode("tt0903747", 1, 1),
    ] {
        let result = comet.streams(&video).await.unwrap();
        assert!(result.notices.is_empty(), "{:?}", result.notices);
        let ranked = RankedSources::rank(result.sources, &StreamFilters::default(), &DisplayCaps::SDR_1080);
        println!(
            "{}: {} shown, {} hidden, first: {:?}",
            video.video_id(),
            ranked.visible.len(),
            ranked.hidden.len(),
            ranked.visible.first().and_then(|s| s.filename.clone())
        );
        assert!(!ranked.visible.is_empty());
    }
    // Pressing play on an instant source: Comet's playback link must redirect to Real-Debrid.
    let result = comet.streams(&VideoRef::movie("tt0111161")).await.unwrap();
    let cached = result.sources.iter().find(|s| s.cached).expect("an instant source");
    let url = comet.resolve_playback(cached.playback_url.as_deref().unwrap()).await.unwrap();
    let host = url.split('/').nth(2).unwrap_or_default().to_owned();
    println!("instant source resolves to host {host}");
    assert!(host.contains("real-debrid") || host.contains("rdeb") || host.contains("download"), "{host}");
}
