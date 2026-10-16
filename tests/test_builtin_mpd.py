"""Integration test using unmodified Home Assistant MPD against the real bridge.

Set MPD_TEST_HOST, MPD_TEST_PASSWORD, and MPD_TEST_AUDIO_URL (a >=10s silent
WAV reachable from the Pi). MPD_TEST_PORT defaults to 6601. Requires HA
2026.10.1, python-mpd2 3.1.1, pytest-asyncio and Python 3.14. No custom HA code.
"""

import asyncio
import os
from types import SimpleNamespace
from unittest.mock import AsyncMock

from aiohttp import ClientSession
import pytest
from homeassistant import auth, bootstrap, loader
from homeassistant.config_entries import ConfigEntries
from homeassistant.core import HomeAssistant
from homeassistant.setup import async_setup_component

HOST = os.getenv("MPD_TEST_HOST")
HTTP_PORT = os.getenv("MPD_TEST_HTTP_PORT", "8090")
pytestmark = pytest.mark.skipif(not HOST, reason="Set MPD_TEST_* for a live bridge test")


@pytest.mark.asyncio
async def test_builtin_mpd_url_tts_pause_resume_volume_seek_and_replay(tmp_path, monkeypatch):
    token = os.environ["MPD_TEST_PASSWORD"]
    audio_url = os.environ["MPD_TEST_AUDIO_URL"]
    hass = HomeAssistant(str(tmp_path))
    hass.config.skip_pip = True
    hass.config.internal_url = "http://ha-test.local:8123"
    hass.config.external_url = "http://ha-test.local:8123"
    loader.async_setup(hass)
    hass.config_entries = ConfigEntries(hass, {})
    assert await bootstrap.async_load_base_functionality(hass)
    hass.auth = await auth.auth_manager_from_config(hass, [{"type": "homeassistant"}], [])
    entry = None
    async with ClientSession() as http:
        async def status():
            async with http.get(f"http://{HOST}:{HTTP_PORT}/v1/status", headers={"Authorization":"Bearer "+token}) as response:
                return await response.json()

        async def wait_state(state):
            for _ in range(100):
                result = await status()
                if result["state"] == state:
                    assert not result["last_error"], result
                    return result
                await asyncio.sleep(.05)
            raise AssertionError(f"Bridge did not reach {state}")

        try:
            assert await async_setup_component(hass, "homeassistant", {})
            result = await hass.config_entries.flow.async_init("mpd", context={"source":"user"})
            result = await hass.config_entries.flow.async_configure(result["flow_id"], {
                "host": HOST, "port": int(os.getenv("MPD_TEST_PORT", "6601")), "password":token,
            })
            assert result["type"] == "create_entry"
            entry = result["result"]
            await hass.async_block_till_done()
            entities = hass.states.async_entity_ids("media_player")
            if not entities:
                assert await hass.config_entries.async_setup(entry.entry_id)
                await hass.async_block_till_done()
                entities = hass.states.async_entity_ids("media_player")
            assert len(entities) == 1
            entity = entities[0]
            assert hass.states.get(entity).state != "unavailable"

            async def call(service, **data):
                await hass.services.async_call("media_player", service, {"entity_id":entity, **data}, blocking=True)

            await call("play_media", media_content_type="music", media_content_id=audio_url)
            await wait_state("playing")
            # Allow the output/IPC socket to finish opening.
            await asyncio.sleep(.25)
            await call("media_pause")
            result = await wait_state("paused")
            assert result["ducked_streams"] == 0
            await call("volume_set", volume_level=.35)
            for _ in range(50):
                result = await status()
                if result["volume_level"] == .35:
                    break
                await asyncio.sleep(.05)
            assert result["volume_level"] == .35
            await call("media_seek", seek_position=1.0)
            await asyncio.sleep(1.1)
            assert (await status())["position"] >= .8
            await call("media_play")
            await wait_state("playing")
            await call("media_stop")
            await wait_state("idle")
            await call("media_play")
            await wait_state("playing")
            await call("media_stop")
            await wait_state("idle")

            # HA's own MPD platform resolves the TTS media source, then passes
            # the playable URL over its standard clear/add/play sequence.
            from homeassistant.components.mpd import media_player as platform
            resolve = AsyncMock(return_value=SimpleNamespace(url=audio_url))
            monkeypatch.setattr(platform.media_source, "async_resolve_media", resolve)
            await call("play_media", media_content_type="music", media_content_id="media-source://tts/tts.piper?message=test")
            await wait_state("playing")
            assert resolve.await_count == 1
            await call("media_stop")
            await wait_state("idle")
            print("PASS: unmodified HA MPD config flow and entity; URL/TTS-source playback, pause/restore, volume, seek, resume and cached replay on the real NG device")
            if os.getenv("MPD_TEST_DIRECT_TTS"):
                await call("play_media", media_content_type="text", media_content_id='{"text":"Silent protocol test","voice":"test-voice"}')
                await wait_state("playing")
                await call("media_stop")
                await wait_state("idle")
                print("PASS: unmodified HA MPD forwards text plus per-request voice to the bridge's direct Wyoming client")
        finally:
            async with http.post(f"http://{HOST}:{HTTP_PORT}/v1/stop", headers={"Authorization":"Bearer "+token}):
                pass
            if entry:
                await hass.config_entries.async_unload(entry.entry_id)
            await hass.async_stop(force=True)
