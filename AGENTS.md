# Joshflix agent notes

Read [README.md](README.md) and [android/README.md](android/README.md) first.

## Releasing

A release is done only when all of these are true. Check each one before you report a release as finished.

1. The change is merged into local `main` (in the main checkout, `~/.t3/projects/joshflix`).
2. `main` is pushed to GitHub (`origin/main` matches local `main`).
3. The release commit is tagged `v<version_name>` on GitHub.
4. The relay manifest (`~/.local/share/joshflix-relay/updates/manifest.json`) lists the new version.
5. The TV runs it: `~/.local/share/joshflix-relay/logs/tv.log` shows `Joshflix <version_name>` in its newest `====` header.

`cargo xtask publish` enforces 1 to 3. It refuses to run off `main`, with uncommitted or untracked files, behind `origin/main`, or with APKs older than the last commit. It pushes `main` before it touches the relay and pushes the tag after. Never work around those checks by publishing from a branch or by copying APKs into the relay by hand.

Several threads release in parallel. Right before you build:

- Run `git pull --rebase origin main` in the main checkout.
- Check the relay manifest's `version_code`, and bump `android/version.toml` above it.

Build and publish from the main checkout. Its `.local/` holds the release keystore, and `cargo xtask apk` without it silently makes a new key whose APKs can't update the installed app.

## TV performance

The TV log has a `frames:` line for every 10 s of animation (late frames, missed vsyncs, render+swap ms). The TV runs at 60 Hz, so a frame has 16.7 ms. Read those lines before and after any animation or scrolling change.

Each `frames:` line also splits the UI thread's frame time three ways: `cpu` (running on a core), `waiting for a core` (runnable while other threads had every core), and `blocked` (asleep, mostly inside the GPU driver). It also gives `gpu`, the GPU's time for the UI from GL timer queries. A frame is GPU-bound when `gpu` is near 16.7 ms. It is CPU-bound when `cpu` is near 16.7 ms. Contention from artwork decoding shows up as `waiting for a core`. The `GPU:` line at startup names the GPU and says whether timer queries are available. Video playback gets `playback:` lines instead, because those frames follow the film's frame rate.
