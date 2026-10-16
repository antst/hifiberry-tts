#!/usr/bin/env python3
"""Real Linux/PipeWire/mpv smoke test using a null sink, never physical speakers.

Run inside an isolated Debian container, under dbus-run-session. Requires an
already-built Linux binary as argv[1]. Does not use mock audio processes.
"""

import array
import contextlib
import http.server
import json
import math
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
import wave
from wyoming_smoke import TtsServer, exercise


def wait_for(predicate, seconds=8):
    deadline = time.monotonic() + seconds
    last = None
    while time.monotonic() < deadline:
        try:
            last = predicate()
            if last:
                return last
        except (OSError, urllib.error.URLError):
            pass
        time.sleep(.05)
    raise AssertionError(f"Timed out waiting for {predicate}; last result={last}")


def unused_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def tone(path, frequency, seconds):
    samples = array.array("h", (int(4000 * math.sin(2 * math.pi * frequency * i / 48000)) for i in range(int(seconds * 48000)) for _ in range(2)))
    with wave.open(str(path), "wb") as output:
        output.setparams((2, 2, 48000, 0, "NONE", "not compressed"))
        output.writeframes(samples.tobytes())


def command(*args):
    return subprocess.check_output(args, stderr=subprocess.PIPE, timeout=5)


def objects():
    return json.loads(command("pw-dump"))


def node(name):
    return next((n for n in objects() if n.get("type") == "PipeWire:Interface:Node" and n.get("info", {}).get("props", {}).get("application.name") == name), None)


def gains(n):
    return next(p["channelVolumes"] for p in n["info"]["params"]["Props"] if "channelVolumes" in p)


def set_gains(n, values):
    command("pw-cli", "set-param", str(n["id"]), "Props", json.dumps({"channelVolumes": values}))
    wait_for(lambda: any(obj.get("id") == n["id"] and approx(gains(obj), values) for obj in objects() if obj.get("id") == n["id"]))


def approx(actual, expected):
    return len(actual) == len(expected) and all(abs(a-b) < .001 for a, b in zip(actual, expected))


def spectral_level(samples, rate, frequency):
    # Projection over an integer number of periods: independent of signal phase.
    n = len(samples)
    real = sum(v * math.cos(2 * math.pi * frequency * i / rate) for i, v in enumerate(samples))
    imag = sum(v * math.sin(2 * math.pi * frequency * i / rate) for i, v in enumerate(samples))
    return 2 * math.hypot(real, imag) / n


