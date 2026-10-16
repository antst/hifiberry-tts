"""Unmodified HA Wyoming integration + assist_satellite.announce, live bridge.

Set WYOMING_TEST_HOST, WYOMING_TEST_PORT and WYOMING_TEST_AUDIO_URL.
The HA process needs ffmpeg available in PATH.
"""
import asyncio
import os
import pytest
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
    finally:
        if entry:await hass.config_entries.async_unload(entry.entry_id)
        await hass.async_stop(force=True)
