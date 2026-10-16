"""Cancel speech and discard its buffers, regardless of the audio input path."""
from homeassistant.components.button import ButtonEntity
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from .const import DOMAIN


async def async_setup_entry(hass, entry, async_add_entities):
    api, coordinator = entry.runtime_data
    async_add_entities([CancelSpeechButton(entry, api, coordinator)])


class CancelSpeechButton(CoordinatorEntity, ButtonEntity):
    _attr_has_entity_name = True
    _attr_translation_key = "cancel_speech"
    _attr_icon = "mdi:stop-circle-outline"

    def __init__(self, entry, api, coordinator):
        super().__init__(coordinator)
        self.api = api
        self._attr_unique_id = f"{entry.entry_id}_cancel_speech"
        self._attr_device_info = {
            "identifiers": {(DOMAIN, entry.entry_id)}, "name": entry.title,
            "manufacturer": "Community", "model": "Rust HiFiBerry TTS Bridge",
        }

    @property
    def available(self):
        return super().available and self.coordinator.data.get("supports_cancel", False)

    async def async_press(self):
        await self.api.request("cancel", {})
        await self.coordinator.async_request_refresh()
