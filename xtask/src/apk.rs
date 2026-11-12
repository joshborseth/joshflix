//! `cargo xtask apk`: cross-compile the app and package a signed APK.
//!
//! Steps per ABI: `cargo build` with the NDK clang as linker, then
//! `aapt2 compile` + `aapt2 link` for the manifest and resources, add
//! `lib/<abi>/*.so` stored uncompressed, `zipalign -P 16`, `apksigner sign`,
//! and verify the result.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::args::Args;
use crate::libmpv;
use crate::sdk::{Abi, Sdk};
use crate::util::{self, command, hex, workspace_root};
use crate::version::Version;

/// File name of the app library inside the APK; matches `android.app.lib_name`.
pub const APP_LIB: &str = "libjoshflix_app.so";
const APP_PACKAGE: &str = "joshflix-app";
const KEY_ALIAS: &str = "joshflix";
const PASSWORD_VAR: &str = "JOSHFLIX_KEYSTORE_PASSWORD";

pub struct ApkOptions {
    pub abis: Vec<Abi>,
    pub debug: bool,
    /// Hidden: build this crate's cdylib instead of `joshflix-app` (pipeline smoke tests).
    pub crate_manifest: Option<PathBuf>,
}

impl ApkOptions {
    pub fn parse(args: &mut Args) -> Result<Self> {
        let abis = match args.value("--abi")? {
            Some(value) => Abi::parse_list(&value)?,
            None => Abi::ALL.to_vec(),
        };
        let debug = args.flag("--debug");
        let crate_manifest = args.value("--crate-manifest")?.map(PathBuf::from);
        Ok(Self {
            abis,
            debug,
            crate_manifest,
        })
    }
}

/// `dist/joshflix-<versionName>-<abi>[-debug].apk`
pub fn apk_path(version: &Version, abi: Abi, debug: bool) -> PathBuf {
    let suffix = if debug { "-debug" } else { "" };
    workspace_root().join("dist").join(format!(
        "joshflix-{}-{}{suffix}.apk",
        version.version_name,
        abi.android_name()
    ))
}

pub fn build(opts: &ApkOptions) -> Result<Vec<PathBuf>> {
    let sdk = Sdk::locate()?;
    let version = Version::load()?;
    let signer = Signer::load_or_create()?;
    let profile = if opts.debug { "debug" } else { "release" };
    let work = workspace_root().join("target/android-apk").join(profile);
    std::fs::create_dir_all(&work)?;
    std::fs::create_dir_all(workspace_root().join("dist"))?;

    // Manifest and resources do not depend on the ABI: link them once.
    let base = link_resources(&sdk, &version, &work)?;

    let mut outputs = Vec::new();
    for &abi in &opts.abis {
        eprintln!("==> {} ({profile})", abi.android_name());
        let app_lib = cargo_build(&sdk, abi, opts.debug, opts.crate_manifest.as_deref())?;
        check_app_lib(&sdk, abi, &app_lib)?;
        let mut libs = vec![(APP_LIB.to_owned(), app_lib)];
        for lib in libmpv::libraries(abi)? {
            let name = lib.file_name().unwrap().to_string_lossy().into_owned();
            ensure!(name != APP_LIB, "libmpv ships a library named {APP_LIB}");
            libs.push((name, lib));
        }
        let out = apk_path(&version, abi, opts.debug);
        package(&sdk, &signer, &base, &libs, abi, &work, &out)?;
        outputs.push(out);
    }
    for out in &outputs {
        let size = std::fs::metadata(out)?.len();
        println!("{} ({:.1} MB)", out.display(), size as f64 / 1_048_576.0);
    }
    Ok(outputs)
}

