//! Audio Normalization on Android, through Android's own audio effects.
//!
//! Stremio's libmpv bundles an FFmpeg built with `--disable-filters`, so the
//! lavfi compressor that desktop uses doesn't exist on the TV. Instead the app
//! generates an audio session, mpv's AudioTrack output plays on it
//! (`audiotrack-session-id`), and an effect attached to that session does the
//! work: `DynamicsProcessing` (Android 9+) with the same curve as desktop, or
//! `LoudnessEnhancer` on Android 8.
//!
//! The effect outlives player instances: a fresh mpv (compatibility mode) plays
//! on the same session, so the effect keeps applying.

use jni::JNIEnv;
use jni::objects::{GlobalRef, JObject, JValue};

/// Compressor and limiter settings. They match desktop's
/// `acompressor=threshold=-24dB:ratio=4:attack=10:release=250:makeup=6dB,alimiter=limit=0.9`.
mod curve {
    pub const THRESHOLD_DB: f32 = -24.0;
    pub const RATIO: f32 = 4.0;
    pub const ATTACK_MS: f32 = 10.0;
    pub const RELEASE_MS: f32 = 250.0;
    pub const KNEE_DB: f32 = 6.0;
    pub const MAKEUP_DB: f32 = 6.0;
    /// Upper edge of the single compressor band: the whole audible range.
    pub const BAND_TOP_HZ: f32 = 20_000.0;
    /// About -0.9 dBFS, desktop's `alimiter=limit=0.9`.
    pub const LIMIT_DB: f32 = -1.0;
    pub const LIMITER_ATTACK_MS: f32 = 1.0;
    pub const LIMITER_RELEASE_MS: f32 = 60.0;
    pub const LIMITER_RATIO: f32 = 10.0;
    /// LoudnessEnhancer's gain in millibels (Android 8 fallback).
    pub const LOUDNESS_GAIN_MB: i32 = 600;
}

/// First API level with `android.media.audiofx.DynamicsProcessing`.
const DYNAMICS_PROCESSING_API: i32 = 28;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectKind {
    DynamicsProcessing,
    LoudnessEnhancer,
}

/// The effect on mpv's audio session. Starts disabled.
pub struct Normalizer {
    effect: GlobalRef,
    kind: EffectKind,
}

impl Normalizer {
    /// Attach the best available effect to `session`.
    pub fn attach(env: &mut JNIEnv, session: i32, sdk_int: i32) -> Result<Self, String> {
        if sdk_int >= DYNAMICS_PROCESSING_API {
            match java(env, |env| dynamics_processing(env, session)) {
                Ok(effect) => return Ok(Self { effect, kind: EffectKind::DynamicsProcessing }),
                Err(e) => tracing::warn!("audio normalization: DynamicsProcessing unavailable ({e}); trying LoudnessEnhancer"),
            }
        }
        java(env, |env| loudness_enhancer(env, session))
            .map(|effect| Self { effect, kind: EffectKind::LoudnessEnhancer })
            .map_err(|e| format!("no audio effect could attach: {e}"))
    }

    pub fn kind(&self) -> EffectKind {
        self.kind
    }

    pub fn set_enabled(&self, env: &mut JNIEnv, on: bool) -> Result<(), String> {
        let status = java(env, |env| env.call_method(&self.effect, "setEnabled", "(Z)I", &[JValue::Bool(on.into())])?.i())?;
        // AudioEffect.SUCCESS = 0.
        if status == 0 { Ok(()) } else { Err(format!("setEnabled({on}) returned {status}")) }
    }
}

/// Ask the system for a fresh audio session id for mpv's AudioTrack.
pub fn generate_session(env: &mut JNIEnv, context: &JObject) -> Result<i32, String> {
    let id = java(env, |env| {
        let name = env.new_string("audio")?; // Context.AUDIO_SERVICE
        let manager = env
            .call_method(context, "getSystemService", "(Ljava/lang/String;)Ljava/lang/Object;", &[JValue::Object(&name)])?
            .l()?;
        env.call_method(&manager, "generateAudioSessionId", "()I", &[])?.i()
    })?;
    // AudioManager.ERROR = -1; 0 would mean "let AudioTrack pick one", which we couldn't attach to.
    if id > 0 { Ok(id) } else { Err(format!("generateAudioSessionId returned {id}")) }
}

