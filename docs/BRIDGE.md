# Install the Rust bridge on HiFiBerryOS-NG

Use **64-bit Debian-based HiFiBerryOS-NG**, logged in as the audio user recorded in `/etc/hifiberry.user`. This does not cover legacy Buildroot releases. The service connects to the existing user PipeWire session; no vendor audio configuration changes or player registration are required.

## Install

Download the source archive and `hifiberry-tts-linux-aarch64` executable from the same [release](https://github.com/antst/hifiberry-tts/releases). Extract the source archive and run from its directory, as the normal audio user:

```sh
sudo apt-get install -y --no-install-recommends libpipewire-0.3-0 pipewire-bin openssl
pw-dump > /dev/null
sh scripts/install-user.sh /path/to/hifiberry-tts-linux-aarch64
systemctl --user status hifiberry-tts
```

The installer adds `~/.local/bin/hifiberry-tts`, `~/.config/hifiberry-tts/env` and `~/.config/systemd/user/hifiberry-tts.service`, generates an API token on first install, preserves the environment on upgrades, and enables/restarts this service. It restarts only the bridge. For boot startup:

```sh
loginctl show-user "$(id -un)" -p Linger
# If it is not already enabled:
sudo loginctl enable-linger "$(id -un)"
```

A headless session may need `export XDG_RUNTIME_DIR="/run/user/$(id -u)"`. The optional read-only `sh scripts/check-ng.sh` diagnostic uses Python 3 to inspect actual PipeWire streams. The Rust runtime itself does not require Python.

## Configure

Edit **the bridge's own** `~/.config/hifiberry-tts/env` (mode 0600), using [deploy/env.example](../deploy/env.example) as reference. It is an environment file, not a shell script. Restart the bridge after changes:

```sh
systemctl --user restart hifiberry-tts
```

| Setting | Default / example | Meaning |
| --- | --- | --- |
| `BIND_ADDR` | `127.0.0.1:8090` | Authenticated HTTP API; use `0.0.0.0:8090` for HA on another host. |
| `MPD_BIND_ADDR` | `127.0.0.1:6601` | MPD-compatible interface; empty disables it. NG's own port 6600 stays separate. |
| `WYOMING_BIND_ADDR` | `127.0.0.1:10700` | Announcement-only satellite; empty disables it. |
| `API_TOKEN` | Generated on install | HTTP bearer token and MPD password. |
| `WYOMING_TTS_ADDR` | `TTS_HOST:10200` | TTS server for plain/JSON MPD text requests. Normal `tts.speak` through the custom integration supplies the selected engine's address instead. |
| `TTS_VOICE` | Empty | Default **advertised Wyoming voice ID** for MPD text. |
| `DUCK_TARGETS` | `*` | Current running playback source streams, or exact process names separated by commas. |
| `DUCK_FACTOR` | `0.25` | Multiplier of background linear amplitude; about −12 dB. `1` disables ducking. |
| `TTS_VOLUME_LEVEL` | `1.0` | Initial speech amplitude, 0–1; 1 is unity, no preamp/normalization. |
| `PCM_BUFFERING_MODE` | `fixed` | `fixed` uses a bounded live queue; `adaptive` estimates arrival speed and can wait for completion. See below. |
| `PCM_PREBUFFER_MS` | `250` | Audio accumulated before startup/recovery. Adaptive mode may increase this target or wait for completion. |
| `PCM_BUFFER_MS` | `1000` | 10–5000 ms, at least the prebuffer. Fixed mode: queue cap/backpressure. Adaptive mode: decoder read-ahead cap; disk spool has a separate byte cap. |
| `FORCE_PROTO` | Empty / `http` / `https` | Force all incoming media URL schemes when explicitly needed. Path, query and non-default port are preserved. |
| `MEDIA_HOSTS` | Empty / `homeassistant.local,HA_IP` | Exact allowed HTTP(S) hosts; empty allows any reachable host. |
| `MAX_AUDIO_BYTES` | `20971520` | Download/PCM byte cap. |
| `MAX_PLAY_SECONDS` | `300` | Playback wall-clock limit, including pauses. |
| `HTTP_DOWNLOAD_TIMEOUT_SECONDS` | `120` | Whole HTTP audio download, including fresh HA TTS generation. Must be positive. |
| `PIPEWIRE_TARGET` | Empty | Optional existing `node.name`; empty follows NG's current default. |

The supplied install example binds listeners to LAN interfaces. HTTP/MPD use the token, but plain HTTP and MPD do not encrypt it. Wyoming has no authentication/encryption. Use a trusted LAN or restrict access to HA. HiFiBerry's nginx authentication does not protect these separate listeners.

HTTP downloads have a 5-second connection timeout, a configurable 120-second total timeout and no redirects. Earlier versions used a hardcoded 20-second total limit, which could expire while HA was still generating speech. Self-signed/private-CA HTTPS needs appropriate trust/proxy support; this build uses public certificate roots. `FORCE_PROTO=http` is an explicit scheme override, not an automatic TLS downgrade.

## Adaptive PCM buffering

Set `PCM_BUFFERING_MODE=adaptive` in the bridge environment to apply the same buffering policy to direct TTS and satellite PCM. This is optional; `fixed` preserves the earlier queue behavior.

The adaptive receiver writes PCM to a private, unlinked temporary file independently of playback. `MAX_AUDIO_BYTES` bounds the complete spool; `PCM_BUFFER_MS` bounds decoder read-ahead in memory. Reception therefore continues while playback waits, without filling a paused queue and blocking the next phrase upstream. Stop/replacement cancels the receiver and discards its spool.

The rate estimate divides received audio duration by elapsed arrival time, excluding the wait for the first chunk. It begins after at least 250 ms between arrivals, and includes subsequent inference and transport gaps. The startup target grows with observed gaps. If the observed source is below 1.05 times playback speed before speech starts, the receiver waits for the final `audio-stop`. If playback starts and later starves, it pauses once and waits for the complete remaining tail. Completed short clips drain even below the initial threshold.

This avoids repeated **PCM starvation** by accepting a possible long initial wait or one long recovery pause. It cannot prevent unrelated PipeWire xruns, repair missing upstream audio, or predict total speech duration from an early burst. For an unknown-length source below real time, waiting for completion is the conservative fallback. Background ducking begins when output opens, after adaptive startup buffering; it remains applied during an active recovery pause. Pauses and buffering remain subject to the service's time/size limits.

`GET /v1/status` and the custom HA player's attributes expose `pcm_buffering_mode`, `pcm_source_rate` (audio seconds per arrival second), `pcm_target_ms`, `pcm_waiting_for_complete`, queued audio and underruns. The spool can exceed `pcm_buffer_limit_ms` in adaptive mode; that field describes the memory/read-ahead limit.

The policy follows the rate-estimation and completion-fallback principles used in [GStreamer's download buffering](https://gstreamer.freedesktop.org/documentation/application-development/advanced/buffering.html); GStreamer is not a runtime dependency.

## Home Assistant alternatives without a custom integration

For the built-in **Music Player Daemon** integration, use the Pi's IP, bridge port **6601**, and `API_TOKEN` as password. Its normal `tts.speak` resolves an audio URL; the Rust bridge downloads the complete file and then plays it. `media_player.play_media` accepts HTTP(S) URLs. Play/resume/replay, pause, stop, speech volume/mute and file seek work.

MPD text mode is this project's convention: `media_content_type: text`, with plain text or JSON `{"text":"Hello","voice":"VOICE_ID"}` in `media_content_id`. The Pi's `WYOMING_TTS_ADDR` chooses the engine. MPD does not forward HA `extra` voice options. Use the HACS integration for normal `tts.speak` text with the selected HA engine and per-call voice.

For the built-in **Wyoming Protocol** integration, use the Pi IP and bridge port **10700**. No mDNS discovery is advertised. Use the generated entity:

```yaml
action: assist_satellite.announce
target:
  entity_id: assist_satellite.kitchen_audio
data:
  message: "Someone is at the front door."
  preannounce: false
```

The satellite's selected **Assist pipeline** supplies engine and voice; `assist_satellite.announce` has no per-call voice field. Select a pipeline configured with a usable TTS engine in the device settings. This satellite has no microphone, STT, wake word or conversation input.

HA resolves the satellite audio and runs FFmpeg before sending PCM. **HA's local URL must have the correct HTTP/HTTPS scheme.** A wrong HTTPS URL for a plain HTTP endpoint can silently produce no audio and block the satellite setup test. The bridge's `FORCE_PROTO` cannot correct HA's own FFmpeg fetch. This path is TTS → HA → bridge; it does not delegate a connection from TTS directly to the speaker.

## API and logs

These are this project's endpoints, not pre-existing HiFiBerry APIs. All require `Authorization: Bearer <API_TOKEN>` on port 8090.

| Method / path | Body / purpose |
| --- | --- |
| `GET /v1/status` | Playback/error state, speech gain/position/duration, ducking warnings, queued PCM and underruns. |
| `POST /v1/play_media` | `url`; optional `volume_level`, `duck_factor`, `duck_targets`. |
| `POST /v1/speak` | `text`; optional `voice`, `language`, `speaker`, `server` (Wyoming host:port). |
| `POST /v1/play` | Resume/replay a file or re-synthesize the last direct request. |
| `POST /v1/pause` | `paused`: boolean. |
| `POST /v1/stop` | Stop/cancel speech and restore background gains. |
| `POST /v1/volume` | `volume_level`: 0–1, speech only. |
| `POST /v1/seek` | `position`: seconds, downloaded files only. |

Commands return 202 when queued; poll status for completion/errors. Media URL paths/query strings and spoken text are not logged. On systems where `journalctl --user` has no entries:

```sh
sudo journalctl _SYSTEMD_USER_UNIT=hifiberry-tts.service -f
```

## Build and remove

Rust 1.88+ and Linux build dependencies:

```sh
sudo apt-get install -y build-essential pkg-config libclang-dev libpipewire-0.3-dev
cargo build --release --locked
sh scripts/install-user.sh target/release/hifiberry-tts
```

A macOS executable cannot run on the Pi. [deploy/Dockerfile.build](../deploy/Dockerfile.build) describes the ARM64 release build environment; select the appropriate container platform when building on another architecture.

To remove:

```sh
systemctl --user disable --now hifiberry-tts.service
rm ~/.config/systemd/user/hifiberry-tts.service
rm ~/.local/bin/hifiberry-tts
rm ~/.config/hifiberry-tts/env
rmdir ~/.config/hifiberry-tts
systemctl --user daemon-reload
```

Remove the corresponding HA entries and optional HACS integration. No vendor config needs restoring. Backup binaries/environment files created manually during upgrades can be removed separately.
