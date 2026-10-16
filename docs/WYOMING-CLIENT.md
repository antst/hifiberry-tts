# Speech bridge client inputs — bridge 0.5.0+

There are two independent ways to make the bridge play speech:

| Client has | Client sends | Interface |
| --- | --- | --- |
| Audio | PCM events and bytes | Wyoming TCP, normally port 10700 |
| Text | Text and optional voice selection, then finishes the request | HTTP `POST /v1/speak`, normally port 8090 |

The client needs the bridge address and, for HTTP, its API token. No model or engine knowledge is required. The bridge handles buffering and speaker output in both cases.

**In 0.5.0, fire-and-forget text playback is an HTTP operation. There is no implemented Wyoming text-only local-playback request.** Wyoming `synthesize` belongs to a separate [PCM synthesis service](WYOMING-TTS-PROXY.md); it returns audio to its caller and does not play it locally.

## Case 1: client sends audio

Connect to the bridge's Wyoming listener. Send:

1. `audio-start` with `rate`, `width` and `channels`.
2. One or more `audio-chunk` events, each with those format fields and a binary PCM payload.
3. `audio-stop` after the last chunk.

The bridge starts playback when its startup buffer is ready and continues receiving ahead independently. Send available audio promptly; do not pace transmission at playback speed. No text or voice selection is needed because the client already has audio.

Receive `played` when output finishes. `audio-stop` marks the end of input; it does not mean output has finished. Keep the connection open through `played`. A new `audio-start` replaces the previous speaker item.

Audio is signed little-endian PCM16, mono/stereo, 8–192 kHz. Use the format of the audio you send. Wyoming framing is a JSON header followed by newline, then any additional JSON bytes specified by `data_length`, then raw payload bytes specified by `payload_length`. Merge additional JSON into header `data`; lengths count bytes. PCM is binary, not base64. Per-event limits: header 16 KiB, additional JSON 64 KiB, payload 256 KiB. The speaker's default accepted audio limit is 20 MiB per item; operators may configure different byte/duration limits.

## Case 2: client sends text, fire and forget

Send one authenticated HTTP request to the bridge:

```http
POST /v1/speak
Authorization: Bearer <API_TOKEN>
Content-Type: application/json

{"text":"Hello.","voice":"<advertised-voice-id>","language":"en"}
```

`text` is required. `voice`, `language` and `speaker` are optional strings. Use an advertised voice ID when selecting one; omit these fields to use the synthesis service's defaults. General clients use the bridge's configured synthesis service and need not supply its address.

The bridge queues the request and replies:

```http
HTTP/1.1 202 Accepted
Content-Type: application/json

{"accepted":true}
```

After receiving this acknowledgement, the client can disconnect. The bridge independently requests synthesis, receives audio, buffers it and plays it. **The client never receives or relays PCM.** A later text request replaces the current item.

`202` acknowledges queue admission, not successful synthesis or playback completion. Non-success HTTP responses indicate rejection. This interface has no per-request completion callback; optional global playback/error status is available at authenticated `GET /v1/status`.

## How Home Assistant maps to these cases

With this project's **custom HA media-player integration**, a normal `tts.speak` action uses **case 2**. The integration extracts the message and options, looks up the Wyoming service address and defaults from the action's selected `tts` entity, and sends them to the bridge through HTTP `/v1/speak`. The bridge requests synthesis and plays the returned audio. HA does not receive audio on this path. This is custom integration behavior; it is not a standard Wyoming text-to-speaker command. HA's audio-file cache is bypassed, so `cache: false` has no effect on this direct path.

With HA's built-in Wyoming satellite integration, `assist_satellite.announce` uses **case 1**. HA obtains speech audio using the satellite's selected Assist pipeline, receives that audio, and sends PCM to the bridge. The bridge receives audio rather than the announcement text.

## Discover available voices

A client that needs voice selection can connect to the Wyoming listener and send:

```json
{"type":"describe"}
```

Read standard `info.tts[0].voices` from the response. Use each advertised `name` exactly as returned; identifiers are opaque strings.

Each voice's optional `description` is forwarded unchanged for display or voice selection, alongside its languages, speakers, attribution, version and any other advertised voice metadata. Descriptions are human-readable text; use `name` as the identifier when requesting speech. Read advertised languages and speakers for those optional selections.

If `tts` is absent, synthesis is unavailable. Clients using service defaults need not query voices.

## Cancel either input

HTTP clients send authenticated `POST /v1/cancel` with `{}`. The response is `202` with `{"accepted":true}`.

Wyoming clients can send this sole custom operation on a separate control connection:

```json
{"type":"user-event","data":{"name":"hifiberry.cancel","data":{}}}
```

Its acknowledgement is:

```json
{"type":"user-event","data":{"name":"hifiberry.cancel","data":{"accepted":true}}}
```

Cancel globally stops current output, discards buffered audio and replay media, restores attenuated background volumes, and cancels active bridge-owned synthesis requests. It applies to all inputs and clients. Acknowledgements indicate queue admission, not completion.

For case 1, stop sending the cancelled audio. To reuse the speaker connection, send `audio-stop` and wait for `played` before the next `audio-start`. Remaining old chunks are discarded through that `audio-stop`; the connection stays open. For case 2, no audio cleanup or additional connection is needed from the client.

## Wyoming connection rules

This subset follows the [official Wyoming protocol](https://github.com/OHF-Voice/wyoming#readme). `user-event` is a standard envelope; `hifiberry.cancel` and its acknowledgement are project-specific. `synthesize-stop` is not an abort command.

Handle `error` and EOF. Standard `ping`/`pong` is supported; keep idle connections alive at least every 30 seconds. Wyoming has no authentication or encryption: use a trusted network. HTTP requires the configured bearer token.