/// One-band compressor plus limiter, like desktop's filter chain.
fn dynamics_processing(env: &mut JNIEnv, session: i32) -> jni::errors::Result<GlobalRef> {
    use curve::*;
    const PKG: &str = "android/media/audiofx/DynamicsProcessing";
    let builder = env.new_object(
        format!("{PKG}$Config$Builder"),
        // variant, channelCount, preEqInUse, preEqBandCount, mbcInUse, mbcBandCount,
        // postEqInUse, postEqBandCount, limiterInUse
        "(IIZIZIZIZ)V",
        &[
            JValue::Int(0), // VARIANT_FAVOR_FREQUENCY_RESOLUTION
            JValue::Int(2), // DynamicsProcessing replicates the last channel for wider layouts.
            JValue::Bool(0),
            JValue::Int(1),
            JValue::Bool(1),
            JValue::Int(1),
            JValue::Bool(0),
            JValue::Int(1),
            JValue::Bool(1),
        ],
    )?;
    let config = env.call_method(&builder, "build", format!("()L{PKG}$Config;"), &[])?.l()?;
    let effect = env.new_object(
        PKG,
        format!("(IIL{PKG}$Config;)V"),
        &[JValue::Int(0), JValue::Int(session), JValue::Object(&config)],
    )?;

    let band = env.new_object(
        format!("{PKG}$MbcBand"),
        // enabled, cutoffFrequency, attackTime, releaseTime, ratio, threshold,
        // kneeWidth, noiseGateThreshold, expanderRatio, preGain, postGain
        "(ZFFFFFFFFFF)V",
        &[
            JValue::Bool(1),
            JValue::Float(BAND_TOP_HZ),
            JValue::Float(ATTACK_MS),
            JValue::Float(RELEASE_MS),
            JValue::Float(RATIO),
            JValue::Float(THRESHOLD_DB),
            JValue::Float(KNEE_DB),
            JValue::Float(-90.0), // noise gate off
            JValue::Float(1.0),   // no expansion
            JValue::Float(0.0),
            JValue::Float(MAKEUP_DB),
        ],
    )?;
    env.call_method(&effect, "setMbcBandAllChannelsTo", format!("(IL{PKG}$MbcBand;)V"), &[JValue::Int(0), JValue::Object(&band)])?;

    let limiter = env.new_object(
        format!("{PKG}$Limiter"),
        // inUse, enabled, linkGroup, attackTime, releaseTime, ratio, threshold, postGain
        "(ZZIFFFFF)V",
        &[
            JValue::Bool(1),
            JValue::Bool(1),
            JValue::Int(0),
            JValue::Float(LIMITER_ATTACK_MS),
            JValue::Float(LIMITER_RELEASE_MS),
            JValue::Float(LIMITER_RATIO),
            JValue::Float(LIMIT_DB),
            JValue::Float(0.0),
        ],
    )?;
    env.call_method(&effect, "setLimiterAllChannelsTo", format!("(L{PKG}$Limiter;)V"), &[JValue::Object(&limiter)])?;
    disable(env, &effect)?;
    env.new_global_ref(effect)
}

/// Gain with a built-in limiter: louder dialogue, peaks held down.
fn loudness_enhancer(env: &mut JNIEnv, session: i32) -> jni::errors::Result<GlobalRef> {
    let effect = env.new_object("android/media/audiofx/LoudnessEnhancer", "(I)V", &[JValue::Int(session)])?;
    env.call_method(&effect, "setTargetGain", "(I)V", &[JValue::Int(curve::LOUDNESS_GAIN_MB)])?;
    disable(env, &effect)?;
    env.new_global_ref(effect)
}

fn disable(env: &mut JNIEnv, effect: &JObject) -> jni::errors::Result<()> {
    env.call_method(effect, "setEnabled", "(Z)I", &[JValue::Bool(0)])?;
    Ok(())
}

/// Run JNI calls and turn a pending Java exception into an error message
/// (clearing it, so later JNI calls on this thread still work).
fn java<T>(env: &mut JNIEnv, f: impl FnOnce(&mut JNIEnv) -> jni::errors::Result<T>) -> Result<T, String> {
    let result = env.with_local_frame(32, |env| f(env));
    match result {
        Ok(value) => Ok(value),
        Err(jni::errors::Error::JavaException) => {
            let message = describe_exception(env);
            Err(message)
        }
        Err(e) => Err(e.to_string()),
    }
}

fn describe_exception(env: &mut JNIEnv) -> String {
    let Ok(throwable) = env.exception_occurred() else { return "Java exception".into() };
    let _ = env.exception_clear();
    let text = env
        .call_method(&throwable, "toString", "()Ljava/lang/String;", &[])
        .and_then(|s| s.l())
        .and_then(|s| env.get_string(&s.into()).map(String::from));
    let _ = env.exception_clear();
    text.unwrap_or_else(|_| "Java exception".into())
}
