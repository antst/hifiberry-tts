#!/usr/bin/env python3
"""Discovery-only metadata regression; no synthesis or audio session required."""
import contextlib
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time

from wyoming_smoke import TtsServer, event, receive


def unused_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def main():
    tts = TtsServer()
    port = unused_port()
    env = {**os.environ, "API_TOKEN": "discovery-test-token-at-least-24-characters",
           "BIND_ADDR": f"127.0.0.1:{unused_port()}", "MPD_BIND_ADDR": "",
           "WYOMING_BIND_ADDR": f"127.0.0.1:{port}",
           "WYOMING_TTS_ADDR": f"127.0.0.1:{tts.server_address[1]}"}
    with tempfile.TemporaryFile(mode="w+") as log:
        bridge = subprocess.Popen([str(Path(sys.argv[1]).resolve())], env=env,
                                  stdout=log, stderr=log)
        try:
            def describe():
                with socket.create_connection(("127.0.0.1", port), timeout=3) as connection:
                    with connection.makefile("rwb") as file:
                        event(file, "describe")
                        info, _ = receive(file)
                        assert info["type"] == "info", info
                        return info["data"]["tts"][0]["voices"]

            for _ in range(200):
                assert bridge.poll() is None, "Bridge exited before discovery"
                try:
                    voices = describe()
                    break
                except ConnectionRefusedError:
                    time.sleep(.01)
            else:
                raise AssertionError("Bridge did not start")
            assert voices == tts.voices, voices
            tts.voices[0]["description"] = "Updated description — обновлённое описание."
            assert describe() == tts.voices, "Description update was not forwarded"
            del tts.voices[0]["description"]
            assert describe() == tts.voices, "Optional description was invented or cached"
            assert not tts.requests, "Discovery unexpectedly triggered synthesis"
            print("PASS: complete voice metadata, Unicode descriptions, live updates and omitted descriptions pass through without synthesis")
        except BaseException:
            log.seek(0)
            print(log.read(), file=sys.stderr)
            raise
        finally:
            bridge.terminate()
            with contextlib.suppress(subprocess.TimeoutExpired):
                bridge.wait(timeout=5)
            if bridge.poll() is None:
                bridge.kill()
                bridge.wait()
            tts.shutdown()
            tts.server_close()


if __name__ == "__main__":
    main()
