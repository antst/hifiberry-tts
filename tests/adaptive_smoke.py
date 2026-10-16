#!/usr/bin/env python3
"""Replay slow TTS cadence on a private PipeWire null sink; no real TTS calls."""
import contextlib
import http.server
import json
import os
import socket
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request

from pipewire_smoke import command, node, objects, unused_port, wait_for
from wyoming_smoke import TtsServer, event, receive


def main():
    processes = []
    server = None
    tts = TtsServer()
    with tempfile.TemporaryDirectory(prefix="tts-adaptive-test-") as tmp:
        root = Path(tmp)
        runtime = root / "runtime"
        runtime.mkdir(mode=0o700)
        os.environ["XDG_RUNTIME_DIR"] = str(runtime)
        with open(root / "process.log", "wb") as log:
            def spawn(*args, env=None):
                proc = subprocess.Popen(args, stdout=log, stderr=log, env=env)
                processes.append(proc)
                return proc

            try:
                spawn("pipewire")
                wait_for(lambda: (runtime / "pipewire-0").exists())
                spawn("wireplumber")
                command("pw-cli", "create-node", "adapter", "{ factory.name = support.null-audio-sink node.name = adaptive-test-sink media.class = Audio/Sink object.linger = true audio.position = [ FL FR ] }")
                wait_for(lambda: any(n.get("info", {}).get("props", {}).get("node.name") == "adaptive-test-sink" for n in objects()))
                port = unused_port()
                satellite_port = unused_port()
                token = "test-token-at-least-24-characters"
                env = {**os.environ, "BIND_ADDR": f"127.0.0.1:{port}",
                       "API_TOKEN": token, "MPD_BIND_ADDR": "", "WYOMING_BIND_ADDR": f"127.0.0.1:{satellite_port}",
                       "WYOMING_TTS_ADDR": f"127.0.0.1:{tts.server_address[1]}",
                       "PCM_BUFFERING_MODE": "adaptive", "PCM_BUFFER_MS": "1000",
                       "PCM_PREBUFFER_MS": "250", "DUCK_FACTOR": "1"}
                spawn(sys.argv[1], env=env)

                def api(path="status", payload=None):
                    request = urllib.request.Request(f"http://127.0.0.1:{port}/v1/{path}",
                        headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"},
                        data=None if path == "status" else json.dumps(payload or {}).encode())
                    with urllib.request.urlopen(request, timeout=5) as response:
                        return json.load(response)

                wait_for(lambda: api()["state"] == "idle")
                fixture = json.loads((Path(__file__).parent / "fixtures/slow-tts-cadence.json").read_text())
                tts.cadence = fixture
                api("speak", {"text": "Recorded cadence replay"})
                wait_for(lambda: api()["state"] == "buffering")
                deadline = time.monotonic() + 22
                while not tts.finished.is_set():
                    state = api()
                    assert state["state"] == "buffering", state
                    assert state["pcm_underruns"] == 0, state
                    assert state["ducked_streams"] == 0, state
                    assert time.monotonic() < deadline, state
                    time.sleep(.05)
                wait_for(lambda: api()["state"] == "playing")
                wait_for(lambda: api()["state"] == "idle", seconds=8)
                state = api()
                expected = sum(frames for _, frames in fixture["chunks"]) / fixture["rate"]
                assert not state["last_error"], state
                assert state["pcm_underruns"] == 0, state
                assert abs(state["position"] - expected) < .05, (state, expected)
                print(f"PASS: recorded 14.65s production / {expected:.5f}s PCM plays continuously with zero underruns; reception exceeds decoded queue window", flush=True)

                # Start fast, then starve once. Reception must keep running while
                # output waits for the whole remaining tail, despite the decoded queue cap.
                tts.cadence = {"rate": 16000, "channels": 1,
                    "chunks": [[0, 6400], [1.2, 6400], [1.4, 16000]], "complete_at": 1.5}
                tts.finished.clear()
                api("speak", {"text": "Single recovery"})
                wait_for(lambda: api()["state"] == "playing")
                assert not tts.finished.is_set()
                wait_for(lambda: api()["pcm_waiting_for_complete"])
                wait_for(lambda: api()["state"] == "idle", seconds=8)
                state = api()
                assert not state["last_error"] and state["pcm_underruns"] == 1, state
                assert abs(state["position"] - 1.8) < .05, state
                print("PASS: one starvation causes one completion refill; no repeated PCM underruns or ingestion deadlock", flush=True)

                # Fresh HA synthesis may hold a media response open longer than
                # the former hardcoded 20-second total download deadline.
                class Handler(http.server.BaseHTTPRequestHandler):
                    def do_GET(self):
                        self.send_response(200)
                        self.send_header("Content-Type", "audio/mpeg")
                        self.end_headers()
                        self.wfile.flush()
                        time.sleep(20.5)
                        self.wfile.write((Path(__file__).parent / "fixtures/tone.mp3").read_bytes())

                    def log_message(self, *args):
                        pass

                server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
                threading.Thread(target=server.serve_forever, daemon=True).start()
                api("play_media", {"url": f"http://127.0.0.1:{server.server_port}/slow.mp3"})
                wait_for(lambda: api()["state"] == "playing", seconds=26)
                wait_for(lambda: api()["state"] == "idle")
                assert not api()["last_error"], api()
                print("PASS: a fresh HTTP audio response delayed beyond 20s downloads and plays", flush=True)

                # Startup fill remains250ms even with60s receive-ahead. After
                # EOF the TTS socket must close while most speech is unplayed.
                tts.silent = True
                tts.cadence = {"rate": 44100, "channels": 1,
                    "chunks": [[0, 88200]] + [[.8, 4410]] * 580, "complete_at": .9}
                tts.finished.clear()
                tts.released.clear()
                api("speak", {"text": "One minute ahead"})
                wait_for(lambda: api()["state"] == "playing")
                assert not tts.finished.is_set(), "Waited for full receive buffer before playing"
                wait_for(lambda: tts.released.is_set())
                wait_for(lambda: api()["pcm_buffered_ms"] > 55000)
                before = api()
                start = time.monotonic()
                api("cancel")
                wait_for(lambda: api()["state"] == "idle")
                state = api()
                assert time.monotonic() - start < 2, state
                assert state["pcm_buffered_ms"] == 0 and not state["has_media"], state
                assert not state["pcm_waiting_for_complete"], state
                wait_for(lambda: not node("hifiberry-tts"))
                try:
                    api("play")
                    raise AssertionError("Cancelled speech remained replayable")
                except urllib.error.HTTPError as err:
                    assert err.code == 409
                print(f"PASS: {before['pcm_buffered_ms']}ms receive-ahead, early playback, producer TCP released before playback ends, full cancel clears60s queue", flush=True)

                # Cancel while the direct TTS producer is stalled mid-inference.
                tts.cadence = None
                tts.cancelled.clear()
                api("speak", {"text": "cancel"})
                wait_for(lambda: api()["state"] == "playing")
                api("cancel")
                wait_for(lambda: tts.cancelled.is_set())
                wait_for(lambda: api()["state"] == "idle")
                assert not api()["has_media"] and api()["pcm_buffered_ms"] == 0
                print("PASS: cancel closes active direct synthesis TCP while producer is stalled", flush=True)

                # Satellite may be waiting for first audio, or hold bufferedPCM
                # after audio-stop. Cancel discards the tail without breaking HA TCP.
                for with_pcm in (False, True):
                    with socket.create_connection(("127.0.0.1", satellite_port), timeout=3) as connection:
                        file = connection.makefile("rwb", buffering=0)
                        event(file, "audio-start", {"rate": 44100, "width": 2, "channels": 1})
                        event(file, "ping", {"text": "registered"})
                        assert receive(file)[0]["type"] == "pong"
                        if with_pcm:
                            event(file, "audio-chunk", {"rate": 44100, "width": 2, "channels": 1}, bytes(44100 * 2 * 2))
                            wait_for(lambda: api()["state"] == "playing")
                        api("cancel")
                        # Keep sending after cancellation: all old tail data must
                        # be discarded, and completion must keep HA connected.
                        event(file, "audio-chunk", {"rate": 44100, "width": 2, "channels": 1}, bytes(44100 * 2))
                        event(file, "audio-stop")
                        event(file, "ping", {"text":"after-cancel"})
                        kinds = set()
                        while not {"played", "pong"}.issubset(kinds):
                            kinds.add(receive(file)[0]["type"])
                        wait_for(lambda: api()["state"] == "idle")
                        assert api()["pcm_buffered_ms"] == 0 and not api()["has_media"]
                print("PASS: satellite cancel discards first-PCM/active tails and preserves TCP/played/ping", flush=True)
            except BaseException:
                log.flush()
                print((root / "process.log").read_text(errors="replace"), file=sys.stderr)
                raise
            finally:
                for proc in reversed(processes):
                    if proc.poll() is None:
                        proc.terminate()
                for proc in reversed(processes):
                    with contextlib.suppress(subprocess.TimeoutExpired):
                        proc.wait(timeout=3)
                    if proc.poll() is None:
                        proc.kill()
                if server is not None:
                    server.shutdown()
                tts.shutdown()


if __name__ == "__main__":
    main()
