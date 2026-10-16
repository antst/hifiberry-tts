"""Actual HA tts.speak -> custom entity -> Rust -> fake Wyoming server on Pi.

Uses only a temporary candidate bridge and silent fixtures. Requires the same
HA environment as test_builtin_mpd.py, plus DIRECT_TEST_TTS_PORT and
DIRECT_TEST_FIXTURE_PORT. Never point this test at the production player.
"""
import asyncio
import os
from pathlib import Path
import shutil
from types import SimpleNamespace
from unittest.mock import AsyncMock
import pytest
from aiohttp import ClientSession
from homeassistant import auth, bootstrap, loader
from homeassistant.config_entries import ConfigEntries
from homeassistant.core import HomeAssistant
from homeassistant.setup import async_setup_component

HOST = os.getenv("MPD_TEST_HOST")
pytestmark = pytest.mark.skipif(not os.getenv("DIRECT_TEST_TTS_PORT"), reason="Set DIRECT_TEST_* for isolated live bridge test")


@pytest.mark.asyncio
async def test_normal_tts_speak_delivers_text_selected_server_and_voice_directly(tmp_path, monkeypatch):
    shutil.copytree(Path(__file__).resolve().parents[1] / "custom_components",
                    tmp_path / "custom_components", ignore=shutil.ignore_patterns("__pycache__"))
    hass = HomeAssistant(str(tmp_path))
    hass.config.skip_pip = True
    hass.config.internal_url = "http://ha-test.local:8123"
    hass.config.external_url = "http://ha-test.local:8123"
    loader.async_setup(hass)
    hass.config_entries = ConfigEntries(hass, {})
    assert await bootstrap.async_load_base_functionality(hass)
    hass.auth = await auth.auth_manager_from_config(hass, [{"type": "homeassistant"}], [])
    token = os.environ["MPD_TEST_PASSWORD"]
    entries = []
    async with ClientSession() as http:
        async def status():
            async with http.get(f"http://{HOST}:{os.environ['MPD_TEST_HTTP_PORT']}/v1/status",
                                headers={"Authorization": "Bearer " + token}) as response:
                return await response.json()

        async def wait_state(state):
            for _ in range(100):
                current = await status()
                assert not current["last_error"], current
                if current["state"] == state:
                    return current
                await asyncio.sleep(.025)
            raise AssertionError(current)

        async def recorded():
            async with http.get(f"http://{HOST}:{os.environ['DIRECT_TEST_FIXTURE_PORT']}/tts-requests") as response:
                return await response.json()

        async def add(domain, data):
            result = await hass.config_entries.flow.async_init(domain, context={"source": "user"})
            result = await hass.config_entries.flow.async_configure(result["flow_id"], data)
            assert result["type"] == "create_entry", result
            entry = result["result"]
            entries.append(entry)
            await hass.async_block_till_done()
            if entry.state.name == "NOT_LOADED":
                assert await hass.config_entries.async_setup(entry.entry_id)
                await hass.async_block_till_done()
            return entry

        try:
            assert await async_setup_component(hass, "homeassistant", {})
            await add("wyoming", {"host": HOST, "port": int(os.environ["DIRECT_TEST_TTS_PORT"])})
            engines = hass.states.async_entity_ids("tts")
            assert len(engines) == 1, engines
            from homeassistant.components.wyoming.tts import WyomingTtsProvider
            synthesize = AsyncMock(side_effect=AssertionError("HA must not synthesize direct TTS"))
            monkeypatch.setattr(WyomingTtsProvider, "async_get_tts_audio", synthesize)
            monkeypatch.setattr(WyomingTtsProvider, "async_stream_tts_audio", synthesize)
            await add("hifiberry_tts", {
                "host": HOST, "port": int(os.environ["MPD_TEST_HTTP_PORT"]),
                "token": token, "name": "Test Kitchen Audio",
            })
            players = hass.states.async_entity_ids("media_player")
            assert len(players) == 1, players
            entity = players[0]
            before = len((await recorded())["requests"])
            await hass.services.async_call("tts", "speak", {
                "entity_id": engines[0], "media_player_entity_id": entity,
                "message": "Прямой тест речи", "language": "ru",
                "options": {"voice": "test-voice", "speaker": "2"}, "cache": False,
            }, blocking=True)
            await wait_state("playing")
            log = await recorded()
            assert log["requests"][before] == {
                "text": "Прямой тест речи", "voice": {"name": "test-voice", "language": "ru", "speaker": "2"},
            }, log
            if expected_peer := os.getenv("DIRECT_TEST_EXPECT_PEER"):
                assert log["peers"][before] == expected_peer, log
            synthesize.assert_not_awaited()

            async def call(service, **data):
                await hass.services.async_call("media_player", service, {"entity_id": entity, **data}, blocking=True)

            await call("media_pause")
            await wait_state("paused")
            await call("volume_set", volume_level=.4)
            for _ in range(50):
                if (await status())["volume_level"] == .4:
                    break
                await asyncio.sleep(.02)
            assert (await status())["volume_level"] == .4
            await call("media_play")
            await wait_state("playing")
            await call("media_stop")
            await wait_state("idle")

            # No voice specified: use the selected engine default, not Pi TTS_VOICE.
            await hass.services.async_call("tts", "speak", {
                "entity_id": engines[0], "media_player_entity_id": entity,
                "message": "Default engine voice", "cache": True,
            }, blocking=True)
            await wait_state("playing")
            assert (await recorded())["requests"][-1] == {
                "text": "Default engine voice", "voice": {"language": "en"},
            }
            await call("media_stop")
            await wait_state("idle")
            synthesize.assert_not_awaited()

            # Non-TTS media-source IDs still use ordinary HA URL resolution.
            from custom_components.hifiberry_tts import media_player as platform
            resolve = AsyncMock(return_value=SimpleNamespace(url=os.environ["MPD_TEST_AUDIO_URL"]))
            monkeypatch.setattr(platform.media_source, "async_resolve_media", resolve)
            await call("play_media", media_content_type="music", media_content_id="media-source://media_source/local/silent.wav")
            await wait_state("playing")
            resolve.assert_awaited_once()
            await call("media_stop")
            await wait_state("idle")
            # DedicatedHAbutton is registered on the same bridge device and
            # cancels every input path, including a cached MPD/URL item.
            await call("media_play")
            await wait_state("playing")
            buttons = hass.states.async_entity_ids("button")
            assert len(buttons) == 1 and hass.states.get(buttons[0]).state != "unavailable"
            await hass.services.async_call("button", "press", {"entity_id": buttons[0]}, blocking=True)
            result = await wait_state("idle")
            assert not result["has_media"] and result["pcm_buffered_ms"] == 0, result
            print("PASS: actual HA tts.speak sends text/server/voice/language/speaker directly to Rust; HA synthesis bypassed; default engine voice, controls and URL fallback verified")
            print("PASS: actual HAbutton.press cancels output and clears replay media")
        finally:
            async with http.post(f"http://{HOST}:{os.environ['MPD_TEST_HTTP_PORT']}/v1/stop",
                                 headers={"Authorization": "Bearer " + token}):
                pass
            for entry in reversed(entries):
                await hass.config_entries.async_unload(entry.entry_id)
            await hass.async_stop(force=True)
