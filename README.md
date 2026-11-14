<img src="assets/icon.svg" width="64" height="64" alt="">

# Joshflix

A native Android TV app for movies and shows, written entirely in Rust. It takes the shape of the Stremio TV app (catalogs, detail pages, a source picker, a full-featured player) with its own design and code, and streams through Real-Debrid like Jedflix.

- **UI:** [Slint](https://slint.dev) 1.18 with its femtovg OpenGL renderer, D-pad first. On Android it runs on our own small platform layer (`src/android_platform.rs`: android-activity, glutin EGL, femtovg) instead of Slint's Skia backend. That keeps 32-bit ARM TVs supported and lets the remote's media keys reach the player.
- **Playback:** [Media3 ExoPlayer](https://developer.android.com/media/media3/exoplayer) on the TV, libmpv on desktop and as the TV's fallback.
  - ExoPlayer (`android/java/com/joshflix/tv/ExoBridge.java`, driven from `src/exo.rs`) hands decoded frames from MediaCodec straight to the TV's video layer: no copy and no OpenGL work per frame. The TV's own decoders play Dolby Vision, HDR10 and HDR10+, and Dolby and DTS audio pass through to the TV or a soundbar when it takes them. Audio nothing on the TV can play (TrueHD, DTS on most TVs) decodes with Media3's FFmpeg extension, built against the FFmpeg that ships with libmpv. Subtitles (text, ASS, PGS) draw in a view above the video; OpenSubtitles files are fetched and parsed beside the player, because ExoPlayer can't add a file to a stream that is already playing.
  - mpv takes over by itself, where the video got to, when ExoPlayer can't play a file: no decodable video or audio track, an unseekable file that should resume part-way, a decoder failure, no picture 15 s after loading, an MP4 that keeps its audio far from the video, or a stream ExoPlayer can't open. It keeps that stream, including reopens. Settings → Playback → **Video player** can make mpv play everything.
  - mpv (Stremio's prebuilt Android build: mpv + FFmpeg + libass) uses MediaCodec in copy-back mode. On Android it draws into its own surface underneath a translucent UI surface (`MainActivity`, the same setup as mpv-android), on its own threads. Video never passes through the UI's OpenGL context, so a busy or lost UI surface can't stall or stop it. On desktop, and on a TV where layering fails, mpv draws into the UI's OpenGL context instead (ExoPlayer needs the layers, so such a TV uses mpv).
  - Every blocking mpv call runs on a control thread, and every ExoPlayer call is posted to Android's main thread, so the UI never waits on a decoder.
  - If a TV's driver stalls on mpv's zero-copy decoding, the app continues on a fresh mpv instance in compatibility (copy-back) mode and remembers that choice.
- **Titles and artwork:** Cinemeta (Stremio's keyless metadata service).
- **Sources:** ElfHosted's public Comet with your Real-Debrid key. Cached sources (⚡) start instantly through Comet's redirect. Uncached ones resolve on the TV with live Real-Debrid download progress.
- **Subtitles:** the file's own tracks plus Stremio's keyless OpenSubtitles service.
- **Key pairing, updates and screen colors:** a small private relay on the dev box's home network (`crates/joshflix-relay`).

## Features

Screenshots of the first desktop build are in [docs/screenshots](docs/screenshots).

- **Onboarding.** An animated welcome, then the Jedflix-style Real-Debrid step. The TV shows a QR code, your phone opens the relay's page, you paste the key and tap **Send to TV**. You can also type the key with an on-screen keyboard. The key is checked with Real-Debrid before it's saved.
- **Home, Movies, Shows.** A hero for the focused title (backdrop, logo, rating, description) over rows: Continue Watching, My List, Popular, Top Rated, New this year and genre rows. Rows from the last session show at once while fresh ones load.
- **Navigation.** A left rail that widens with labels when focused: Search, Home, Movies, Shows, Discover, My List, Settings. Back from deep in a screen returns to its top, then to the rail; on Home, Back twice leaves the app.
- **More options.** Hold OK (or press Menu) on a title or episode: Remove from Continue Watching, Mark as watched or unwatched, Mark watched up to here, and My List. Marking moves a show on to its next episode, and unmarking undoes exactly what marking did.
- **Search.** Our own on-screen keyboard with live movie and show results. A physical keyboard works too. Before you type, it shows popular titles and your recent searches (cleared in Settings → About).
- **Discover.** A grid filtered by type, sort (Popular, New, Top rated) and genre or year, loading more pages as you scroll.
- **Detail pages.** Play or **Resume 42:10** with **Start over**, My List, cast and director. Shows have season chips and episode cards with thumbnails, progress and watched marks.
- **Source picker.** A Stremio-style list ranked cached-first, then by quality and health, with badges (4K, Dolby Vision, HDR, REMUX, Atmos…), size, seeders and tracker.
  - **Match my TV** hides formats the screen can't show, and junk CAM and screener copies are hidden too. **Show N hidden sources** brings them back.
  - **Resume** reuses the last source automatically.
  - **Ready ahead:** while a detail page is open, the app fetches its sources (and those of the episode you rest on), so the picker opens filled, and resolves the Resume source's link, so Resume starts the video at once. The next episode's link is resolved while its card shows.
- **Starting playback.** From the moment a stream is chosen until its first frame, one screen shows the title, episode and resume point over the backdrop, with the step it's on (getting the stream, opening it, buffering), Real-Debrid and buffering percentages, and a hint when a file is slow. Errors show there too: OK tries again, Back offers other sources. It dissolves into the video.
- **Player.**
  - **Seeking:** ±10 s with the D-pad, ±30 s while held, with a preview before committing.
  - **Tracks:** audio and subtitle menus, with OpenSubtitles results added to the subtitle menu.
  - **Audio Normalization:** dynamic-range compression for clearer dialogue and quieter loud scenes (mpv's lavfi filters on desktop, Android's DynamicsProcessing effect on the TV).
  - **Frame-rate matching** on Android 11 and later.
  - **Next episode:** a card in the last minute ("Next Episode", "Season 3 Finale · Up next", "Final Episode") that counts down to the end of the episode, then autoplays the same release family. Autoplay waits for the last frame; OK on the card skips the credits. Episodes that haven't aired yet are never offered.
  - **Dropped streams** reopen where they stopped (up to four times, waiting a little longer each time).
  - **Remembered choices:** the audio language and whether subtitles are on carry over to the next episode and title.
- **Remote.** Play/pause, rewind, fast-forward, next, captions and audio-track keys work in the player, and the Search key opens Search from anywhere. Back steps back through screens and leaves the app from Home.
- **House lights.** While a video plays, the lights can follow the colors on screen. About once a second the TV reads a 48×27 copy of the picture (Android's PixelCopy, scaled on the GPU) and posts it to the relay. The relay finds the one color the picture reads as and streams it to the lights app, which paints a group of bulbs that color. The relay sends a new color only after the picture's color has clearly changed and held for 1.5 seconds, so cuts and flashes don't flicker the bulbs. The TV reads the picture only while the lights app listens. Turn it on in the lights app under Automations → Screen colors.
- **Settings.** Real-Debrid account (change or remove key), playback toggles (including the video player and mpv's decoding mode), subtitle language (from a list) and size, source filters and About with a self-updater.

## Repository layout

| Path | What it is |
| --- | --- |
| `crates/joshflix-app` | The TV app: Slint UI (`ui/*.slint`) and the Rust controller (`src/app/*`). Builds as `libjoshflix_app.so` for Android and as a `joshflix` binary for desktop development. |
| `crates/joshflix-core` | Domain logic with no UI: Cinemeta, Comet, Real-Debrid, OpenSubtitles, release parsing and ranking, redb storage, relay and update clients, artwork cache. |
| `crates/joshflix-player` | Our own libmpv FFI (loaded at runtime), the OpenGL render bridge, and attaching mpv to an Android surface. Its `PlayerEvent`s and `Track`s are shared with ExoPlayer's bridge (`crates/joshflix-app/src/exo.rs`). |
| `crates/joshflix-protocol` | Wire types shared by the app and the relay. |
| `crates/joshflix-relay` | The private pairing, update, TV log and screen colors server ([deploy/README.md](deploy/README.md)). |
| `xtask` | `cargo xtask`: APK builds without Gradle, install and run over adb, publishing updates ([android/README.md](android/README.md)). |
| `android/` | Manifest, resources, `version.toml`, the Java activity and ExoPlayer bridge (`java/com/joshflix/tv`), and Media3's FFmpeg audio decoder (`java/androidx/media3/decoder/ffmpeg`, `jni/`). |

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
| `JOSHFLIX_COMET_URL` on a TV or emulator | With `JOSHFLIX_RD_KEY`, test playback against a mock Comet that lists local files (Comet's JSON: a cached `[RD⚡]` name, a 40-hex `bingeGroup` hash, and a playback URL that redirects to the file). |

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

Release from the main checkout, on `main`, with everything committed.

1. Run `git pull --rebase origin main`: another release may have landed.
2. Bump `version_code` and `version_name` in `android/version.toml`, and commit.
3. Build the APKs with `cargo xtask apk --abi all`.
4. Write the release notes to a file outside the checkout, then publish with `cargo xtask publish --notes ~/.cache/joshflix-notes.txt`.

`publish` refuses to run unless the checkout is on `main`, has no uncommitted or untracked files, is not behind `origin/main`, and the APKs are newer than the last commit. It pushes `main` to GitHub before it updates the relay, then tags the release `v<version_name>` and pushes the tag. So every APK a TV can download is a tagged commit on GitHub.

TVs check the relay from Settings → About, and automatically on launch. They download the APK, verify its SHA-256, and open Android's installer.

## Privacy and network

- The Real-Debrid key is stored only on the TV (app-private storage). It is sent to Real-Debrid, and to ElfHosted's Comet as part of its config URL, because Comet resolves cached streams for you.
- The relay never stores keys. It holds a key in memory until the TV collects it, then forgets it. It only answers LAN and Tailscale addresses.
- While a video plays, the TV tells the relay its title and whether it is playing. While the lights app listens, it also sends a 48×27 copy of the picture about once a second. The relay keeps only the color and the title, in memory.
- Nothing else leaves the TV except requests to Cinemeta, OpenSubtitles and the artwork CDN.
