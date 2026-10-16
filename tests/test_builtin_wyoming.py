"""Unmodified HA Wyoming integration + assist_satellite.announce, live bridge.

Set WYOMING_TEST_HOST, WYOMING_TEST_PORT and WYOMING_TEST_AUDIO_URL.
The HA process needs ffmpeg available in PATH.
"""
import asyncio
import os
import pytest
from aiohttp import ClientSession
from wyoming.client import AsyncTcpClient
from wyoming.info import Describe, Info
from homeassistant import auth, bootstrap, loader
from homeassistant.config_entries import ConfigEntries
from homeassistant.core import HomeAssistant
from homeassistant.setup import async_setup_component

HOST=os.getenv('WYOMING_TEST_HOST')
pytestmark=pytest.mark.skipif(not HOST,reason='Set WYOMING_TEST_* for live satellite test')

@pytest.mark.asyncio
async def test_builtin_wyoming_satellite_announcement(tmp_path):
    hass=HomeAssistant(str(tmp_path));hass.config.skip_pip=True
    hass.config.internal_url='http://ha-test.local:8123'
    loader.async_setup(hass);hass.config_entries=ConfigEntries(hass,{})
    assert await bootstrap.async_load_base_functionality(hass)
    hass.auth=await auth.auth_manager_from_config(hass,[{'type':'homeassistant'}],[])
    entry=None
    try:
        async with AsyncTcpClient(HOST, int(os.environ['WYOMING_TEST_PORT'])) as client:
            await client.write_event(Describe().event())
            description = Info.from_event(await client.read_event())
            assert description.satellite and description.snd
            assert description.tts and description.tts[0].voices[0].name == 'test-voice'
            assert description.tts[0].voices[0].description == 'Calm narration — спокойная речь.'
            assert not description.tts[0].supports_synthesize_streaming
        assert await async_setup_component(hass,'homeassistant',{})
        result=await hass.config_entries.flow.async_init('wyoming',context={'source':'user'})
        result=await hass.config_entries.flow.async_configure(result['flow_id'],{'host':HOST,'port':int(os.environ['WYOMING_TEST_PORT'])})
        assert result['type']=='create_entry',result
        entry=result['result']
        await hass.async_block_till_done()
        if not hass.states.async_entity_ids('assist_satellite'):
            assert await hass.config_entries.async_setup(entry.entry_id)
            await hass.async_block_till_done()
        entities=hass.states.async_entity_ids('assist_satellite')
        assert len(entities)==1,entities
        entity=entities[0]
        for _ in range(100):
            state=hass.states.get(entity)
            if state and state.state not in ('unavailable','unknown'):break
            await asyncio.sleep(.05)
        await asyncio.sleep(.2)
        await hass.services.async_call('assist_satellite','announce',{'entity_id':entity,'media_id':os.environ['WYOMING_TEST_AUDIO_URL'],'preannounce':False},blocking=True)
        assert hass.states.get(entity).state=='idle',hass.states.get(entity)
        print('PASS: unmodified HA Wyoming config flow, satellite entity and assist_satellite.announce via FFmpeg/PCM on the real NG device')
        if os.getenv('MPD_TEST_PASSWORD'):
            async with ClientSession() as http:
                base=f"http://{HOST}:{os.environ['MPD_TEST_HTTP_PORT']}/v1/"
                headers={'Authorization':'Bearer '+os.environ['MPD_TEST_PASSWORD']}
                async def status():
                    async with http.get(base+'status',headers=headers) as response:
                        return await response.json()
                # Cancel two successive announcements, verifying HA receives
                # completion and keeps its connection for another announcement.
                for _ in range(2):
                    for attempt in range(200):
                        if hass.states.get(entity).state=='idle':break
                        await asyncio.sleep(.05)
                    pending=asyncio.create_task(hass.services.async_call('assist_satellite','announce',{'entity_id':entity,'media_id':os.environ['WYOMING_TEST_AUDIO_URL'],'preannounce':False},blocking=True))
                    for attempt in range(200):
                        current=await status()
                        if current['state']=='playing' and current['pcm_buffered_ms']>10000:break
                        assert not pending.done(),pending.exception() if pending.done() else current
                        await asyncio.sleep(.025)
                    else:raise AssertionError(current)
                    async with http.post(base+'cancel',headers=headers,json={}) as response:
                        assert response.status==202
                    await asyncio.wait_for(pending,5)
                    for attempt in range(100):
                        current=await status()
                        if current['state']=='idle':break
                        await asyncio.sleep(.025)
                    assert current['state']=='idle' and current['pcm_buffered_ms']==0 and not current['has_media'],current
                print('PASS: HA satellite announcements cancel promptly, complete their service call and accept a subsequent announcement')
    finally:
        if entry:await hass.config_entries.async_unload(entry.entry_id)
        await hass.async_stop(force=True)