/// `aapt2 compile` + `aapt2 link` into an APK holding the manifest and resources.
fn link_resources(sdk: &Sdk, version: &Version, work: &Path) -> Result<PathBuf> {
    let android = workspace_root().join("android");
    let compiled = work.join("res.zip");
    let _ = std::fs::remove_file(&compiled);
    util::run(
        command(sdk.tool("aapt2"))
            .arg("compile")
            .arg("--dir")
            .arg(android.join("res"))
            .arg("-o")
            .arg(&compiled),
    )?;
    let base = work.join("base.apk");
    util::run(
        command(sdk.tool("aapt2"))
            .arg("link")
            .arg("-o")
            .arg(&base)
            .arg("-I")
            .arg(sdk.android_jar()?)
            .arg("--manifest")
            .arg(android.join("AndroidManifest.xml"))
            .args(["--version-code", &version.version_code.to_string()])
            .args(["--version-name", &version.version_name])
            .arg("--replace-version")
            .arg(&compiled),
    )?;
    Ok(base)
}

struct CrateInfo {
    target_dir: PathBuf,
    lib_name: String,
    /// `cargo build` arguments that select the crate.
    select: Vec<String>,
}

fn crate_info(crate_manifest: Option<&Path>) -> Result<CrateInfo> {
    let mut cmd = command(cargo());
    cmd.args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(workspace_root());
    let manifest = match crate_manifest {
        Some(path) => {
            let path = path
                .canonicalize()
                .with_context(|| format!("{} not found", path.display()))?;
            cmd.arg("--manifest-path").arg(&path);
            Some(path)
        }
        None => None,
    };
    let metadata: serde_json::Value = serde_json::from_str(&util::output(&mut cmd)?)?;
    let target_dir = metadata["target_directory"]
        .as_str()
        .context("cargo metadata: no target_directory")?;
    let packages = metadata["packages"]
        .as_array()
        .context("cargo metadata: no packages")?;
    let package = packages
        .iter()
        .find(|p| match &manifest {
            Some(path) => p["manifest_path"].as_str().map(Path::new) == Some(path.as_path()),
            None => p["name"] == APP_PACKAGE,
        })
        .context("cargo metadata: app package not found")?;
    let lib = package["targets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| {
            t["crate_types"]
                .as_array()
                .is_some_and(|k| k.iter().any(|k| k == "cdylib"))
        })
        .context("the app package has no cdylib target")?;
    let lib_name = lib["name"]
        .as_str()
        .context("cdylib target has no name")?
        .replace('-', "_");
    let select = match &manifest {
        Some(path) => vec![
            "--manifest-path".to_owned(),
            path.to_string_lossy().into_owned(),
        ],
        None => vec!["-p".to_owned(), APP_PACKAGE.to_owned()],
    };
    Ok(CrateInfo {
        target_dir: PathBuf::from(target_dir),
        lib_name,
        select,
    })
}

fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned())
}

/// Cross-compiles the app cdylib and returns its path.
fn cargo_build(sdk: &Sdk, abi: Abi, debug: bool, crate_manifest: Option<&Path>) -> Result<PathBuf> {
    let info = crate_info(crate_manifest)?;
    let triple = abi.rust_triple();
    let env_triple = triple.replace('-', "_");
    let upper = env_triple.to_uppercase();
    let clang = sdk.clang(abi);
    ensure!(
        clang.is_file(),
        "NDK clang not found at {}",
        clang.display()
    );

    // 16 KB ELF segment alignment (required for 64-bit apps on Android 15+ devices
    // with 16 KB pages). NDK r27 still defaults to 4 KB.
    let rustflags_var = format!("CARGO_TARGET_{upper}_RUSTFLAGS");
    let mut rustflags = std::env::var(&rustflags_var).unwrap_or_default();
    if !rustflags.contains("max-page-size") {
        rustflags = format!("{rustflags} -C link-arg=-Wl,-z,max-page-size=16384")
            .trim()
            .to_owned();
    }

    let mut cmd = command(cargo());
    cmd.arg("build")
        .args(&info.select)
        .args(["--lib", "--target", triple])
        .current_dir(workspace_root())
        .env(format!("CARGO_TARGET_{upper}_LINKER"), &clang)
        .env(&rustflags_var, rustflags)
        .env(format!("CC_{env_triple}"), &clang)
        .env(format!("CXX_{env_triple}"), sdk.clangxx(abi))
        .env(format!("AR_{env_triple}"), sdk.llvm_bin().join("llvm-ar"))
        .env("ANDROID_HOME", &sdk.root)
        .env("ANDROID_SDK_ROOT", &sdk.root)
        .env("ANDROID_NDK_HOME", &sdk.ndk)
        .env("ANDROID_NDK_ROOT", &sdk.ndk);
    if std::env::var_os("CARGO_BUILD_JOBS").is_none() {
        cmd.env("CARGO_BUILD_JOBS", "6");
    }
    if !debug {
        cmd.arg("--release");
    }
    util::run(&mut cmd)?;

    let profile = if debug { "debug" } else { "release" };
    let lib = info
        .target_dir
        .join(triple)
        .join(profile)
        .join(format!("lib{}.so", info.lib_name));
    ensure!(
        lib.is_file(),
        "cargo build did not produce {}",
        lib.display()
    );
    Ok(lib)
}

