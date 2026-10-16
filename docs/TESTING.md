# Verification scope

The initial implementation was tested on 64-bit Debian 13 HiFiBerryOS-NG with AudioControl 0.21.0, PipeWire 1.4.2 and WirePlumber 0.5.8. A baseline of 33 vendor configuration checksums remained unchanged after installation/upgrades. Device-specific addresses, credentials and raw production records are not included in this repository.

A private Linux null-sink test uses real PipeWire/WirePlumber and two background tones. The Rust speech stream mixes with both; recorded background amplitude during ducking was 0.250 of baseline and speech amplitude 3999.4 against an input amplitude of 4000. Sink/master gains remained unchanged. Tests exercise decoding, MPD commands, pause/resume/seek, speech volume, replay, replacement, user gain changes, graceful SIGTERM restoration, HTTP failures/size limits and Wyoming transport/cancellation. The fake TTS test checks the streaming text sequence and legacy plain-synthesis fallback. These are protocol/audio-output tests, not a live S2 quality or inference-speed benchmark.

An isolated Home Assistant 2026.10.1 instance exercised three real config-entry/service paths against a silent temporary candidate on the actual NG device: built-in MPD, built-in Wyoming satellite (including a 15-second announcement), and the custom media player via actual `tts.speak`. The direct test patches HA synthesis methods to fail if called; Rust receives the selected engine, exact text, per-call voice, language and speaker and contacts the fake Wyoming server itself. The user's production HA files are not modified by these tests.

Rust tests cover protected NG stages, URL/auth validation, server selection, native decoding/seek, bounded PCM, prebuffer/recovery, short tails, cancellation, and capability negotiation. A trace-driven test feeds two-phrase cadence (4.226 seconds of PCM over 14.321 seconds) to the queue: no samples are lost, but a 1500 ms prebuffer still starves, as expected from the producer's rate.

Adaptive tests replay a second captured arrival schedule: 4.13315 seconds of PCM produced over 14.648 seconds in 43 chunks. Only chunk sizes/timing are retained in the repository, without recorded speech, text, addresses or logs. Both the queue test and a real private PipeWire playback test wait for completion and then drain all samples with zero PCM underruns. A separate initially fast source starves once, continues receiving beyond the RAM window, then drains its complete tail with exactly one underrun. Another HTTP test holds a fresh audio response open for 20.5 seconds before providing audio, verifying the configurable download deadline.

Run local checks:

```sh
cargo fmt --check
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
```

Linux checks require the build dependencies in [BRIDGE.md](BRIDGE.md). The private PipeWire smoke test requires PipeWire, WirePlumber, dbus-run-session, Python 3 and mpv for background generation only:

```sh
dbus-run-session -- python3 tests/pipewire_smoke.py target/release/hifiberry-tts
dbus-run-session -- python3 tests/adaptive_smoke.py target/release/hifiberry-tts
```

**Use an isolated container/audio session**, not a production PipeWire session: the test starts its own graph and generators. The runtime does not use mpv or Python.

The three HA tests require Python 3.14, HA 2026.10.1, pytest/pytest-asyncio and the integrations' dependencies. Their module docstrings describe environment variables; they skip without explicit test-device settings. Run only against a **temporary silent candidate**, never a player's live production ports. HACS and hassfest validation run in GitHub Actions. A passing protocol test does not establish glitch-free live inference; see [KNOWN_ISSUES.md](KNOWN_ISSUES.md).
