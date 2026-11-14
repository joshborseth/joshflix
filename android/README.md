# Android packaging

Joshflix ships as a `NativeActivity` APK built without Gradle.
`cargo xtask` cross-compiles `joshflix-app`, compiles the Java in `android/java` with `javac`, shrinks it together with Media3 ExoPlayer's jars into `classes.dex` with R8 (from the SDK's `d8.jar`), builds Media3's FFmpeg audio decoder, and packages everything with `aapt2`, `zipalign`, and `apksigner`.

## Prerequisites

- Android SDK at `~/Android/Sdk` (or `ANDROID_HOME`) with `build-tools;35.0.0`, `platforms;android-35`, `platform-tools`, and `ndk;27.2.12479018`.
- Rust targets `armv7-linux-androideabi`, `aarch64-linux-android`, and `x86_64-linux-android`.
- A JDK, 11 or newer (`javac`, `keytool`), `curl`, and `rsvg-convert` (only for `cargo xtask icons`).

## Commands

| Command | What it does |
| --- | --- |
| `cargo xtask fetch-libmpv` | Downloads the pinned libmpv release, checks its SHA-256, and extracts the libraries to `.local/libmpv/1.0.0-stremio02/<abi>/`. `apk` runs this automatically when needed. |
| `cargo xtask fetch-media3` | Downloads Media3 ExoPlayer and its dependencies from Google Maven and Maven Central (each pinned by SHA-256) into `.local/media3/<version>/`, and FFmpeg's headers into `.local/ffmpeg-headers/<version>/`. `apk` runs this automatically when needed. |
| `cargo xtask apk [--abi arm\|arm64\|x86_64\|all] [--debug]` | Builds `dist/joshflix-<versionName>-<abi>.apk` (release by default; `--debug` adds a `-debug` suffix). ABIs build one after another. |
| `cargo xtask install [--serial S] [--debug]` | Runs `adb install -r` with the dist APK for the first ABI in the device's `ro.product.cpu.abilist`. |
| `cargo xtask run [--serial S] [--debug] [--build] [--logcat]` | Installs and launches `com.joshflix.tv/.MainActivity`. `--build` builds the device's ABI first; `--logcat` follows the app's log afterwards. |
| `cargo xtask logcat [--serial S]` | Follows `adb logcat --pid` for the running app. |
| `cargo xtask publish [--notes FILE]` | Copies the three release APKs to `$JOSHFLIX_RELAY_DATA/updates/` (default `~/.local/share/joshflix-relay/updates/`) and writes `manifest.json` atomically. |
| `cargo xtask icons` | Rasterizes `assets/icon.svg`, `assets/icon-foreground.svg`, and `assets/banner.svg` into `android/res/` and `assets/icon-512.png`. |

## Releasing an update

1. Increase `version_code` and change `version_name` in `android/version.toml`.
2. Run `cargo xtask apk --abi all`.
3. Run `cargo xtask publish --notes notes.txt`.

`publish` refuses to run when `version_code` is not higher than the published one, or when `version_name` is unchanged (APK file names contain only the name).
It also checks each APK's package name and version with `aapt2 dump badging`.

## Signing key

The first `apk` run creates `.local/joshflix-release.jks` (PKCS12, alias `joshflix`, RSA 4096) and `.local/keystore.env`, which holds the random password.
Both files are gitignored and readable only by you.
**Back up both files together.**
Android only installs an update signed with the same key, so a lost key means uninstalling the app on every TV.
Debug and release builds use the same key, so you can install either over the other.

APKs are signed with APK Signature Scheme v2 and v3 only, because `minSdk` is 26.

## What an APK contains

