//! Fetches Media3 ExoPlayer and the FFmpeg headers its audio extension needs.
//!
//! The TV plays video with ExoPlayer (`android/java/com/joshflix/tv/ExoBridge.java`).
//! Without Gradle, `cargo xtask apk` needs the libraries as plain jars:
//! every Maven artifact below is downloaded once, checked against its pinned
//! SHA-256, and its `classes.jar` (or the jar itself) is stored in
//! `.local/media3/<version>/`, with each AAR's consumer ProGuard rules beside it.
//!
//! Media3 doesn't publish its FFmpeg decoder extension, so its sources live in
//! `android/java/androidx/media3/decoder/ffmpeg` and `android/jni`. Its JNI glue
//! links the FFmpeg that ships with libmpv, and compiles against that release's
//! headers (`.local/ffmpeg-headers/<version>/`).

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

use crate::util::{self, command, sha256_file, workspace_root};

pub const VERSION: &str = "1.11.1";

const GOOGLE: &str = "https://dl.google.com/android/maven2";
const CENTRAL: &str = "https://repo1.maven.org/maven2";

/// How an artifact takes part in the build.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Compiled into the APK (shrunk by R8).
    Program,
    /// Annotations only: needed to compile, never shipped.
    Classpath,
}

struct Artifact {
    repo: &'static str,
    group: &'static str,
    name: &'static str,
    version: &'static str,
    /// `aar` or `jar`.
    kind: &'static str,
    sha256: &'static str,
    role: Role,
}

/// Media3's ExoPlayer and its runtime dependencies (from the POMs of
/// `media3-exoplayer` 1.11.1 and what they pull in).
const ARTIFACTS: &[Artifact] = &[
    media3("exoplayer", "588592baec110837c33ce0c82bb76464203e6b0621177bbe5d88779001a6f1ed"),
    media3("common", "50813cf219c7c93ed8a32a750891ca6d1280729ecf7a90c49567adacd67ef31b"),
    media3("container", "930fca2f018f06f1d08c12599f8c97f31eddbd728edee5de06eb53a882f8fc54"),
    media3("database", "520e6f814e42a7d428db187ac073b453c0fcba0f4f5f4edd1cf33fafd2cfb740"),
    media3("datasource", "490fd23c7562a6a1183d63bf854b291ae3a77dce5a151450aa1937148c80c60b"),
    media3("decoder", "c3bcd704959026e0d644b0260f9e682a2e26e7357c2cf82d990bcb323f800d49"),
    media3("extractor", "6b2d58d23a0c53425249d8b7c945600019d61a8b404c8fb8cd9ca860c5738508"),
    Artifact {
        repo: GOOGLE,
        group: "androidx/exifinterface",
        name: "exifinterface",
        version: "1.3.6",
        kind: "aar",
        sha256: "1804105e9e05fdd8f760413bad5de498c381aa329f4f9d94c851bc891ac654c6",
        role: Role::Program,
    },
    Artifact {
        repo: CENTRAL,
        group: "com/google/guava",
        name: "guava",
        version: "33.3.1-android",
        kind: "jar",
        sha256: "2c3e41d1b380f2044d257947a3aa82dabf3ae4b978622745254aa18b6cf89ab0",
        role: Role::Program,
    },
    Artifact {
        repo: CENTRAL,
        group: "com/google/guava",
        name: "failureaccess",
        version: "1.0.2",
        kind: "jar",
        sha256: "8a8f81cf9b359e3f6dfa691a1e776985c061ef2f223c9b2c80753e1b458e8064",
        role: Role::Program,
    },
    Artifact {
        repo: GOOGLE,
        group: "androidx/annotation",
        name: "annotation-jvm",
        version: "1.6.0",
        kind: "jar",
        sha256: "60b10b5ef5769b79570172e015b8159405c92f034ba88b9391a977589c9deb4e",
        role: Role::Classpath,
    },
    Artifact {
        repo: GOOGLE,
        group: "androidx/annotation",
        name: "annotation-experimental",
        version: "1.3.1",
        kind: "aar",
        sha256: "e4d49dc3880b7b7437ea54ce71461e02897faa4e1ecc53e63ecadb1a14c20c9c",
        role: Role::Classpath,
    },
    // Kotlin's annotation classes, which androidx.annotation's own
    // annotations use: without them javac warns about every one.
    Artifact {
        repo: CENTRAL,
        group: "org/jetbrains/kotlin",
        name: "kotlin-stdlib",
        version: "1.8.0",
        kind: "jar",
        sha256: "c77bef8774640b9fb9d6e217459ff220dae59878beb7d2e4b430506feffc654e",
        role: Role::Classpath,
    },
    Artifact {
        repo: CENTRAL,
        group: "org/checkerframework",
        name: "checker-qual",
        version: "3.43.0",
        kind: "jar",
        sha256: "3fbc2e98f05854c3df16df9abaa955b91b15b3ecac33623208ed6424640ef0f6",
        role: Role::Classpath,
    },
];

