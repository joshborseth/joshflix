# Media3 FFmpeg audio decoder (vendored)

Copied unchanged from [androidx/media](https://github.com/androidx/media) tag `1.11.1`,
`libraries/decoder_ffmpeg/src/main/java/androidx/media3/decoder/ffmpeg/`
(Apache License 2.0; each file keeps its header). Media3 doesn't publish this module to Maven.
`ExperimentalFfmpegVideoRenderer.java` is left out: video always decodes in hardware.

Its JNI glue is `android/jni/ffmpeg_jni.cc` from the same tag. `cargo xtask apk` compiles it into
`libffmpegJNI.so` against the FFmpeg that ships with libmpv (see `xtask/src/media3.rs`).
ExoPlayer uses it only for audio formats the TV can neither decode nor pass through, such as
TrueHD or DTS on most TVs.

When updating Media3, copy these files and `ffmpeg_jni.cc` from the matching tag.
