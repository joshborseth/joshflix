//! Joshflix build tasks: `cargo xtask <command>`.
//!
//! Packages the Rust TV app into a signed Android APK without Gradle, installs
//! it over adb, and publishes releases to the relay's update directory.
//! Run `cargo xtask help` for the command list.

mod apk;
mod args;
mod device;
mod icons;
mod libmpv;
mod publish;
mod sdk;
mod util;
mod version;

use anyhow::{Result, bail};

use crate::args::Args;

const USAGE: &str = "\
Usage: cargo xtask <command> [options]

Commands:
  fetch-libmpv                 Download, verify and extract the prebuilt libmpv into .local/libmpv/
  apk [--abi A] [--debug]      Build signed APKs into dist/ (A: arm, arm64, x86_64 or all; default all)
  install [--serial S] [--debug]
                               adb install -r the dist APK that matches the device ABI
  run [--serial S] [--debug] [--build] [--logcat]
                               Install (building first with --build), launch, and optionally follow logs
  logcat [--serial S]          Follow logcat for the running app's pid
  icons                        Rasterize assets/*.svg into android/res/ and assets/icon-512.png
  publish [--notes FILE]       Copy the release APKs into the relay's updates/ dir and write manifest.json
  help                         Show this text

Environment:
  ANDROID_HOME / ANDROID_SDK_ROOT   SDK root (default ~/Android/Sdk)
  ANDROID_NDK_HOME                  NDK root (default <sdk>/ndk/27.2.12479018)
  JOSHFLIX_RELAY_DATA               Relay data dir for publish (default ~/.local/share/joshflix-relay)
  CARGO_BUILD_JOBS                  Parallel rustc jobs (default 6)
";

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = Args::from_env();
    let Some(command) = args.command() else {
        print!("{USAGE}");
        return Ok(());
    };
    match command.as_str() {
        "fetch-libmpv" => {
            args.finish()?;
            libmpv::fetch()?;
        }
        "apk" => {
            let opts = apk::ApkOptions::parse(&mut args)?;
            args.finish()?;
            apk::build(&opts)?;
        }
        "install" => {
            let opts = device::DeviceOptions::parse(&mut args)?;
            args.finish()?;
            device::install(&opts)?;
        }
        "run" => {
            let opts = device::DeviceOptions::parse(&mut args)?;
            let build = args.flag("--build");
            let logcat = args.flag("--logcat");
            args.finish()?;
            device::run(&opts, build, logcat)?;
        }
        "logcat" => {
            let opts = device::DeviceOptions::parse(&mut args)?;
            args.finish()?;
            device::logcat(&opts)?;
        }
        "icons" => {
            args.finish()?;
            icons::render()?;
        }
        "publish" => {
            let notes = args.value("--notes")?;
            args.finish()?;
            publish::publish(notes.as_deref())?;
        }
        "help" | "-h" | "--help" => print!("{USAGE}"),
        other => bail!("unknown command `{other}`\n\n{USAGE}"),
    }
    Ok(())
}