const fn media3(name: &'static str, sha256: &'static str) -> Artifact {
    Artifact {
        repo: GOOGLE,
        group: "androidx/media3",
        name: match name.as_bytes() {
            b"exoplayer" => "media3-exoplayer",
            b"common" => "media3-common",
            b"container" => "media3-container",
            b"database" => "media3-database",
            b"datasource" => "media3-datasource",
            b"decoder" => "media3-decoder",
            b"extractor" => "media3-extractor",
            _ => panic!("unknown media3 module"),
        },
        version: VERSION,
        kind: "aar",
        sha256,
        role: Role::Program,
    }
}

/// FFmpeg release whose headers match the libraries in libmpv
/// (libavcodec 63.1.101, libavutil 61.1.101, libswresample 7.1).
pub const FFMPEG_VERSION: &str = "9.0.1";
const FFMPEG_URL: &str = "https://ffmpeg.org/releases/ffmpeg-9.0.1.tar.xz";
const FFMPEG_SHA256: &str = "cf38e0e28c7e5605942c4a77755349b0145804a397af37eb1fb4c77cb237f635";

/// The jars and ProGuard rules for the build.
pub struct Libraries {
    /// Jars compiled into the APK.
    pub program: Vec<PathBuf>,
    /// Annotation jars, only for compiling.
    pub classpath: Vec<PathBuf>,
    /// Consumer ProGuard rules shipped in the AARs.
    pub rules: Vec<PathBuf>,
}

fn media3_dir() -> PathBuf {
    workspace_root().join(".local/media3").join(VERSION)
}

/// The libraries, fetching them first if needed.
pub fn libraries() -> Result<Libraries> {
    let dir = media3_dir();
    if !dir.join(".complete").is_file() {
        fetch()?;
    }
    let mut libs = Libraries { program: Vec::new(), classpath: Vec::new(), rules: Vec::new() };
    for artifact in ARTIFACTS {
        let jar = dir.join(format!("{}.jar", artifact.name));
        ensure!(jar.is_file(), "{} is missing; delete .local/media3 and run `cargo xtask fetch-media3`", jar.display());
        match artifact.role {
            Role::Program => libs.program.push(jar),
            Role::Classpath => libs.classpath.push(jar),
        }
        let rules = dir.join(format!("{}.pro", artifact.name));
        if artifact.role == Role::Program && rules.is_file() {
            libs.rules.push(rules);
        }
    }
    Ok(libs)
}

/// The FFmpeg include directory, fetching it first if needed.
pub fn ffmpeg_headers() -> Result<PathBuf> {
    let dir = workspace_root().join(".local/ffmpeg-headers").join(FFMPEG_VERSION);
    if !dir.join(".complete").is_file() {
        fetch_ffmpeg_headers(&dir)?;
    }
    Ok(dir.join("include"))
}

/// Download and unpack everything (`cargo xtask fetch-media3`).
pub fn fetch() -> Result<()> {
    let target = media3_dir();
    if !target.join(".complete").is_file() {
        let staging = target.with_file_name(format!("{VERSION}.partial"));
        if staging.exists() {
            std::fs::remove_dir_all(&staging)?;
        }
        std::fs::create_dir_all(&staging)?;
        let downloads = workspace_root().join(".local/media3/downloads");
        std::fs::create_dir_all(&downloads)?;
        for artifact in ARTIFACTS {
            let file = format!("{}-{}.{}", artifact.name, artifact.version, artifact.kind);
            let url = format!("{}/{}/{}/{}/{file}", artifact.repo, artifact.group, artifact.name, artifact.version);
            let path = downloads.join(&file);
            download(&url, &path, artifact.sha256)?;
            let jar = staging.join(format!("{}.jar", artifact.name));
            if artifact.kind == "jar" {
                std::fs::copy(&path, &jar)?;
            } else {
                unpack_aar(&path, &jar, &staging.join(format!("{}.pro", artifact.name)))?;
            }
        }
        std::fs::write(staging.join(".complete"), format!("media3 {VERSION}\n"))?;
        if target.exists() {
            std::fs::remove_dir_all(&target)?;
        }
        std::fs::rename(&staging, &target)?;
        std::fs::remove_dir_all(&downloads)?;
        println!("Media3 {VERSION} unpacked to {}", target.display());
    }
    let headers = ffmpeg_headers()?;
    println!("FFmpeg {FFMPEG_VERSION} headers in {}", headers.display());
    Ok(())
}

