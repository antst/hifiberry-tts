"""Home Assistant glue for the Rust HiFiBerry TTS bridge."""
from datetime import timedelta
import logging
from homeassistant.const import Platform
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.update_coordinator import DataUpdateCoordinator, UpdateFailed
from .api import BridgeApi, BridgeAuthError, BridgeError
from .const import CONF_TOKEN, DOMAIN

PLATFORMS = [Platform.MEDIA_PLAYER, Platform.BUTTON]


async def async_setup_entry(hass, entry):
    api = BridgeApi(async_get_clientsession(hass), entry.data["host"],
                    entry.data["port"], entry.data[CONF_TOKEN])

    async def update():
        try:
            return await api.request()
        except BridgeAuthError as err:
            from homeassistant.exceptions import ConfigEntryAuthFailed
            raise ConfigEntryAuthFailed(str(err)) from err
        except BridgeError as err:
            raise UpdateFailed(str(err)) from err

    coordinator = DataUpdateCoordinator(
        hass, logging.getLogger(__name__), name=DOMAIN, config_entry=entry,
        update_method=update, update_interval=timedelta(seconds=1),
    )
    await coordinator.async_config_entry_first_refresh()
    entry.runtime_data = (api, coordinator)
    await hass.config_entries.async_forward_entry_setups(entry, PLATFORMS)
    return True


async def async_unload_entry(hass, entry):
    return await hass.config_entries.async_unload_platforms(entry, PLATFORMS)
