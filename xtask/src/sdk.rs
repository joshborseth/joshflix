//! Locating the Android SDK, NDK and build tools, and the ABIs we ship.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::util::home_dir;

/// NDK the project is tested with. A newer installed NDK is used if this one is missing.
pub const NDK_VERSION: &str = "27.2.12479018";
pub const BUILD_TOOLS_VERSION: &str = "35.0.0";
pub const COMPILE_SDK: u32 = 35;
/// `minSdkVersion`; also the API level of the NDK clang wrappers.
pub const MIN_SDK: u32 = 26;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Abi {
    Armv7,
    Arm64,
    X86_64,
}

impl Abi {
    pub const ALL: [Abi; 3] = [Abi::Armv7, Abi::Arm64, Abi::X86_64];

    /// Parses the `--abi` value (short names or Android ABI names).
    pub fn parse_list(value: &str) -> Result<Vec<Abi>> {
        Ok(match value {
            "all" => Self::ALL.to_vec(),
            "arm" | "armv7" | "armeabi-v7a" => vec![Abi::Armv7],
            "arm64" | "aarch64" | "arm64-v8a" => vec![Abi::Arm64],
            "x86_64" | "x64" => vec![Abi::X86_64],
            other => bail!("unknown ABI `{other}` (expected arm, arm64, x86_64 or all)"),
        })
    }

    pub fn from_android_name(name: &str) -> Option<Abi> {
        Self::ALL.into_iter().find(|abi| abi.android_name() == name)
    }

    /// The directory name under `lib/` in an APK.
    pub fn android_name(self) -> &'static str {
        match self {
            Abi::Armv7 => "armeabi-v7a",
            Abi::Arm64 => "arm64-v8a",
            Abi::X86_64 => "x86_64",
        }
    }

    pub fn rust_triple(self) -> &'static str {
        match self {
            Abi::Armv7 => "armv7-linux-androideabi",
            Abi::Arm64 => "aarch64-linux-android",
            Abi::X86_64 => "x86_64-linux-android",
        }
    }

    /// Prefix of the NDK's `<prefix><api>-clang` wrapper.
    fn clang_prefix(self) -> &'static str {
        match self {
            Abi::Armv7 => "armv7a-linux-androideabi",
            Abi::Arm64 => "aarch64-linux-android",
            Abi::X86_64 => "x86_64-linux-android",
        }
    }
}

pub struct Sdk {
    pub root: PathBuf,
    pub ndk: PathBuf,
    pub build_tools: PathBuf,
}

impl Sdk {
    pub fn locate() -> Result<Self> {
        let root = ["ANDROID_HOME", "ANDROID_SDK_ROOT"]
            .iter()
            .find_map(|var| std::env::var_os(var).filter(|v| !v.is_empty()))
            .map(PathBuf::from)
            .map_or_else(|| home_dir().map(|h| h.join("Android/Sdk")), Ok)?;
        if !root.is_dir() {
            bail!(
                "Android SDK not found at {} (set ANDROID_HOME)",
                root.display()
            );
        }
        let ndk = match std::env::var_os("ANDROID_NDK_HOME").filter(|v| !v.is_empty()) {
            Some(path) => PathBuf::from(path),
            None => find_ndk(&root)?,
        };
        let build_tools = root.join("build-tools").join(BUILD_TOOLS_VERSION);
        if !build_tools.join("aapt2").is_file() {
            bail!(
                "build-tools {BUILD_TOOLS_VERSION} missing: run `sdkmanager \"build-tools;{BUILD_TOOLS_VERSION}\"`"
            );
        }
        Ok(Self {
            root,
            ndk,
            build_tools,
        })
    }

    pub fn tool(&self, name: &str) -> PathBuf {
        self.build_tools.join(name)
    }

    pub fn android_jar(&self) -> Result<PathBuf> {
        let jar = self
            .root
            .join(format!("platforms/android-{COMPILE_SDK}/android.jar"));
        if !jar.is_file() {
            bail!(
                "{} missing: run `sdkmanager \"platforms;android-{COMPILE_SDK}\"`",
                jar.display()
            );
        }
        Ok(jar)
    }

    pub fn adb(&self) -> Result<PathBuf> {
        let adb = self.root.join("platform-tools/adb");
        if !adb.is_file() {
            bail!("adb missing: run `sdkmanager platform-tools`");
        }
        Ok(adb)
    }

    pub fn llvm_bin(&self) -> PathBuf {
        let host = if cfg!(target_os = "macos") {
            "darwin-x86_64"
        } else {
            "linux-x86_64"
        };
        self.ndk
            .join("toolchains/llvm/prebuilt")
            .join(host)
            .join("bin")
    }

    pub fn clang(&self, abi: Abi) -> PathBuf {
        self.llvm_bin()
            .join(format!("{}{MIN_SDK}-clang", abi.clang_prefix()))
    }

    pub fn clangxx(&self, abi: Abi) -> PathBuf {
        self.llvm_bin()
            .join(format!("{}{MIN_SDK}-clang++", abi.clang_prefix()))
    }
}

fn find_ndk(sdk: &Path) -> Result<PathBuf> {
    let ndk_dir = sdk.join("ndk");
    let pinned = ndk_dir.join(NDK_VERSION);
    if pinned.is_dir() {
        return Ok(pinned);
    }
    let mut versions: Vec<(Vec<u64>, PathBuf)> = std::fs::read_dir(&ndk_dir)
        .with_context(|| {
            format!(
                "no NDK in {}: run `sdkmanager \"ndk;{NDK_VERSION}\"`",
                ndk_dir.display()
            )
        })?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let key = name
                .split('.')
                .map(str::parse)
                .collect::<Result<Vec<u64>, _>>()
                .ok()?;
            Some((key, e.path()))
        })
        .collect();
    versions.sort();
    let (_, path) = versions.pop().with_context(|| {
        format!(
            "no NDK in {}: run `sdkmanager \"ndk;{NDK_VERSION}\"`",
            ndk_dir.display()
        )
    })?;
    eprintln!(
        "warning: NDK {NDK_VERSION} not installed; using {}",
        path.display()
    );
    Ok(path)
}