/// `classes.jar` and `proguard.txt` out of an AAR.
fn unpack_aar(aar: &Path, jar: &Path, rules: &Path) -> Result<()> {
    let mut zip = zip::ZipArchive::new(File::open(aar)?).with_context(|| format!("open {}", aar.display()))?;
    {
        let mut classes = zip.by_name("classes.jar").with_context(|| format!("{} has no classes.jar", aar.display()))?;
        std::io::copy(&mut classes, &mut File::create(jar)?)?;
    }
    if let Ok(mut file) = zip.by_name("proguard.txt") {
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        std::fs::write(rules, text)?;
    }
    Ok(())
}

/// Downloads `url` to `dest` unless a verified copy exists.
fn download(url: &str, dest: &Path, sha256: &str) -> Result<()> {
    if dest.is_file() && sha256_file(dest)? == sha256 {
        return Ok(());
    }
    let partial = dest.with_extension("part");
    util::run(
        command("curl")
            .args(["--fail", "--location", "--retry", "3", "--silent", "--show-error", "--output"])
            .arg(&partial)
            .arg(url),
    )?;
    let actual = sha256_file(&partial)?;
    if actual != sha256 {
        let _ = std::fs::remove_file(&partial);
        bail!("{url}: SHA-256 mismatch: expected {sha256}, got {actual}");
    }
    std::fs::rename(&partial, dest)?;
    Ok(())
}

/// The public headers of libavcodec, libavutil and libswresample, plus the
/// one header FFmpeg's configure generates (`libavutil/avconfig.h`).
fn fetch_ffmpeg_headers(dir: &Path) -> Result<()> {
    let base = dir.parent().context("headers dir has no parent")?;
    std::fs::create_dir_all(base)?;
    let tarball = base.join(format!("ffmpeg-{FFMPEG_VERSION}.tar.xz"));
    download(FFMPEG_URL, &tarball, FFMPEG_SHA256)?;
    let staging = dir.with_file_name(format!("{FFMPEG_VERSION}.partial"));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(&staging)?;
    let top = format!("ffmpeg-{FFMPEG_VERSION}");
    util::run(
        command("tar")
            .arg("-xJf")
            .arg(&tarball)
            .arg("-C")
            .arg(&staging)
            .arg("--wildcards")
            .args(["libavcodec", "libavutil", "libswresample"].map(|lib| format!("{top}/{lib}/*.h"))),
    )
    .context("tar (needs xz support)")?;
    let include = staging.join("include");
    std::fs::rename(staging.join(&top), &include)?;
    // Only the public API is used; these two values don't change any struct layout.
    std::fs::write(
        include.join("libavutil/avconfig.h"),
        "/* Written by cargo xtask: FFmpeg's configure generates this file. */\n\
         #ifndef AVUTIL_AVCONFIG_H\n#define AVUTIL_AVCONFIG_H\n\
         #define AV_HAVE_BIGENDIAN 0\n#define AV_HAVE_FAST_UNALIGNED 0\n#endif\n",
    )?;
    let version = std::fs::read_to_string(include.join("libavcodec/version_major.h"))?;
    ensure!(version.contains("LIBAVCODEC_VERSION_MAJOR  63"), "FFmpeg {FFMPEG_VERSION} headers aren't libavcodec 63");
    std::fs::write(staging.join(".complete"), format!("ffmpeg {FFMPEG_VERSION}\n"))?;
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    std::fs::rename(&staging, dir)?;
    std::fs::remove_file(&tarball)?;
    Ok(())
}
