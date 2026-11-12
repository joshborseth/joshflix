//! Git checks that keep releases and GitHub in step.
//!
//! Every APK on the relay must come from a commit that is on GitHub's `main`
//! and tagged `v<version_name>`. `cargo xtask publish` calls [`check_release`]
//! before it touches the relay and [`push_release`] around publishing.

use std::path::Path;
use std::process::Command;
use std::time::SystemTime;

use anyhow::{Context, Result, bail, ensure};

use crate::util::{self, command, workspace_root};

const REMOTE: &str = "origin";
const BRANCH: &str = "main";

fn git() -> Command {
    let mut cmd = command("git");
    cmd.arg("-C").arg(workspace_root());
    cmd
}

fn git_output(args: &[&str]) -> Result<String> {
    Ok(util::output(git().args(args))?.trim().to_owned())
}

fn tag_name(version_name: &str) -> String {
    format!("v{version_name}")
}

/// Fails unless the checkout can be released: on `main`, nothing uncommitted,
/// not behind GitHub, the release tag free, and the APKs built after the last
/// commit.
pub fn check_release(version_name: &str, apks: &[&Path]) -> Result<()> {
    let branch = git_output(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    ensure!(
        branch == BRANCH,
        "publish from `{BRANCH}`, not `{branch}`: merge your branch into {BRANCH} first"
    );

    let dirty = git_output(&["status", "--porcelain"])?;
    ensure!(
        dirty.is_empty(),
        "commit or remove these changes before publishing (keep the notes file outside the checkout); the release must match GitHub:\n{dirty}"
    );

    util::run(git().args(["fetch", "--quiet", "--tags", REMOTE, BRANCH]))?;
    let remote = format!("{REMOTE}/{BRANCH}");
    let up_to_date = git()
        .args(["merge-base", "--is-ancestor", &remote, "HEAD"])
        .status()
        .context("run git merge-base")?
        .success();
    ensure!(
        up_to_date,
        "{remote} has commits this checkout lacks (another release?); \
         run `git pull --rebase {REMOTE} {BRANCH}`, bump android/version.toml again if needed, rebuild, and retry"
    );

    let tag = tag_name(version_name);
    let tag_exists = git()
        .args([
            "rev-parse",
            "--quiet",
            "--verify",
            &format!("refs/tags/{tag}"),
        ])
        .output()
        .context("run git rev-parse")?
        .status
        .success();
    ensure!(
        !tag_exists,
        "tag {tag} already exists; bump android/version.toml"
    );

    let committed: u64 = git_output(&["log", "-1", "--format=%ct"])?
        .parse()
        .context("parse commit time")?;
    for apk in apks {
        let built = std::fs::metadata(apk)
            .and_then(|m| m.modified())
            .with_context(|| format!("stat {}", apk.display()))?
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_secs();
        if built < committed {
            bail!(
                "{} is older than the last commit; rebuild with `cargo xtask apk --abi all` so the APK matches what goes to GitHub",
                apk.display()
            );
        }
    }
    Ok(())
}

/// Pushes `main` to GitHub. Runs before the relay changes, so a release that
/// TVs can download is always on GitHub.
pub fn push_main() -> Result<()> {
    util::run(git().args(["push", REMOTE, &format!("HEAD:refs/heads/{BRANCH}")]))
}

/// Tags the released commit `v<version_name>` and pushes the tag.
pub fn push_tag(version_name: &str) -> Result<()> {
    let tag = tag_name(version_name);
    util::run(git().args(["tag", "-a", &tag, "-m", &format!("Joshflix {version_name}")]))?;
    util::run(git().args(["push", REMOTE, &format!("refs/tags/{tag}")]))
}
