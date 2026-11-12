<img src="assets/icon.svg" width="64" height="64" alt="">

# Joshflix

A native Android TV app for movies and shows, written entirely in Rust. It takes the shape of the Stremio TV app (catalogs, detail pages, a source picker, a full-featured player) with its own design and code, and streams through Real-Debrid like Jedflix.

- **UI:** [Slint](https://slint.dev) 1.18 with its femtovg OpenGL renderer, D-pad first. On Android it runs on our own small platform layer (`src/android_platform.rs`: android-activity, glutin EGL, femtovg) instead of Slint's Skia backend. That keeps 32-bit ARM TVs supported, lets mpv draw into the same GL context, and lets the remote's media keys reach the player.
- **Playback:** libmpv (Stremio's prebuilt Android build: mpv + FFmpeg + libass) with MediaCodec hardware decoding, drawn underneath the UI in the same OpenGL context. Plays MKV, DTS, TrueHD, ASS subtitles and HDR.
  - Every blocking mpv call runs on a control thread, so the UI never waits on the decoder.
  - If a TV's driver stalls on zero-copy decoding, the app continues on a fresh mpv instance in compatibility (copy-back) mode and remembers that choice.
- **Titles and artwork:** Cinemeta (Stremio's keyless metadata service).
- **Sources:** ElfHosted's public Comet with your Real-Debrid key. Cached sources (⚡) start instantly through Comet's redirect. Uncached ones resolve on the TV with live Real-Debrid download progress.
- **Subtitles:** the file's own tracks plus Stremio's keyless OpenSubtitles service.
- **Key pairing and updates:** a small private relay on the dev box's home network (`crates/joshflix-relay`).

## Features

- **Onboarding.** An animated welcome, then the Jedflix-style Real-Debrid step. The TV shows a QR code, your phone opens the relay's page, you paste the key and tap **Send to TV**. You can also type the key with an on-screen keyboard. The key is checked with Real-Debrid before it's saved.
- **Home, Movies, Shows.** A hero for the focused title (backdrop, logo, rating, description) over rows: Continue Watching, My List, Popular, Top Rated, New this year and genre rows.
- **Navigation.** A left rail that widens with labels when focused: Search, Home, Movies, Shows, Discover, My List, Settings.
- **Search.** Our own on-screen keyboard with live movie and show results. A physical keyboard works too.
- **Discover.** A grid filtered by type, sort (Popular, New, Top rated) and genre or year, loading more pages as you scroll.
- **Detail pages.** Play or **Resume 42:10** with **Start over**, My List, cast and director. Shows have season chips and episode cards with thumbnails, progress and watched marks.
- **Source picker.** A Stremio-style list ranked cached-first, then by quality and health, with badges (4K, Dolby Vision, HDR, REMUX, Atmos…), size, seeders and tracker.
  - **Match my TV** hides formats the screen can't show, and junk CAM and screener copies are hidden too. **Show N hidden sources** brings them back.
  - **Resume** reuses the last source automatically.
- **Player.**
  - **Seeking:** ±10 s with the D-pad, ±30 s while held, with a preview before committing.
  - **Tracks:** audio and subtitle menus, with OpenSubtitles results added to the subtitle menu.
  - **Night mode:** dynamic-range compression for clearer dialogue and quieter loud scenes.
  - **Frame-rate matching** on Android 11 and later.
  - **Next episode:** a card in the last minute ("Next Episode", "Season 3 Finale · Up next", "Final Episode") with a 10-second autoplay that picks the same release family.
- **Remote.** Play/pause, rewind, fast-forward, next, captions and audio-track keys work in the player, and the Search key opens Search from anywhere. Back steps back through screens and leaves the app from Home.
- **Settings.** Real-Debrid account (change or remove key), playback toggles (including video decoding mode), subtitle language and size, source filters and About with a self-updater.

## Repository layout

| Path | What it is |
| --- | --- |
| `crates/joshflix-app` | The TV app: Slint UI (`ui/*.slint`) and the Rust controller (`src/app/*`). Builds as `libjoshflix_app.so` for Android and as a `joshflix` binary for desktop development. |
| `crates/joshflix-core` | Domain logic with no UI: Cinemeta, Comet, Real-Debrid, OpenSubtitles, release parsing and ranking, redb storage, relay and update clients, artwork cache. |
| `crates/joshflix-player` | Our own libmpv FFI (loaded at runtime) and the OpenGL render bridge. |
| `crates/joshflix-protocol` | Wire types shared by the app and the relay. |
| `crates/joshflix-relay` | The private pairing and update server ([deploy/README.md](deploy/README.md)). |
| `xtask` | `cargo xtask`: APK builds without Gradle, install and run over adb, publishing updates ([android/README.md](android/README.md)). |
| `android/` | Manifest, resources, `version.toml`. |

## Develop on desktop

The desktop build runs the same UI and player with libmpv from the system (`apt install libmpv-dev`). To get the TV's 960×540 dp canvas, run it at 1920×1080 with scale factor 2:

```sh
SLINT_SCALE_FACTOR=2 cargo run -p joshflix-app
```

Keys: arrows navigate, Enter is OK, Escape is Back, and letters type into Search.

Useful environment variables (desktop only):

| Variable | Effect |
| --- | --- |
| `JOSHFLIX_HOME` | Where data and cache live (default `~/.local/share/joshflix`). |
| `JOSHFLIX_RD_KEY` | Skip onboarding with this Real-Debrid key. |
| `JOSHFLIX_SERVER_URL` | Relay base URL (default `http://192.168.1.67:8788`; also a build-time setting for Android). |
| `JOSHFLIX_COMET_URL` | Point at a different Comet instance, for example a local mock. |
| `JOSHFLIX_DISPLAY=sdr1080` | Pretend the screen is a 1080p SDR TV, to test **Match my TV**. |
| `JOSHFLIX_HWDEC`, `JOSHFLIX_MPV_LOG=v` | Override mpv's hardware decoding and log level. |

On a TV or emulator, put the same `JOSHFLIX_*=value` lines in a `dev.env` file in the app's private data folder (needs `adb root` or `run-as`):

```sh
adb root
adb push dev.env /data/data/com.joshflix.tv/files/dev.env
```

Tests:

```sh
cargo test --workspace
```

## Build and install on a TV

```sh
cargo xtask apk --abi arm64          # or arm / x86_64 / all
cargo xtask run --serial <tv-ip>:5555
```

The first build creates the signing key in `.local/`. **Back it up**: Android only installs updates signed with the same key.

## Ship an update

1. Bump `version_code` and `version_name` in `android/version.toml`.
2. Build the APKs with `cargo xtask apk --abi all`.
3. Publish them with `cargo xtask publish --notes notes.txt`.

TVs check the relay from Settings → About, and automatically on launch. They download the APK, verify its SHA-256, and open Android's installer.

## Privacy and network

- The Real-Debrid key is stored only on the TV (app-private storage). It is sent to Real-Debrid, and to ElfHosted's Comet as part of its config URL, because Comet resolves cached streams for you.
- The relay never stores keys. It holds a key in memory until the TV collects it, then forgets it. It only answers LAN and Tailscale addresses.
- Nothing else leaves the TV except requests to Cinemeta, OpenSubtitles and the artwork CDN.
