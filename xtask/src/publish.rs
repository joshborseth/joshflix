//! `cargo xtask publish`: push `main` to GitHub, copy the release APKs into
//! the relay's data directory, write `updates/manifest.json`, and tag the
//! release.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use joshflix_protocol::{UpdateApk, UpdateManifest};

use crate::apk;
use crate::device::PACKAGE;
use crate::git;
use crate::sdk::{Abi, Sdk};
use crate::util::{self, command, home_dir};
use crate::version::Version;

fn relay_data_dir() -> Result<PathBuf> {
    match std::env::var_os("JOSHFLIX_RELAY_DATA").filter(|v| !v.is_empty()) {
        Some(dir) => Ok(PathBuf::from(dir)),
        None => Ok(home_dir()?.join(".local/share/joshflix-relay")),
    }
}

pub fn publish(notes: Option<&str>) -> Result<()> {
    let version = Version::load()?;
    let sdk = Sdk::locate()?;
    let notes = match notes {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("read {path}"))?
            .trim()
            .to_owned(),
        None => String::new(),
    };

    // Publish every ABI that was built (x86_64 only matters for emulators).
    let apks: Vec<(Abi, PathBuf)> = Abi::ALL
        .iter()
        .map(|&abi| (abi, apk::apk_path(&version, abi, false)))
        .filter(|(abi, path)| {
            let present = path.is_file();
            if !present {
                eprintln!(
                    "note: no {abi:?} APK at {}; TVs with that ABI won't see this update",
                    path.display()
                );
            }
            present
        })
        .collect();
    ensure!(
        !apks.is_empty(),
        "no release APKs in dist/; run `cargo xtask apk` first"
    );
    for (_, path) in &apks {
        check_badging(&sdk, path, &version)?;
    }

    let updates = relay_data_dir()?.join("updates");
    std::fs::create_dir_all(&updates).with_context(|| format!("create {}", updates.display()))?;
    let manifest_path = updates.join("manifest.json");
    if manifest_path.is_file() {
        let current: UpdateManifest = serde_json::from_slice(&std::fs::read(&manifest_path)?)
            .with_context(|| format!("parse {}", manifest_path.display()))?;
        if version.version_code <= current.version_code {
            bail!(
                "version_code {} is not higher than the published {} ({}); bump android/version.toml",
                version.version_code,
                current.version_code,
                current.version_name
            );
        }
        // APK file names carry only the version name; reusing it would overwrite
        // the files the current manifest points at.
        if version.version_name == current.version_name {
            bail!(
                "version_name {} is already published; bump it too",
                version.version_name
            );
        }
    }

    let apk_paths: Vec<&Path> = apks.iter().map(|(_, path)| path.as_path()).collect();
    git::check_release(&version.version_name, &apk_paths)?;
    git::push_main()?;

    let mut entries = Vec::new();
    for (abi, path) in &apks {
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        let (sha256, size) = util::copy_atomic(path, &updates.join(&file))?;
        println!("{file}: {size} bytes, sha256 {sha256}");
        entries.push(UpdateApk {
            abi: abi.android_name().to_owned(),
            path: format!("/updates/{file}"),
            sha256,
            size,
        });
    }
    let manifest = UpdateManifest {
        version_name: version.version_name.clone(),
        version_code: version.version_code,
        notes,
        published_at: util::now_rfc3339(),
        apks: entries,
    };
    let mut json = serde_json::to_vec_pretty(&manifest)?;
    json.push(b'\n');
    util::write_atomic(&manifest_path, &json)?;
    println!(
        "Published {} ({}) to {}",
        version.version_name,
        version.version_code,
        manifest_path.display()
    );
    git::push_tag(&version.version_name)?;
    Ok(())
}

/// Checks the APK's package and version against `android/version.toml`.
fn check_badging(sdk: &Sdk, path: &Path, version: &Version) -> Result<()> {
    let badging = util::output(
        command(sdk.tool("aapt2"))
            .args(["dump", "badging"])
            .arg(path),
    )?;
    let line = badging
        .lines()
        .find(|l| l.starts_with("package:"))
        .context("aapt2 badging: no package line")?;
    let field = |key: &str| {
        let start = line.find(&format!("{key}='"))? + key.len() + 2;
        let len = line[start..].find('\'')?;
        Some(line[start..start + len].to_owned())
    };
    let expected = [
        ("name", PACKAGE.to_owned()),
        ("versionCode", version.version_code.to_string()),
        ("versionName", version.version_name.clone()),
    ];
    for (key, want) in expected {
        let got = field(key).unwrap_or_default();
        ensure!(
            got == want,
            "{} has {key}={got:?}, expected {want:?}; rebuild with `cargo xtask apk`",
            path.display()
        );
    }
    Ok(())
}
