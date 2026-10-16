# HiFiBerry TTS Bridge

![HiFiBerry TTS Bridge icon](https://raw.githubusercontent.com/antst/hifiberry-tts/main/custom_components/hifiberry_tts/brand/icon.png)

A Rust announcement player for **HiFiBerryOS-NG**, with a small Home Assistant integration installable through **HACS**. Mix speech with existing PipeWire sources and temporarily reduce selected background streams without changing the shared master volume or HiFiBerry's configuration files.

Three Home Assistant input paths are available:

| Path | HA setup | Audio path |
| --- | --- | --- |
| Direct `tts.speak`, including per-call voice | This HACS integration | Selected Wyoming TTS server → Rust → PipeWire |
| Audio URLs / ordinary TTS URLs | Built-in MPD, bridge port 6601 | HA-generated URL → Rust decoder → PipeWire |
| `assist_satellite.announce` | Built-in Wyoming Protocol, bridge port 10700 | TTS → HA/FFmpeg → Rust PCM → PipeWire |

The Pi runs Rust; the Python files run inside HA. **HACS installs the HA integration only. Install the Rust service on the Pi separately.** Basic controls require bridge 0.3.0+; receive-ahead in RAM and the Cancel speech button require 0.5.0+. Tested with HA **2026.10.1**, 64-bit Debian 13 HiFiBerryOS-NG, PipeWire 1.4.2 and WirePlumber 0.5.8. Legacy Buildroot HiFiBerryOS is outside scope.

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
# Optional for bursty/slow live PCM:
PCM_BUFFERING_MODE=adaptive
```

`DUCK_TARGETS=*` selects current running source streams; a comma-separated list selects process names. Shared EQ/RIAA and sink/master controls are excluded. New streams appearing after speech starts are not automatically ducked. A hard crash can leave an attenuated stream; graceful stop/restart restores gains while preserving intervening user changes.

## Streaming status

**Adaptive buffering trades startup delay for continuous playback when synthesis is slow.** Enable it with `PCM_BUFFERING_MODE=adaptive`; the default `fixed` retains the earlier bounded live queue.

A confirmed request-path difference was corrected in 0.3.1: plain `synthesize` and streaming text can activate different server inference/phrase paths. Server traces also show examples of inference slower than playback and gaps between phrases. That can starve any small prebuffer; it does not by itself explain every perceived difference between direct and HA-mediated playback. HA's URL path waits for a complete file; its satellite path has additional FFmpeg/resampling/read-ahead behavior. Compare identical uncached text, voice and request framing before drawing timing conclusions.

Adaptive mode estimates audio arrival speed and grows its startup reserve. A source observed below real time before playback is held until completion. If playback later starves, it waits for the complete remaining speech, allowing at most one PCM starvation/recovery per item. A bounded raw PCM queue in RAM keeps receiving independently of playback. Consumed chunks are freed; the TTS connection closes on reception completion while queued speech continues playing. This does not accelerate inference or prevent unrelated output glitches.

`PCM_PREBUFFER_MS` is accumulated audio, not a fixed timer. `PCM_BUFFER_MS` caps the live queue in fixed mode and decoder read-ahead in adaptive mode. `MAX_AUDIO_BYTES` caps accepted PCM per item and bounds raw receive memory. The default 20 MiB accommodates almost four minutes of 44100 Hz mono PCM16; 30 seconds uses 2.65 MB. Large receive capacity does not require filling it before starting. Status exposes rate, buffering target, completion wait and underruns. See [configuration and tradeoffs](docs/BRIDGE.md#adaptive-pcm-buffering), [known limitations](docs/KNOWN_ISSUES.md) and [test scope](docs/TESTING.md).

## Cancel speech

The HACS integration adds a **Cancel speech** button to the bridge device. It stops current output, discards received PCM and cached replay media, closes direct TTS connections, and restores ducked background gains. It applies to direct TTS, MPD/URL playback and satellite announcements. It also cancels a satellite waiting for its first PCM chunk. Use the actual button entity ID in your HA actions:

```yaml
action: button.press
target:
  entity_id: button.kitchen_audio_cancel_speech
```

This uses the bridge's authenticated `POST /v1/cancel` endpoint. It does not depend on a nonstandard Wyoming event. Direct upstream computation cancellation depends on the TTS gateway honoring disconnect; HA owns upstream generation for satellite/URL paths. The standard media-player Stop control also stops audio and clears live PCM, while retaining replay metadata. Cancel discards that metadata as well.

For other clients, see [audio and text inputs](docs/WYOMING-CLIENT.md): Wyoming receives PCM for playback; authenticated HTTP `/v1/speak` accepts text for fire-and-forget local speech. Satellite cancellation discards the remaining incoming audio while preserving HA's connection.

This is a community project, not an official HiFiBerry or Home Assistant integration.
