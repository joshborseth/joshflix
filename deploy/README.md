# Joshflix relay

`joshflix-relay` is a small private server on the dev box. It does four jobs for the TV app:

- **Real-Debrid pairing.** The TV shows a QR code. The phone opens the relay's page, pastes its Real-Debrid API key, and the relay checks the key and hands it to the TV once. The key stays in memory only and is forgotten after delivery.
- **App updates.** It serves `updates/manifest.json` and the APKs that `cargo xtask publish` writes into the data directory.
- **TV logs.** TVs usually have no adb, so the app posts its log lines every few seconds. The relay appends them to `logs/tv.log` in the data directory, and rotates it to `tv.log.1` past 8 MB. The app removes the Real-Debrid key, and the Comet link segment that contains it, before sending.

- **Screen colors.** While a video plays, the TV posts its title, whether it plays, and (while someone listens) a 48×27 copy of the picture about once a second to `POST /api/v1/screen`. The relay finds the one color the picture reads as (k-means in OKLab, the most prominent colorful cluster) and streams it on the read-only WebSocket `/api/v1/screen/feed`, which the lights app's automations service listens to. Frames look like `{"_tag":"ScreenColors","playing":true,"title":"Dune","color":"#c8742f"}`; `color` is null for a dark or grey picture. A new color goes out only after the picture's color has clearly changed and held for 1.5 seconds, and at most every 2 seconds. A TV silent for 20 seconds counts as stopped. Nothing is written to disk.

It answers only on the home LAN (`192.168.1.67`) and the tailnet (`100.107.121.43`). Requests from any other address get 403. Do not port-forward it, open firewall ports for it, or put it behind Tailscale Funnel or serve.

## Endpoints

The wire contract lives in `crates/joshflix-protocol/src/lib.rs`. Quick checks:

```sh
curl http://192.168.1.67:8788/healthz                    # ok
curl -X POST http://192.168.1.67:8788/api/v1/pairings    # create a pairing
curl http://192.168.1.67:8788/api/v1/updates/latest      # current manifest, 404 if none
websocat ws://127.0.0.1:8788/api/v1/screen/feed          # screen colors, one JSON frame per change
```

## Environment

| Variable | Default | Meaning |
| --- | --- | --- |
| `JOSHFLIX_RELAY_BIND` | `0.0.0.0:8788` | Listen address. IPv4 only, because the dev box's IPv6 address is public. Port 8787 is taken by another service. |
| `JOSHFLIX_RELAY_DATA` | `~/.local/share/joshflix-relay` | Data directory. Updates go in `updates/`. |
| `JOSHFLIX_RELAY_PUBLIC_URL` | unset | Base URL put in the QR code, such as `http://192.168.1.67:8788`. When unset, the relay uses the `Host` header of the TV's request. |
| `JOSHFLIX_RD_API_BASE` | `https://api.real-debrid.com/rest/1.0` | Real-Debrid API base. Override only for testing. |
| `RUST_LOG` | `info` | Log filter. |

## Install or update

Run from the repository root as `slammer`:

```sh
CARGO_BUILD_JOBS=6 cargo build --release -p joshflix-relay
install -m 0755 target/release/joshflix-relay ~/.local/bin/joshflix-relay
install -m 0644 deploy/joshflix-relay.service ~/.config/systemd/user/joshflix-relay.service
systemctl --user daemon-reload
systemctl --user enable --now joshflix-relay    # first install
systemctl --user restart joshflix-relay         # after an update
```

Lingering must be on so the service keeps running after logout:

```sh
loginctl show-user slammer -p Linger            # expect Linger=yes
sudo loginctl enable-linger slammer             # if it is off
```

Restarting drops any pairing in progress. The TV then shows an expired code and makes a new one.

## Logs

```sh
systemctl --user status joshflix-relay
journalctl --user -u joshflix-relay -f
tail -f ~/.local/share/joshflix-relay/logs/tv.log   # what the TVs report
```

Logs never include API keys, TV secrets, or full pairing codes. A code appears only as its first four characters.
