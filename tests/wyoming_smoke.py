"""Wyoming protocol/streaming checks used by pipewire_smoke.py (no real speech)."""
import json
import math
import os
import socket
import socketserver
import struct
import threading
import time


def event(file, kind, data=None, payload=b""):
    header = {"type": kind, "data": data or {}}
    if payload:
        header["payload_length"] = len(payload)
    file.write(json.dumps(header).encode() + b"\n" + payload)
    file.flush()


def receive(file):
    header = json.loads(file.readline())
    extra = file.read(header.get("data_length", 0))
    if extra:
        header.setdefault("data", {}).update(json.loads(extra))
    payload = file.read(header.get("payload_length", 0))
    return header, payload


class TtsServer(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True

    def __init__(self, host="127.0.0.1"):
        self.requests = []
        self.modes = []
        self.streaming = True
        self.voices = [{"name": "test-voice", "installed": True,
                        "languages": ["en", "ru"],
                        "description": "Calm narration — спокойная речь.",
                        "version": "1.0",
                        "attribution": {"name": "Test", "url": "https://example.org"}}]
        self.cadence = None
        self.silent = False
        self.peers = []
        self.finished = threading.Event()
        self.cancelled = threading.Event()
        self.released = threading.Event()
        server = self

        class Handler(socketserver.StreamRequestHandler):
            def handle(self):
                try:
                    request, _ = receive(self.rfile)
                    if request["type"] == "describe":
                        event(self.wfile, "info", {"tts": [{
                            "name": "Test Wyoming TTS", "installed": True,
                            "supports_synthesize_streaming": server.streaming,
                            "attribution": {"name": "Test", "url": "https://example.org"},
                            "voices": server.voices,
                        }]})
                        # HA describes and disconnects; the bridge describes
                        # and synthesizes on the same connection.
                        if not self.rfile.peek(1):
                            return
                        request, _ = receive(self.rfile)
                    mode = "plain"
                    if request["type"] == "synthesize-start":
                        mode = "streaming"
                        start = request
                        chunk, _ = receive(self.rfile)
                        request, _ = receive(self.rfile)
                        stop, _ = receive(self.rfile)
                        assert chunk["type"] == "synthesize-chunk", chunk
                        assert stop["type"] == "synthesize-stop", stop
                        assert chunk["data"]["text"] == request["data"]["text"]
                        assert start["data"].get("voice") == request["data"].get("voice")
                    assert request["type"] == "synthesize", request
                    server.requests.append(request["data"])
                    server.modes.append(mode)
                    server.peers.append(self.client_address[0])
                    server.finished.clear()
                    if server.cadence is not None:
                        cadence = server.cadence
                        rate = cadence["rate"]
                        fmt = {"rate": rate, "width": 2, "channels": 1}
                        total = sum(frames for _, frames in cadence["chunks"])
                        audio = bytes(total * 2) if server.silent else b"".join(struct.pack("<h", int(4000 * math.sin(2 * math.pi * 880 * i / rate))) for i in range(total))
                        event(self.wfile, "audio-start", fmt)
                        started = time.monotonic()
                        offset = 0
                        for arrival, frames in cadence["chunks"]:
                            time.sleep(max(0, started + arrival - time.monotonic()))
                            end = offset + frames * 2
                            event(self.wfile, "audio-chunk", fmt, audio[offset:end])
                            offset = end
                        time.sleep(max(0, started + cadence["complete_at"] - time.monotonic()))
                        event(self.wfile, "audio-stop")
                        if mode == "streaming":event(self.wfile, "synthesize-stopped")
                        server.finished.set()
                        self.connection.settimeout(3)
                        if not self.rfile.read(1):
                            server.released.set()
                        return
                    fmt = {"rate": 22050, "width": 2, "channels": 1}
                    audio = b"".join(struct.pack("<h", int(4000 * math.sin(2 * math.pi * 880 * i / 22050))) for i in range(22050 * 2))
                    if os.getenv("TTS_FAKE_SILENT"):
                        audio = bytes(len(audio))
                    event(self.wfile, "audio-start", fmt)
                    event(self.wfile, "audio-chunk", fmt, audio[:17640])
                    if request["data"]["text"] == "cancel":
                        self.connection.settimeout(5)
                        if not self.rfile.read(1):
                            server.cancelled.set()
                        return
                    # Playback must begin during this intentional synthesis gap.
                    time.sleep(.7)
                    # Streaming Wyoming permits multiple audio segments within
                    # one utterance; only synthesize-stopped ends that request.
                    if mode == "streaming":
                        event(self.wfile, "audio-stop")
                        event(self.wfile, "audio-start", fmt)
                    event(self.wfile, "audio-chunk", fmt, audio[17640:])
                    event(self.wfile, "audio-stop")
                    if mode == "streaming":event(self.wfile, "synthesize-stopped")
                    server.finished.set()
                except (BrokenPipeError, ConnectionResetError, TimeoutError):
                    pass

        super().__init__((host, 0), Handler)
        threading.Thread(target=self.serve_forever, daemon=True).start()


def exercise(api, node, gains, original, wait_for, tts, wyoming_port, mpd_port, token):
    before = len(tts.requests)
    start = time.monotonic()
    api("play_media", {"url": json.dumps({"text": "Привет", "voice": "test-voice"}, ensure_ascii=False)})
    wait_for(lambda: api()["state"] == "playing", seconds=2)
    elapsed = time.monotonic() - start
    assert not tts.finished.is_set(), "Waited for the end of synthesis instead of streaming"
    assert tts.requests[before] == {"text": "Привет", "voice": {"name": "test-voice"}}, tts.requests
    assert tts.modes[before] == "streaming", tts.modes
    wait_for(lambda: api()["state"] == "idle")
    assert not api()["last_error"], api()
    assert abs(api()["position"] - 2) < .05, api()
    expected_peak = 20 * math.log10(4000 / 32768) + api()["speech_gain_db"]
    assert abs(api()["speech_peak_dbfs"] - expected_peak) < .1, api()
    assert api()["speech_clipped_samples"] == 0, api()
    api("play")  # Direct TTS replay re-synthesizes.
    wait_for(lambda: len(tts.requests) == before + 2)
    api("stop")
    wait_for(lambda: api()["state"] == "idle")

    tts.streaming = False
    api("play_media", {"url": "Legacy server fallback"})
    wait_for(lambda: api()["state"] == "playing")
    assert tts.modes[-1] == "plain", tts.modes
    api("stop")
    wait_for(lambda: api()["state"] == "idle")
    tts.streaming = True

    # The same custom text convention over standard MPD, no custom HA platform.
    with socket.create_connection(("127.0.0.1", mpd_port), timeout=5) as connection:
        file = connection.makefile("rwb", buffering=0)
        assert file.readline().startswith(b"OK MPD")
        for command in ('password ' + json.dumps(token), "clear", "add " + json.dumps("cancel"), "play"):
            file.write((command + "\n").encode())
            assert file.readline() == b"OK\n"
        wait_for(lambda: api()["state"] == "playing")
        file.write(b"stop\n")
        assert file.readline() == b"OK\n"
    wait_for(lambda: tts.cancelled.is_set())
    wait_for(lambda: api()["state"] == "idle")
    for label, values in original.items():
        assert all(abs(a-b)<.001 for a,b in zip(gains(node(label)), values))

    # Native Wyoming satellite: describe, handshake, PCM, concurrent ping, played.
    with socket.create_connection(("127.0.0.1", wyoming_port), timeout=5) as connection:
        file = connection.makefile("rwb", buffering=0)
        event(file, "describe")
        info, _ = receive(file)
        assert info["type"] == "info" and info["data"]["satellite"]["installed"]
        event(file, "run-satellite")
        assert receive(file)[0]["type"] == "satellite-connected"
        fmt = {"rate": 22050, "width": 2, "channels": 1}
        event(file, "audio-start", fmt)
        # Silence avoids a second audible tone in future live-device reuse.
        event(file, "audio-chunk", fmt, bytes(22050 * 2))
        event(file, "audio-stop")
        event(file, "ping", {"text": "test"})
        kinds = set()
        while "played" not in kinds:
            header, _ = receive(file)
            kinds.add(header["type"])
            if header["type"] == "pong":
                assert header["data"]["text"] == "test"
        assert "pong" in kinds
    wait_for(lambda: api()["state"] == "idle")
    assert not api()["last_error"], api()
    # Standard TTS proxy returns PCM to the caller without playing locally.
    # Voices are in standard info.tts, parsed by the real Wyoming library in HA.
    with socket.create_connection(("127.0.0.1", wyoming_port), timeout=5) as connection:
        file = connection.makefile("rwb", buffering=0)
        event(file, "describe")
        info, _ = receive(file)
        assert "hifiberry" not in info["data"], info
        program = info["data"]["tts"][0]
        assert not program["supports_synthesize_streaming"], info
        assert program["voices"] == tts.voices, info
        before = len(tts.requests)
        # Exercise data_length framing as well as binary payload transport.
        data = {"text": "Proxy test", "voice": {"name": "test-voice", "language": "ru", "speaker": "2"}, "text_format": "text"}
        extra = json.dumps(data, ensure_ascii=False).encode()
        file.write(json.dumps({"type":"synthesize", "data_length":len(extra)}).encode()+b"\n"+extra)
        event(file, "ping", {"text":"during-proxy"})
        kinds = []
        pcm = b""
        while "audio-stop" not in kinds or "pong" not in kinds:
            header, payload = receive(file)
            kinds.append(header["type"])
            pcm += payload
        assert len(pcm) == 22050 * 2 * 2 and kinds.count("audio-start") == 1, kinds
        assert tts.requests[before] == data, tts.requests
        assert api()["state"] == "idle", "TTS proxy unexpectedly played audio locally"
        # A second utterance on the same connection is supported.
        event(file, "synthesize", {"text":"Again"})
        while receive(file)[0]["type"] != "audio-stop":pass
    # Closing a proxy client must close upstream synthesis even before EOF.
    tts.cancelled.clear()
    with socket.create_connection(("127.0.0.1", wyoming_port), timeout=5) as connection:
        file = connection.makefile("rwb", buffering=0)
        event(file, "synthesize", {"text":"cancel"})
        assert receive(file)[0]["type"] == "audio-start"
        assert receive(file)[0]["type"] == "audio-chunk"
        file.close()
    wait_for(lambda: tts.cancelled.is_set())
    # Global cancellation also closes a standalone synthesis-proxy request.
    tts.cancelled.clear()
    with socket.create_connection(("127.0.0.1", wyoming_port), timeout=5) as connection:
        file = connection.makefile("rwb", buffering=0)
        event(file, "synthesize", {"text":"cancel"})
        assert receive(file)[0]["type"] == "audio-start"
        assert receive(file)[0]["type"] == "audio-chunk"
        api("cancel")
        assert not file.readline(), "Cancel left TTS proxy socket open"
        wait_for(lambda: tts.cancelled.is_set())
    # The sole custom operation is a user-event for global cancellation.
    tts.cancelled.clear()
    api("speak", {"text":"cancel"})
    wait_for(lambda: api()["state"] == "playing")
    with socket.create_connection(("127.0.0.1", wyoming_port), timeout=5) as connection:
        file = connection.makefile("rwb", buffering=0)
        event(file, "user-event", {"name":"hifiberry.cancel", "data":{}})
        reply, _ = receive(file)
        assert reply["data"] == {"name":"hifiberry.cancel", "data":{"accepted":True}}, reply
        wait_for(lambda: tts.cancelled.is_set())
        wait_for(lambda: api()["state"] == "idle")
        assert not api()["has_media"]
        event(file, "user-event", {"name":"unknown"})
        assert receive(file)[0]["type"] == "error"
        event(file, "ping", {"text":"still-connected"})
        assert receive(file)[0]["type"] == "pong"
    print("PASS: standard Wyoming describe/voice catalogue, plain synthesis proxy, PCM payloads, extended JSON framing, concurrent ping, sequential requests, disconnect cancellation and custom Cancel", flush=True)
    print(f"PASS: direct Wyoming PCM starts during synthesis ({elapsed*1000:.1f} ms to observed playing), per-request voice, MPD text, replay, upstream cancellation, satellite handshake/PCM/ping/played", flush=True)