def main():
    processes = []
    with tempfile.TemporaryDirectory(prefix="hifiberry-tts-test-") as tmp:
        root = Path(tmp)
        runtime = root / "runtime"
        runtime.mkdir(mode=0o700)
        os.environ["XDG_RUNTIME_DIR"] = str(runtime)
        log = open(root / "process.log", "wb")

        def spawn(*args, env=None):
            proc = subprocess.Popen(args, stdout=log, stderr=log, env=env)
            processes.append(proc)
            return proc

        server = None
        try:
            spawn("pipewire")
            wait_for(lambda: (runtime / "pipewire-0").exists())
            spawn("wireplumber")
            command("pw-cli", "create-node", "adapter", "{ factory.name = support.null-audio-sink node.name = tts-test-sink media.class = Audio/Sink object.linger = true audio.position = [ FL FR ] }")
            wait_for(lambda: any(n.get("info", {}).get("props", {}).get("node.name") == "tts-test-sink" for n in objects()))
            tone(root / "background.wav", 220, 2)
            tone(root / "background2.wav", 330, 2)
            tone(root / "speech.wav", 880, 2)

            class Handler(http.server.BaseHTTPRequestHandler):
                hits = 0

                def do_GET(self):
                    Handler.hits += 1
                    if self.path == "/missing":
                        self.send_error(404)
                        return
                    if self.path == "/oversize":
                        self.send_response(200)
                        self.send_header("Content-Length", "99999999")
                        self.end_headers()
                        return
                    data = (Path(__file__).parent / "fixtures/tone.mp3").read_bytes() if self.path == "/speech.mp3" else (root / "speech.wav").read_bytes()
                    self.send_response(200)
                    self.send_header("Content-Type", "audio/wav")
                    self.send_header("Content-Length", str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)

                def log_message(self, *args):
                    pass

            server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
            threading.Thread(target=server.serve_forever, daemon=True).start()
            audio_url = f"http://127.0.0.1:{server.server_port}/speech.wav"
            port = unused_port()
            mpd_port = unused_port()
            wyoming_port = unused_port()
            tts = TtsServer()
            env = {**os.environ, "BIND_ADDR": f"127.0.0.1:{port}", "MPD_BIND_ADDR":f"127.0.0.1:{mpd_port}", "API_TOKEN": "test-token-at-least-24-characters", "MEDIA_HOSTS": "127.0.0.1"}
            env.update(WYOMING_BIND_ADDR=f"127.0.0.1:{wyoming_port}", WYOMING_TTS_ADDR=f"127.0.0.1:{tts.server_address[1]}")
            bridge = spawn(sys.argv[1], env=env)

            def api(path="status", payload=None):
                req = urllib.request.Request(f"http://127.0.0.1:{port}/v1/{path}",
                    headers={"Authorization": f"Bearer {env['API_TOKEN']}", "Content-Type": "application/json"},
                    data=json.dumps(payload or {}).encode() if path != "status" else None)
                with urllib.request.urlopen(req, timeout=5) as response:
                    return json.load(response)

            wait_for(lambda: api()["state"] == "idle")
            for label, filename in [("background-one", "background.wav"), ("background-two", "background2.wav")]:
                spawn("mpv", "--no-config", "--no-video", "--ao=pipewire", "--loop-file=inf", f"--audio-client-name={label}", str(root / filename))
                wait_for(lambda: node(label) and node(label)["info"]["state"] == "running")
            set_gains(node("background-one"), [.8, .4])
            set_gains(node("background-two"), [.6, .6])
            original = {label: gains(node(label)) for label in ("background-one", "background-two")}
            sink = next(n for n in objects() if n.get("info", {}).get("props", {}).get("node.name") == "tts-test-sink")
            sink_gains = gains(sink)

            recorder = spawn("pw-record", "--target", "tts-test-sink", "--properties", "{ stream.capture.sink = true }", "--format", "s16", "--rate", "48000", str(root / "mixed.wav"))
            time.sleep(.8)
            record_start = time.monotonic() - .8
            api("play_media", {"url": audio_url, "duck_factor": .25})
            wait_for(lambda: api()["state"] == "playing")
            speech_start = time.monotonic() - record_start
            print("Playing status:", api(), flush=True)
            assert api()["ducked_streams"] == 2
            assert api().get("duck_warning") is None, api()
            for label, before in original.items():
                assert approx(gains(node(label)), [v * .25 for v in before]), (label, gains(node(label)))
            wait_for(lambda: node("hifiberry-tts") and node("hifiberry-tts")["info"]["state"] == "running")
            wait_for(lambda: api()["state"] == "idle")
            time.sleep(.6)
            recorder.send_signal(signal.SIGINT)
            recorder.wait(timeout=5)
            for label, before in original.items():
                assert approx(gains(node(label)), before), (label, gains(node(label)))
            current_sink = next(n for n in objects() if n.get("id") == sink["id"])
            assert gains(current_sink) == sink_gains

            with wave.open(str(root / "mixed.wav"), "rb") as source:
                rate, channels = source.getframerate(), source.getnchannels()
                samples = array.array("h", source.readframes(source.getnframes()))
                mono = samples[::channels]
            def level(start, freq):
                return spectral_level(mono[int(start * rate):int((start + .2) * rate)], rate, freq)
            baseline = level(.4, 220)
            background_during = level(speech_start + .5, 220)
            speech_during = level(speech_start + .5, 880)
            assert baseline > 100 and speech_during > 100, (baseline, background_during, speech_during)
            assert .15 < background_during / baseline < .35, (baseline, background_during)
            print(f"Captured audio: background ratio={background_during / baseline:.3f}, TTS amplitude={speech_during:.1f}; master unchanged", flush=True)

            # media_play uses the private cache; no second HTTP download.
            hits = Handler.hits
            api("play")
            wait_for(lambda: api()["state"] == "playing")
            api("stop")
            wait_for(lambda: api()["state"] == "idle")
            assert Handler.hits == hits
            assert approx(gains(node("background-one")), original["background-one"])

            # A user/player changes a stream during ducking: do not overwrite it.
            api("play")
            wait_for(lambda: api()["state"] == "playing")
            set_gains(node("background-one"), [.33, .22])
            api("stop")
            wait_for(lambda: api()["state"] == "idle")
            assert approx(gains(node("background-one")), [.33, .22]), (gains(node("background-one")), api())
            assert approx(gains(node("background-two")), original["background-two"])

            # Standard MPD protocol: real decoder controls through the same worker.
            with socket.create_connection(("127.0.0.1", mpd_port), timeout=5) as connection:
                stream = connection.makefile("rwb", buffering=0)
                assert stream.readline().startswith(b"OK MPD ")
                def mpd(command):
                    stream.write((command+"\n").encode())
                    lines = []
                    while True:
                        line = stream.readline().decode().strip()
                        lines.append(line)
                        if line == "OK" or line.startswith("ACK "):
                            return lines
                assert mpd("status")[-1].startswith("ACK [4@")
                assert mpd('password '+json.dumps(env['API_TOKEN'])) == ["OK"]
                assert mpd("clear") == ["OK"]
                assert mpd("add "+json.dumps(audio_url)) == ["OK"]
                assert mpd("play") == ["OK"]
                wait_for(lambda: node("hifiberry-tts") and node("hifiberry-tts")["info"]["state"] == "running")
                assert mpd("pause 1") == ["OK"]
                wait_for(lambda: api()["state"] == "paused")
                assert approx(gains(node("background-one")), [.33, .22])
                assert mpd("setvol 35") == ["OK"]
                wait_for(lambda: api()["volume_level"] == .35)
                assert mpd("seekcur 0.5") == ["OK"]
                wait_for(lambda: api()["position"] >= .4)
                assert mpd("play") == ["OK"]
                wait_for(lambda: api()["state"] == "playing")
                assert approx(gains(node("background-one")), [.33 * .25, .22 * .25])
                assert mpd("stop") == ["OK"]
                wait_for(lambda: api()["state"] == "idle")
                assert approx(gains(node("background-one")), [.33, .22])
                assert mpd("command_list_ok_begin\nstatus\ncurrentsong\ncommand_list_end").count("list_OK") == 2
            print("PASS: standard MPD authentication, URL load, pause/release duck, speech volume, seek, resume/re-duck, stop and command lists",flush=True)

            # Replacement restores before applying ducking again, never compounds.
            api("play_media", {"url": audio_url})
            wait_for(lambda: api()["state"] == "playing")
            api("play_media", {"url": audio_url})
            time.sleep(.5)
            assert approx(gains(node("background-one")), [.33 * .25, .22 * .25])
            bridge.terminate()
            bridge.wait(timeout=10)
            assert approx(gains(node("background-one")), [.33, .22])
            assert approx(gains(node("background-two")), original["background-two"])

            # Restart and check HTTP failures are observable, without ducking music.
            bridge = spawn(sys.argv[1], env=env)
            wait_for(lambda: api()["state"] == "idle")
            for path in ("missing", "oversize"):
                api("play_media", {"url": audio_url.rsplit("/", 1)[0] + "/" + path})
                wait_for(lambda: api().get("last_error"))
                assert api()["state"] == "idle"
                assert approx(gains(node("background-one")), [.33, .22])
            print("PASS: real mixing, all-stream duck/restore, master unchanged, cached replay, stop, external volume changes, replacement, SIGTERM, HTTP errors and size limit", flush=True)
            exercise(api, node, gains, {"background-one":[.33,.22],"background-two":original["background-two"]}, wait_for, tts, wyoming_port, mpd_port, env["API_TOKEN"])
            api("play_media", {"url":audio_url.rsplit("/",1)[0]+"/speech.mp3"})
            wait_for(lambda: api()["state"] == "playing")
            wait_for(lambda: api()["state"] == "idle")
            assert not api()["last_error"],api()
            print("PASS: native MP3 decoding and output drain",flush=True)
            tts.shutdown()
        except BaseException:
            log.flush()
            print((root / "process.log").read_text(errors="replace")[-12000:], file=sys.stderr)
            with contextlib.suppress(Exception):
                print(json.dumps(objects(), indent=2)[-10000:], file=sys.stderr)
            raise
        finally:
            if server:
                server.shutdown()
            for proc in reversed(processes):
                if proc.poll() is None:
                    proc.terminate()
                    with contextlib.suppress(subprocess.TimeoutExpired):
                        proc.wait(timeout=5)
                    if proc.poll() is None:
                        proc.kill()
            log.close()


if __name__ == "__main__":
    main()
