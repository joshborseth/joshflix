//! Fetches Stremio's prebuilt libmpv for Android.
//!
//! The release tarball is a Maven repository holding one AAR. We only need the
//! AAR's `jni/<abi>/*.so` files. Both the tarball and the AAR are pinned by
//! SHA-256. See `android/README.md` for what the libraries contain.

use std::fs::File;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

use crate::sdk::Abi;
use crate::util::{self, command, sha256_bytes, sha256_file, workspace_root};

pub const VERSION: &str = "1.0.0-stremio02";
const URL: &str =
    "https://github.com/Stremio/libmpv-android/releases/download/1.0.0-stremio02/libmpv.tar.gz";
const TARBALL_SHA256: &str = "84fe683ccef414129a97abb52641dd21db149bc9083981225ac1f5ce233e9cd0";
const AAR_PATH: &str = "libmpv/1.0.0-stremio02/libmpv-1.0.0-stremio02.aar";
/// Matches the `sha256` in the tarball's Gradle module metadata.
const AAR_SHA256: &str = "5af4652b16caf3e26a597904dc609c69be8e2fb794d0ea77b1ba26ce725096e0";

/// Libraries in the AAR that we do not ship.
///
/// `libplayer.so` is the JNI glue for the AAR's Kotlin `dev.jdtech.mpv.MPVLib`
/// class. We have no Java code, so it would only waste space.
const EXCLUDED: &[&str] = &["libplayer.so"];

fn base_dir() -> PathBuf {
    workspace_root().join(".local/libmpv")
}

/// Directory holding the extracted `<abi>/*.so` files.
fn extracted_dir() -> PathBuf {
    base_dir().join(VERSION)
}

/// The libmpv shared libraries for `abi`, fetching them first if needed.
pub fn libraries(abi: Abi) -> Result<Vec<PathBuf>> {
    if !extracted_dir().join(".complete").is_file() {
        fetch()?;
    }
    let dir = extracted_dir().join(abi.android_name());
    let mut libs: Vec<PathBuf> = std::fs::read_dir(&dir)
        .with_context(|| format!("read {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "so"))
        .collect();
    libs.sort();
    ensure!(
        libs.iter().any(|p| p.ends_with("libmpv.so")),
        "{} has no libmpv.so; delete .local/libmpv and run `cargo xtask fetch-libmpv`",
        dir.display()
    );
    Ok(libs)
}

pub fn fetch() -> Result<()> {
    let target = extracted_dir();
    if target.join(".complete").is_file() {
        println!("libmpv {VERSION} already extracted in {}", target.display());
        return Ok(());
    }
    std::fs::create_dir_all(base_dir())?;
    let tarball = base_dir().join("libmpv.tar.gz");
    download(&tarball)?;

    let aar = read_aar(&tarball)?;
    let staging = base_dir().join(format!("{VERSION}.partial"));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    extract_jni(&aar, &staging)?;
    std::fs::write(staging.join(".complete"), format!("{AAR_SHA256}\n"))?;
    if target.exists() {
        std::fs::remove_dir_all(&target)?;
    }
    std::fs::rename(&staging, &target)?;
    // The extracted libraries are all we use; drop the 47 MB tarball.
    std::fs::remove_file(&tarball)?;

    for abi in Abi::ALL {
        let names: Vec<String> = std::fs::read_dir(target.join(abi.android_name()))?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        println!("{}: {}", abi.android_name(), names.join(" "));
    }
    println!("libmpv {VERSION} extracted to {}", target.display());
    Ok(())
}

/// Downloads the tarball unless a verified copy already exists.
fn download(dest: &Path) -> Result<()> {
    if dest.is_file() {
        if sha256_file(dest)? == TARBALL_SHA256 {
            return Ok(());
        }
        eprintln!(
            "cached {} has the wrong hash; downloading again",
            dest.display()
        );
        std::fs::remove_file(dest)?;
    }
    let partial = dest.with_extension("gz.part");
    util::run(
        command("curl")
            .args([
                "--fail",
                "--location",
                "--retry",
                "3",
                "--silent",
                "--show-error",
                "--output",
            ])
            .arg(&partial)
            .arg(URL),
    )?;
    let actual = sha256_file(&partial)?;
    if actual != TARBALL_SHA256 {
        let _ = std::fs::remove_file(&partial);
        bail!("libmpv tarball SHA-256 mismatch: expected {TARBALL_SHA256}, got {actual}");
    }
    std::fs::rename(&partial, dest)?;
    Ok(())
}

/// Reads the AAR out of the tarball and checks its hash.
fn read_aar(tarball: &Path) -> Result<Vec<u8>> {
    let gz = flate2::read::GzDecoder::new(File::open(tarball)?);
    let mut archive = tar::Archive::new(gz);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if path.strip_prefix(".").unwrap_or(&path) == Path::new(AAR_PATH) {
            let mut aar = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut aar)?;
            let actual = sha256_bytes(&aar);
            ensure!(
                actual == AAR_SHA256,
                "AAR SHA-256 mismatch: expected {AAR_SHA256}, got {actual}"
            );
            return Ok(aar);
        }
    }
    bail!("{AAR_PATH} not found in {}", tarball.display())
}

/// Extracts `jni/<abi>/<lib>.so` for the ABIs we ship into `dest/<abi>/`.
fn extract_jni(aar: &[u8], dest: &Path) -> Result<()> {
    let mut zip = zip::ZipArchive::new(Cursor::new(aar)).context("open AAR")?;
    for i in 0..zip.len() {
        let mut file = zip.by_index(i)?;
        let name = file.name()?.into_owned();
        let parts: Vec<&str> = name.split('/').collect();
        let [jni, abi_name, lib] = parts[..] else {
            continue;
        };
        let Some(abi) = Abi::from_android_name(abi_name) else {
            continue;
        };
        let safe = jni == "jni"
            && lib.ends_with(".so")
            && !lib.starts_with('.')
            && lib
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b));
        if !safe || EXCLUDED.contains(&lib) {
            continue;
        }
        let dir = dest.join(abi.android_name());
        std::fs::create_dir_all(&dir)?;
        let mut out = File::create(dir.join(lib))?;
        std::io::copy(&mut file, &mut out).with_context(|| format!("extract {name}"))?;
    }
    for abi in Abi::ALL {
        ensure!(
            dest.join(abi.android_name()).join("libmpv.so").is_file(),
            "AAR has no jni/{}/libmpv.so",
            abi.android_name()
        );
    }
    Ok(())
}
