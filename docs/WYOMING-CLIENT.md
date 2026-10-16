# Wyoming client protocol — bridge 0.5.0+

Any client can use this interface. Connect to the bridge TCP listener, normally port **10700**, on a trusted LAN. No model, engine, file format for voice profiles, or other backend knowledge is required.

This subset follows the [official Wyoming protocol](https://github.com/OHF-Voice/wyoming#readme). **Synthesis returns PCM to the caller; speaker playback accepts PCM.** Cancel is the only custom operation. `run-pipeline`, microphone input and STT are not supported.

## Discovery

Send `{"type":"describe"}` followed by newline. Read the standard `info` response for available capabilities. It includes `snd` and `satellite`; `tts` is present when synthesis is available. An abbreviated TTS descriptor might look like:

```json
{"type":"info","data":{"tts":[{"name":"tts-service","installed":true,"supports_synthesize_streaming":false,"voices":[{"name":"voice-1","languages":["en"],"installed":true}]}]}}
```

Names here are examples. Select an advertised voice from **`info.tts[0].voices`** and use its `name` exactly as returned. Treat identifiers as opaque strings. Use the descriptor's advertised languages and speakers when selecting those options. There is at most one TTS program, so `select-program` is unnecessary. If `tts` is absent, synthesis is unavailable; PCM speaker playback remains available.

## Generate and play

Use two connections to the same bridge address:

1. **Synthesis connection:** send plain `synthesize` with text and an optional standard voice object:

   ```json
   {"type":"synthesize","data":{"text":"Hello.","voice":{"name":"voice-1","language":"en"},"text_format":"text"}}
   ```

   Receive `audio-start`, one or more `audio-chunk` events containing binary PCM, then `audio-stop`. Optional `voice.speaker` is a string. Omit voice options to use service defaults. Failures use standard `error` events.

2. **Speaker connection:** immediately forward those audio events and payloads unchanged. Do not pace transmission at playback speed: the bridge buffers received audio independently of playback. Wait for `played` after sending `audio-stop` to learn that playback finished. A new `audio-start` replaces the previous speaker item.

This version advertises **`supports_synthesize_streaming:false`**. Send one complete text per `synthesize`; do not send `synthesize-start/chunk/stop`. One request at a time per synthesis connection; sequential requests are supported. Closing that connection abandons its synthesis request.

## Framing and audio

Each event consists of a UTF-8 JSON header followed by newline, then exactly `data_length` bytes of additional JSON data, then exactly `payload_length` bytes of raw payload, if present. Merge additional data into header `data`. Lengths count bytes. PCM is **binary, not base64**.

Forward sample rate, byte width, channels and timestamps from events; do not hardcode an audio format. The speaker accepts signed little-endian PCM16, mono/stereo, 8–192 kHz. Limits per event: header 16 KiB, additional JSON 64 KiB, payload 256 KiB. The default total audio limit is 20 MiB per item; operators may configure different byte/duration limits.

## Cancel — the sole custom extension

On a separate control connection send:

```json
{"type":"user-event","data":{"name":"hifiberry.cancel","data":{}}}
```

Reply acknowledges queue admission, not completion:

```json
{"type":"user-event","data":{"name":"hifiberry.cancel","data":{"accepted":true}}}
```

Cancel globally stops speaker output, discards queued audio and replay media, restores attenuated background volumes, and closes active synthesis connections. It applies to all inputs and clients, not just the sender.

Stop forwarding cancelled audio and close the synthesis connection. To reuse the speaker connection, send `audio-stop` to terminate its cancelled item and wait for `played` before starting another. The speaker discards remaining chunks of the cancelled item until that `audio-stop`; it keeps the connection open.

`user-event` is a standard envelope; **`hifiberry.cancel` and its acknowledgement are project-specific**. Standard `synthesize-stop` ends streaming text and requests final audio; it is never an abort command.

All connections support standard `ping`/`pong`. Keep idle connections alive at least every 30 seconds, handle `error` and EOF, and reconnect when needed. Minimal event set: **describe/info, synthesize, audio-start/chunk/stop, played, ping/pong, error**, plus the custom Cancel user-event.