```
AndroidManifest.xml                 binary XML, versionCode/versionName injected from version.toml
resources.arsc, res/...             theme, launcher icons, adaptive icon, TV banner
classes.dex                         the activity, the ExoPlayer bridge, Media3 and Guava (R8, about 1.4 MB)
lib/<abi>/libjoshflix_app.so        the app (stored, 16 KB aligned)
lib/<abi>/libmpv.so                 libmpv and its dependencies (stored, 16 KB aligned)
lib/<abi>/libavcodec.so ... libswscale.so
lib/<abi>/libffmpegJNI.so           Media3's FFmpeg audio decoder glue, linked against libavcodec.so
lib/<abi>/libc++_shared.so
```

There is no `META-INF/` JAR signature: the v2/v3 signatures live in the APK signing block.

Libraries are stored uncompressed and `zipalign -P 16` aligns them to 16 KB pages, so Android maps them directly from the APK (`extractNativeLibs="false"`).
The xtask links the app with `-z max-page-size=16384`, because NDK r27 still defaults to 4 KB.
A release arm64 APK is about 27 MB, almost all of it FFmpeg, mpv and Media3.

## Media3 ExoPlayer

The TV plays video with Media3 ExoPlayer 1.11.1 (`com.joshflix.tv.ExoBridge`). `xtask/src/media3.rs` pins every jar it needs: `media3-{exoplayer,common,container,database,datasource,decoder,extractor}`, `exifinterface`, Guava and `failureaccess` go into the APK. `androidx.annotation`, `annotation-experimental`, `checker-qual` and `kotlin-stdlib` (for Kotlin's annotation classes) are only on the compile classpath. R8 shrinks everything with each AAR's consumer rules and `android/proguard-rules.pro`, without obfuscation, so stack traces in the TV log stay readable.

Media3 doesn't publish its FFmpeg extension. Its Java sources (`android/java/androidx/media3/decoder/ffmpeg/`) and JNI glue (`android/jni/ffmpeg_jni.cc`) are copied from the `1.11.1` tag. `apk` compiles the glue per ABI with the NDK's clang++ against FFmpeg 9.0.1's headers, which match libmpv's libavcodec 63.1.101, and links it to libmpv's `libavcodec.so`, `libswresample.so` and `libavutil.so`. ExoPlayer only uses it for audio nothing else on the TV plays (`DefaultRenderersFactory.EXTENSION_RENDERER_MODE_ON`).

## libmpv

Source: <https://github.com/Stremio/libmpv-android/releases/tag/1.0.0-stremio02>.
`libmpv.tar.gz` is a local Maven repository holding one AAR, `libmpv/1.0.0-stremio02/libmpv-1.0.0-stremio02.aar`.
The xtask pins the SHA-256 of both the tarball and the AAR (the AAR hash matches the Gradle module metadata in the tarball).

The AAR has `jni/<abi>/` for `arm64-v8a`, `armeabi-v7a`, `x86`, and `x86_64`. We extract `arm64-v8a`, `armeabi-v7a`, and `x86_64`, and skip `x86`.

| Library | Notes |
| --- | --- |
| `libmpv.so` | mpv v0.41.0. Enabled features include libplacebo, `gpu`/`gpu-next`, `egl-android`, `mediacodec` hwdec, AAudio, AudioTrack, OpenSL ES, libass, Lua |
| `libavcodec.so`, `libavformat.so`, `libavutil.so`, `libavfilter.so`, `libavdevice.so`, `libswresample.so`, `libswscale.so` | FFmpeg as separate shared libraries (symbol version `LIBAVCODEC_63`). `libavcodec` links `libmediandk.so` for MediaCodec. |
| `libc++_shared.so` | The NDK C++ runtime that libmpv needs. Any C++ in our own crates must link against `c++_shared` too, so only one copy exists. |
| `libplayer.so` | **Not shipped.** JNI glue for the AAR's Kotlin class `dev.jdtech.mpv.MPVLib`, which we don't use: the player crate calls libmpv directly. |

`libmpv.so` needs (`DT_NEEDED`): all seven FFmpeg libraries, `libc++_shared.so`, and the system libraries `libandroid`, `libOpenSLES`, `libEGL`, `libz`, `libm`, `libdl`, `libc`.
The 64-bit libraries are aligned to 16 KB. On `armeabi-v7a`, `libc++_shared.so` is aligned to 4 KB, which is fine because 32-bit processes always use 4 KB pages.

### Exported symbols the app uses

- `libmpv.so` exports the full client API, including the render API: `mpv_render_context_create`, `mpv_render_context_render`, `mpv_render_context_set_update_callback`, `mpv_render_context_report_swap`, `mpv_render_context_free`, and so on.
- `libavcodec.so` (not `libmpv.so`) exports `av_jni_set_java_vm`, `av_jni_get_java_vm`, `av_jni_set_android_app_ctx`, and `av_jni_get_android_app_ctx`.

The AAR has no C headers. The player crate declares the mpv and `av_jni_*` functions itself.

## What the app crate must provide

- A `cdylib` named `joshflix_app` (file `libjoshflix_app.so`). The manifest's `android.app.lib_name` points at it.
- `android_main(app: AndroidApp)` from `android-activity` 0.6 with the `native-activity` feature, which exports `ANativeActivity_onCreate`. `cargo xtask apk` fails if the library does not export that symbol.
- Before hardware decoding (`hwdec=mediacodec` or `mediacodec-copy`), call `av_jni_set_java_vm(vm, NULL)` from `libavcodec.so` with the process's `JavaVM*` (`AndroidApp::vm_as_ptr()`).
  Also call `av_jni_set_android_app_ctx(activity_or_context, NULL)` if a codec needs the app context.
  mpv's AudioTrack output reads the same JavaVM through FFmpeg.
- `minSdk` is 26 (Android 8.0). The NDK clang wrappers are `<triple>26-clang`.
- `com.joshflix.tv.ExoBridge` runs ExoPlayer on Android's main thread for the native side (`src/exo.rs`): every call is posted there, and events come back through `nativeEvent` and `nativeLog`, which the native side registers. `MainActivity` gives it its own video `SurfaceView` (resized to the video's aspect ratio, so frames aren't stretched), and a `SubtitleOverlay` view, added last so that no `SurfaceView` clears it. `files/exo-attempt` and `files/exo-strikes` count processes that died while ExoPlayer started a video; after two, `files/exo-off` holds the version code and that version plays with mpv.
- `com.joshflix.tv.MainActivity` (`android/java`) is a `NativeActivity` subclass that stacks `SurfaceView`s: the video surfaces at the bottom (ExoPlayer's, and mpv's, only one shown at a time: `showVideoLayer`) and a translucent UI surface on top, which it hands to the native side in place of the window's surface. The native side reaches it through the activity's class loader (`src/video_surface.rs`): it registers `nativeVideoSurface`, attaches mpv to that surface while the player is open, and confirms the first UI frame.
  That version of the app then uses a single surface for good (`files/single-surface` holds its version code) when:
  - no UI frame is confirmed within 15 s (the activity also closes, so reopening works);
  - two processes died before the UI's first frame, or while mpv was starting on the video surface before video had played for 3 s (`files/ui-attempt` and `files/video-surface-attempt` hold `versionCode pid` and are left behind; `files/*-strikes` count them);
  - playback runs 8 s, while the video surface exists, with a video track but no picture on mpv's surface, and then video does show when drawn into the UI. "Picture" means `video-out-params/w` equals the decoder's `video-params/w`: with `force-window` or `vo=null`, the output reports a dummy 960 px frame. The app switches to drawing video into the UI right away, on a fresh mpv instance; the version is marked only once that shows picture, so a file that can't be decoded doesn't count.

  Settings → Playback → **Separate video layer** shows whether layering is on for the next start. Turning it off writes `files/single-surface-user`, for the case no check can see: video that plays but is hidden. Turning it on also clears an automatic switch-off. Anything else that needs Java APIs, such as installing a downloaded update with `PackageInstaller`, goes through JNI on the `JavaVM`.
- The window background is `#0B0D12` (theme `Theme.Joshflix`), so the first frame should match it.
