# R8 rules for the app's own Java code. Media3's AARs bring their own rules
# (cargo xtask passes them along).

# Readable stack traces in the TV log; the size cost is small.
-dontobfuscate

# The activity, the ExoPlayer bridge and its views are reached from Rust
# through JNI by name.
-keep class com.joshflix.tv.** { *; }

# Native methods keep their names (JNI_OnLoad registers them by name).
-keepclasseswithmembernames,includedescriptorclasses class * {
    native <methods>;
}

# Media3's FFmpeg decoder: ffmpeg_jni.cc finds these classes by name in
# JNI_OnLoad and calls FfmpegAudioDecoder.growOutputBuffer.
-keep class androidx.media3.decoder.ffmpeg.FfmpegAudioDecoder { *; }
-keep class androidx.media3.decoder.ffmpeg.FfmpegLibrary { *; }

# Compile-only annotations that Media3 and Guava reference.
-dontwarn org.checkerframework.**
-dontwarn javax.annotation.**
-dontwarn kotlin.**
-dontwarn com.google.errorprone.annotations.**
-dontwarn com.google.j2objc.annotations.**
-dontwarn androidx.annotation.**
