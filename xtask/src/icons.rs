//! `cargo xtask icons`: rasterize `assets/*.svg` into the Android resources.
//! The PNGs are committed; rerun this after editing the SVGs.

use anyhow::Result;

use crate::util::{self, command, workspace_root};

/// Launcher icon sizes per density (48 dp).
const LAUNCHER: [(&str, u32); 5] = [
    ("mdpi", 48),
    ("hdpi", 72),
    ("xhdpi", 96),
    ("xxhdpi", 144),
    ("xxxhdpi", 192),
];

pub fn render() -> Result<()> {
    let root = workspace_root();
    let assets = root.join("assets");
    let res = root.join("android/res");
    let svg =
        |cmd: &mut std::process::Command, size: (u32, u32), src: &str, dst: &std::path::Path| {
            if let Some(dir) = dst.parent() {
                std::fs::create_dir_all(dir)?;
            }
            util::run(
                cmd.args(["-w", &size.0.to_string(), "-h", &size.1.to_string(), "-o"])
                    .arg(dst)
                    .arg(assets.join(src)),
            )
        };
    for (density, px) in LAUNCHER {
        let dir = res.join(format!("mipmap-{density}"));
        svg(
            &mut command("rsvg-convert"),
            (px, px),
            "icon.svg",
            &dir.join("ic_launcher.png"),
        )?;
        // Adaptive-icon foreground: 108 dp canvas.
        let fg = px * 108 / 48;
        svg(
            &mut command("rsvg-convert"),
            (fg, fg),
            "icon-foreground.svg",
            &dir.join("ic_launcher_foreground.png"),
        )?;
    }
    // Android TV wants a 320x180 px banner in drawable-xhdpi.
    svg(
        &mut command("rsvg-convert"),
        (320, 180),
        "banner.svg",
        &res.join("drawable-xhdpi/banner.png"),
    )?;
    svg(
        &mut command("rsvg-convert"),
        (512, 512),
        "icon.svg",
        &assets.join("icon-512.png"),
    )?;
    Ok(())
}
