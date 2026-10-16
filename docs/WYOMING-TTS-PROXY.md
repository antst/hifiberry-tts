# Optional Wyoming PCM synthesis service — bridge 0.5.0+

This service returns generated audio to the caller. It does not play that audio on the bridge. For local playback, use one of the two [client input paths](WYOMING-CLIENT.md).

Connect to the bridge Wyoming listener, normally port 10700. Send `describe` and inspect `info.tts`. When available, the single TTS program advertises voices and `supports_synthesize_streaming:false`.

Send one plain request:

```json
{"type":"synthesize","data":{"text":"Hello.","voice":{"name":"<advertised-voice-id>","language":"en"},"text_format":"text"}}
```

`text` is required. Standard optional voice fields are `name`, `language` and `speaker`; omitted fields use synthesis-service defaults. Treat advertised voice identifiers as opaque strings. Complete voice descriptors, including optional `description`, are forwarded unchanged by `describe`.

Receive `audio-start`, one or more `audio-chunk` events with binary PCM, and `audio-stop`. Errors use standard `error`. Send one request at a time; sequential requests on the same connection are supported. Streaming-text events `synthesize-start/chunk/stop` are unsupported by this service. Closing the connection abandons its synthesis request.

Framing and connection rules follow the [official Wyoming protocol](https://github.com/OHF-Voice/wyoming#readme). Keep reading generated audio promptly. This service is optional for clients that need PCM; neither local-playback input requires relaying synthesized audio back to the bridge.
