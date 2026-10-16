# HiFiBerry TTS Bridge

![HiFiBerry TTS Bridge icon](https://raw.githubusercontent.com/antst/hifiberry-tts/main/custom_components/hifiberry_tts/brand/icon.png)

A Rust announcement player for **HiFiBerryOS-NG**, with a small Home Assistant integration installable through **HACS**. Mix speech with existing PipeWire sources and temporarily reduce selected background streams without changing the shared master volume or HiFiBerry's configuration files.

Three Home Assistant input paths are available:

| Path | HA setup | Audio path |
| --- | --- | --- |
| Direct `tts.speak`, including per-call voice | This HACS integration | Selected Wyoming TTS server → Rust → PipeWire |
| Audio URLs / ordinary TTS URLs | Built-in MPD, bridge port 6601 | HA-generated URL → Rust decoder → PipeWire |
| `assist_satellite.announce` | Built-in Wyoming Protocol, bridge port 10700 | TTS → HA/FFmpeg → Rust PCM → PipeWire |

The Pi runs Rust; the Python files run inside HA. **HACS installs the HA integration only. Install the Rust service on the Pi separately.** Requires bridge 0.3.0+; 0.3.1 is recommended to match HA's streaming text protocol. Tested with HA **2026.10.1**, 64-bit Debian 13 HiFiBerryOS-NG, PipeWire 1.4.2 and WirePlumber 0.5.8. Legacy Buildroot HiFiBerryOS is outside scope.

## Install through HACS

[Open this repository in HACS](https://my.home-assistant.io/redirect/hacs_repository/?owner=antst&repository=hifiberry-tts&category=integration)

1. Open **HACS → ⋮ → Custom repositories**.
2. Add `https://github.com/antst/hifiberry-tts`, category **Integration**.
3. Find **HiFiBerry TTS Bridge** in HACS and download it.
4. Restart **Home Assistant Core**.
5. Open **Settings → Devices & services → Add integration → HiFiBerry TTS Bridge**.
6. Enter the Pi's hostname/IP, **HTTP port 8090**, and its `API_TOKEN` from `~/.config/hifiberry-tts/env`. Choose a name for the new player.

This is a HACS **custom repository**, not a listing in HACS's default catalog. Manual install: extract the `hifiberry_tts.zip` release asset into `/config/custom_components/hifiberry_tts/`, so `manifest.json` is directly in that directory, then restart Core and follow step 5.

The integration creates its own entity. An existing built-in MPD entity is separate. To reuse an old entity ID, rename that MPD entity first, then assign the ID to the new player.

## Speak with a selected voice

Use your actual TTS and **new custom media-player** entity IDs:

```yaml
action: tts.speak
target:
  entity_id: tts.wyoming_tts
data:
  media_player_entity_id: media_player.kitchen_audio
  message: "Hello from the kitchen."
  options:
    voice: YOUR_ADVERTISED_VOICE_ID
  cache: false
```

The adapter reads the selected Wyoming entity's server address and forwards text, voice, language and optional `speaker` to Rust. The TTS server must be reachable **from the Pi** at that address. HA-local loopback/container names may not work. Use the voice ID advertised by Wyoming; for S2 this is a profile name such as `renata-bilingual`, without the native HTTP API's `.s2voice` filename extension.

Non-Wyoming engines, existing TTS stream IDs, and `audio_output` modes fall back to HA's normal audio URL path. Direct mode synthesizes each request even with `cache: true`; it bypasses HA's audio-file cache and output encoding hints. The HA adapter uses internal TTS/Wyoming interfaces tested at 2026.10.1 and may need changes with future HA versions.

In bridge **0.3.1**, direct mode first checks the server's `describe`/`info` response. If streaming text is advertised, it sends the same `synthesize-start` → `synthesize-chunk` → compatibility `synthesize` → `synthesize-stop` sequence as HA. Other servers receive ordinary `synthesize`. All returned PCM chunks feed one output stream until the final `audio-stop`.

## Pi service

See [install, environment and API instructions](docs/BRIDGE.md). Release assets include a Linux ARM64 executable; source is under `src/`. The service uses native PipeWire and Symphonia, with no mpv process. It follows NG's existing default output/EQ path. It adds only its own binary, user unit and environment file.

The HA player's volume controls **speech only** at 0–100% amplitude. Background selection and reduction live in the Pi environment:

```ini
DUCK_TARGETS=*
DUCK_FACTOR=0.25
TTS_VOLUME_LEVEL=1.0
PCM_PREBUFFER_MS=250
PCM_BUFFER_MS=1000
```

`DUCK_TARGETS=*` selects current running source streams; a comma-separated list selects process names. Shared EQ/RIAA and sink/master controls are excluded. New streams appearing after speech starts are not automatically ducked. A hard crash can leave an attenuated stream; graceful stop/restart restores gains while preserving intervening user changes.

## Streaming status

**Direct streaming hiccups are under investigation. Larger buffers have not been established as a fix.** The initial real-audio tests establish mixing and protocol compatibility, not uninterrupted playback from every live TTS engine.

A confirmed request-path difference was corrected in 0.3.1: plain `synthesize` and streaming text can activate different server inference/phrase paths. Server traces also show examples of inference slower than playback and gaps between phrases. That can starve any small prebuffer; it does not by itself explain every perceived difference between direct and HA-mediated playback. HA's URL path waits for a complete file; its satellite path has additional FFmpeg/resampling/read-ahead behavior. Compare identical uncached text, voice and request framing before drawing timing conclusions.

`PCM_PREBUFFER_MS` is audio accumulated before start/recovery, not a fixed timer; `PCM_BUFFER_MS` caps the queue. Status exposes `pcm_underruns` and queued milliseconds. No finite small buffer can compensate indefinitely for synthesis slower than real time. See [known limitations](docs/KNOWN_ISSUES.md) and [test scope](docs/TESTING.md).

This is a community project, not an official HiFiBerry or Home Assistant integration.