/// Checks the app library before packaging it.
///
/// - `NativeActivity` calls `ANativeActivity_onCreate`; android-activity's
///   `native-activity` feature exports it once the crate defines `android_main`.
/// - 64-bit libraries need LOAD segments aligned to 16 KB.
fn check_app_lib(sdk: &Sdk, abi: Abi, lib: &Path) -> Result<()> {
    let symbols = util::output(
        command(sdk.llvm_bin().join("llvm-nm"))
            .args(["-D", "--defined-only"])
            .arg(lib),
    )?;
    ensure!(
        symbols
            .lines()
            .any(|l| l.split_whitespace().last() == Some("ANativeActivity_onCreate")),
        "{} does not export ANativeActivity_onCreate, so NativeActivity cannot start it; \
         define `android_main` with android-activity's `native-activity` feature",
        lib.display()
    );
    if abi == Abi::Armv7 {
        return Ok(());
    }
    let headers = util::output(
        command(sdk.llvm_bin().join("llvm-readelf"))
            .arg("-lW")
            .arg(lib),
    )?;
    for line in headers
        .lines()
        .filter(|l| l.trim_start().starts_with("LOAD"))
    {
        let align = line.split_whitespace().last().unwrap_or_default();
        let value = u64::from_str_radix(align.trim_start_matches("0x"), 16).unwrap_or(0);
        ensure!(
            value >= 0x4000,
            "{} has a LOAD segment aligned to {align}; need 0x4000",
            lib.display()
        );
    }
    Ok(())
}

