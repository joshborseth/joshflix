//! `install`, `run` and `logcat` over adb.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::apk::{self, ApkOptions};
use crate::args::Args;
use crate::sdk::{Abi, Sdk};
use crate::util;
use crate::version::Version;

pub const PACKAGE: &str = "com.joshflix.tv";
pub const ACTIVITY: &str = "android.app.NativeActivity";

pub struct DeviceOptions {
    serial: Option<String>,
    debug: bool,
}

impl DeviceOptions {
    pub fn parse(args: &mut Args) -> Result<Self> {
        Ok(Self {
            serial: args.value("--serial")?,
            debug: args.flag("--debug"),
        })
    }
}

struct Adb {
    path: PathBuf,
    serial: Option<String>,
}

impl Adb {
    fn new(opts: &DeviceOptions) -> Result<Self> {
        Ok(Self {
            path: Sdk::locate()?.adb()?,
            serial: opts.serial.clone(),
        })
    }

    fn cmd(&self) -> Command {
        let mut cmd = util::command(&self.path);
        if let Some(serial) = &self.serial {
            cmd.args(["-s", serial]);
        }
        cmd
    }

    /// The device's ABIs in preference order.
    fn abis(&self) -> Result<Vec<String>> {
        let mut list =
            util::output(
                self.cmd()
                    .args(["shell", "getprop", "ro.product.cpu.abilist"]),
            )?;
        if list.trim().is_empty() {
            list = util::output(self.cmd().args(["shell", "getprop", "ro.product.cpu.abi"]))?;
        }
        let abis: Vec<String> = list
            .trim()
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        if abis.is_empty() {
            bail!("could not read the device ABI list");
        }
        Ok(abis)
    }

    fn pid(&self) -> Option<String> {
        let out = self
            .cmd()
            .args(["shell", "pidof", "-s", PACKAGE])
            .stderr(Stdio::null())
            .output()
            .ok()?;
        let pid = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        (out.status.success() && !pid.is_empty()).then_some(pid)
    }
}

/// The first device ABI we build for.
fn device_abi(adb: &Adb) -> Result<Abi> {
    let abis = adb.abis()?;
    abis.iter()
        .find_map(|name| Abi::from_android_name(name))
        .ok_or_else(|| anyhow::anyhow!("device ABIs {abis:?} are not supported"))
}

pub fn install(opts: &DeviceOptions) -> Result<()> {
    let adb = Adb::new(opts)?;
    install_with(&adb, opts)
}

fn install_with(adb: &Adb, opts: &DeviceOptions) -> Result<()> {
    let version = Version::load()?;
    let abis = adb.abis()?;
    let candidates: Vec<(Abi, PathBuf)> = abis
        .iter()
        .filter_map(|name| Abi::from_android_name(name))
        .map(|abi| (abi, apk::apk_path(&version, abi, opts.debug)))
        .collect();
    let Some((_, apk)) = candidates.iter().find(|(_, path)| path.is_file()) else {
        let debug = if opts.debug { " --debug" } else { "" };
        match candidates.first() {
            Some((abi, path)) => bail!(
                "{} not found; build it with `cargo xtask apk --abi {}{debug}`",
                path.display(),
                abi.android_name()
            ),
            None => bail!("device ABIs {abis:?} are not supported"),
        }
    };
    util::run(adb.cmd().args(["install", "-r"]).arg(apk))
}

pub fn run(opts: &DeviceOptions, build: bool, follow_logs: bool) -> Result<()> {
    let adb = Adb::new(opts)?;
    if build {
        let abi = device_abi(&adb)?;
        apk::build(&ApkOptions {
            abis: vec![abi],
            debug: opts.debug,
            crate_manifest: None,
        })?;
    }
    install_with(&adb, opts)?;
    util::run(adb.cmd().args([
        "shell",
        "am",
        "start",
        "-n",
        &format!("{PACKAGE}/{ACTIVITY}"),
    ]))?;
    if follow_logs {
        logcat_with(&adb)?;
    }
    Ok(())
}

pub fn logcat(opts: &DeviceOptions) -> Result<()> {
    logcat_with(&Adb::new(opts)?)
}

fn logcat_with(adb: &Adb) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    let pid = loop {
        if let Some(pid) = adb.pid() {
            break pid;
        }
        if Instant::now() > deadline {
            bail!("{PACKAGE} is not running; start it with `cargo xtask run`");
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    eprintln!("Following logcat for {PACKAGE} (pid {pid}); Ctrl-C to stop");
    util::run(
        adb.cmd()
            .args(["logcat", "-v", "color", &format!("--pid={pid}")]),
    )
}
