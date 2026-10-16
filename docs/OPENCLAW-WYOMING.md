# OpenClaw Wyoming subset — bridge 0.5.0+

Connect to the bridge TCP listener, normally port **10700**, on a trusted LAN. The bridge queries its configured `WYOMING_TTS_ADDR`; clients need only the bridge address.

This follows the [official Wyoming protocol](https://github.com/OHF-Voice/wyoming#readme). **Synthesis returns PCM to the caller; speaker playback accepts PCM.** The only custom operation is Cancel below. `run-pipeline`, microphone input and STT are not implemented.

## Discover voices

Send `{"type":"describe"}` followed by newline. The standard `info` response includes `snd`, `satellite`, and, when upstream discovery succeeds, `tts`:

```json
{"type":"info","data":{"tts":[{"name":"hifiberry-tts-proxy","installed":true,"supports_synthesize_streaming":false,"voices":[{"name":"sarah-bilingual","languages":["en","ru"],"installed":true}]}]}}
```

This is an abbreviated example; real descriptors include attribution and preserve upstream voice metadata. Read **`info.tts[0].voices`** and use the exact voice name, without a `.s2voice` suffix. The bridge refreshes discovery on each `describe`. If upstream is unavailable/unconfigured, `tts` is omitted; speaker discovery still succeeds. There is one proxied TTS program, so `select-program` is unnecessary.

## Generate and play

Use two connections to the same bridge address:

1. **TTS connection:** send plain `synthesize` with text and optional standard voice object:

   ```json
   {"type":"synthesize","data":{"text":"Hello from the kitchen.","voice":{"name":"sarah-bilingual","language":"en"},"text_format":"text"}}
   ```

   Receive `audio-start`, one or more `audio-chunk` events with binary PCM, then `audio-stop`. Optional `voice.speaker` is a string. Omitted fields are forwarded unchanged to the upstream engine; the proxy does not substitute the bridge's MPD/direct-player default voice. Upstream errors use standard `error` events.

2. **Speaker connection:** immediately forward those audio events and payloads unchanged. Do not pace transmission at playback speed: keep draining TTS so the bridge can receive ahead and release inference early. Wait for `played` after sending `audio-stop` to learn that playback finished. A new `audio-start` replaces the previous speaker item.

The proxy advertises **`supports_synthesize_streaming:false`**: do not send `synthesize-start/chunk/stop` to it. One plain request at a time per TTS connection; sequential requests are supported. Closing this connection cancels its upstream request. The bridge's separate internal direct-player client can negotiate streaming with upstream, but that does not change this proxy's advertised subset.

Wyoming framing is a UTF-8 JSON header plus newline, then exactly `data_length` bytes of additional JSON data, then exactly `payload_length` bytes of raw payload, if present. Merge additional data into header `data`. Byte lengths count UTF-8 bytes. PCM is **binary, not base64**. Forward sample rate, byte width, channels and timestamps from events; do not hardcode a format. The speaker accepts signed little-endian PCM16, mono/stereo, 8–192 kHz. Limits: header 16 KiB, additional JSON 64 KiB, event payload 256 KiB; total audio uses bridge byte/duration limits.

## Cancel — the sole custom extension

On a separate control connection send:

```json
{"type":"user-event","data":{"name":"hifiberry.cancel","data":{}}}
```

Reply acknowledges queue admission, not completion:

```json
{"type":"user-event","data":{"name":"hifiberry.cancel","data":{"accepted":true}}}
```

This globally stops current speaker output, frees queued PCM/cache, restores ducked gains, and closes active direct/proxied TTS requests. It also applies to HA/MPD inputs. Stop forwarding cancelled audio; close the producer connection as well. A cancelled speaker connection discards its remaining old audio until `audio-stop`, replies `played`, and remains usable for the next utterance. Cancellation of inference behind HA is not guaranteed: HA owns that upstream request. Direct disconnect abort depends on the configured TTS server honoring it.

`user-event` is a standard envelope; **`hifiberry.cancel` and its acknowledgement are project-specific**. `synthesize-stop` ends streaming text and requests final audio; it is never an abort command.

Both connections support standard `ping`/`pong`. Keep idle connections alive at least every 30 seconds, handle `error` and EOF, and reconnect when needed. Minimal event set: **describe/info, synthesize, audio-start/chunk/stop, played, ping/pong, error**, plus the one custom Cancel user-event.