/// Adds the native libraries to the linked base APK, aligns, signs and verifies.
fn package(
    sdk: &Sdk,
    signer: &Signer,
    base: &Path,
    libs: &[(String, PathBuf)],
    abi: Abi,
    work: &Path,
    out: &Path,
) -> Result<()> {
    let unaligned = work.join(format!("{}-unaligned.apk", abi.android_name()));
    {
        let mut reader = ZipArchive::new(File::open(base)?).context("open linked APK")?;
        let mut writer = ZipWriter::new(File::create(&unaligned)?);
        for i in 0..reader.len() {
            writer.raw_copy_file(reader.by_index_raw(i)?)?;
        }
        // Stored, so Android can mmap the libraries straight from the APK
        // (`extractNativeLibs="false"`).
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .unix_permissions(0o755);
        for (name, path) in libs {
            writer.start_file(format!("lib/{}/{name}", abi.android_name()), options)?;
            let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
            std::io::copy(&mut file, &mut writer)
                .with_context(|| format!("add {}", path.display()))?;
        }
        writer.finish()?;
    }

    let aligned = work.join(format!("{}-aligned.apk", abi.android_name()));
    util::run(
        command(sdk.tool("zipalign"))
            .args(["-P", "16", "-f", "4"])
            .arg(&unaligned)
            .arg(&aligned),
    )?;

    let file_name = out.file_name().unwrap().to_string_lossy();
    let signed = out.with_file_name(format!(".{file_name}.tmp"));
    let _ = std::fs::remove_file(&signed);
    util::run(
        command(sdk.tool("apksigner"))
            .arg("sign")
            .arg("--ks")
            .arg(&signer.keystore)
            .args(["--ks-key-alias", KEY_ALIAS])
            .args(["--ks-pass", &format!("env:{PASSWORD_VAR}")])
            .args(["--key-pass", &format!("env:{PASSWORD_VAR}")])
            // minSdk 26 only needs v2/v3; skip the legacy JAR signature and the v4 .idsig file.
            .args([
                "--v1-signing-enabled",
                "false",
                "--v2-signing-enabled",
                "true",
            ])
            .args([
                "--v3-signing-enabled",
                "true",
                "--v4-signing-enabled",
                "false",
            ])
            .arg("--out")
            .arg(&signed)
            .arg(&aligned)
            .env(PASSWORD_VAR, &signer.password),
    )?;
    util::run(command(sdk.tool("apksigner")).arg("verify").arg(&signed))?;
    util::run(
        command(sdk.tool("zipalign"))
            .args(["-c", "-P", "16", "4"])
            .arg(&signed),
    )?;
    std::fs::rename(&signed, out)?;
    let _ = std::fs::remove_file(&unaligned);
    let _ = std::fs::remove_file(&aligned);
    Ok(())
}

/// The release keystore, generated once in `.local/`.
struct Signer {
    keystore: PathBuf,
    password: String,
}

impl Signer {
    fn load_or_create() -> Result<Self> {
        let local = workspace_root().join(".local");
        let keystore = local.join("joshflix-release.jks");
        let env_file = local.join("keystore.env");
        match (keystore.is_file(), env_file.is_file()) {
            (true, true) => {
                let text = std::fs::read_to_string(&env_file)?;
                let password = text
                    .lines()
                    .find_map(|l| l.strip_prefix(&format!("{PASSWORD_VAR}=")))
                    .map(|p| p.trim().to_owned())
                    .filter(|p| !p.is_empty())
                    .with_context(|| format!("{} has no {PASSWORD_VAR}", env_file.display()))?;
                Ok(Self { keystore, password })
            }
            (false, false) => Self::create(&local, keystore, env_file),
            _ => bail!(
                "only one of {} and {} exists; restore the missing file from backup \
                 (a new key cannot update installed apps)",
                keystore.display(),
                env_file.display()
            ),
        }
    }

    fn create(local: &Path, keystore: PathBuf, env_file: PathBuf) -> Result<Self> {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;

        std::fs::create_dir_all(local)?;
        let mut random = [0u8; 24];
        File::open("/dev/urandom")?.read_exact(&mut random)?;
        let password = hex(&random);

        eprintln!("Generating release signing key in {}", keystore.display());
        let mut env = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&env_file)?;
        writeln!(
            env,
            "# Password for joshflix-release.jks (alias {KEY_ALIAS}). Back up both files together."
        )?;
        writeln!(env, "{PASSWORD_VAR}={password}")?;
        env.sync_all()?;

        let result = util::run(
            command("keytool")
                .args([
                    "-genkeypair",
                    "-noprompt",
                    "-storetype",
                    "PKCS12",
                    "-keystore",
                ])
                .arg(&keystore)
                .args(["-alias", KEY_ALIAS, "-keyalg", "RSA", "-keysize", "4096"])
                .args(["-validity", "36500", "-dname", "CN=Joshflix, O=Joshflix"])
                .args(["-storepass:env", PASSWORD_VAR, "-keypass:env", PASSWORD_VAR])
                .env(PASSWORD_VAR, &password),
        );
        if let Err(err) = result {
            let _ = std::fs::remove_file(&env_file);
            let _ = std::fs::remove_file(&keystore);
            return Err(err);
        }
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&keystore, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self { keystore, password })
    }
}
