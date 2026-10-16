"""Forward normal tts.speak text to Rust before resolving it to an audio URL."""
from yarl import URL
from homeassistant.components import media_source
from homeassistant.components.media_player import (
    MediaPlayerEntity, MediaPlayerEntityFeature, MediaPlayerState, MediaType,
    async_process_play_media_url,
)
from homeassistant.components.tts.const import DATA_TTS_MANAGER
from homeassistant.components.tts.helper import get_engine_instance
from homeassistant.components.tts.media_source import parse_media_source_id
from homeassistant.helpers.update_coordinator import CoordinatorEntity
from homeassistant.util import dt as dt_util
from .const import DOMAIN

FEATURES = (MediaPlayerEntityFeature.PLAY_MEDIA | MediaPlayerEntityFeature.PLAY
            | MediaPlayerEntityFeature.PAUSE | MediaPlayerEntityFeature.STOP
            | MediaPlayerEntityFeature.VOLUME_SET | MediaPlayerEntityFeature.VOLUME_MUTE)


async def async_setup_entry(hass, entry, async_add_entities):
    api, coordinator = entry.runtime_data
    async_add_entities([BridgePlayer(entry, api, coordinator)])


class BridgePlayer(CoordinatorEntity, MediaPlayerEntity):
    _attr_has_entity_name = True
    _attr_name = None
    _attr_media_content_type = MediaType.MUSIC

    def __init__(self, entry, api, coordinator):
        super().__init__(coordinator)
        self.api = api
        self._attr_unique_id = entry.entry_id
        self._attr_device_info = {
            "identifiers": {(DOMAIN, entry.entry_id)}, "name": entry.title,
            "manufacturer": "Community", "model": "Rust HiFiBerry TTS Bridge",
        }
        self._unmuted_volume = 1.0

    @property
    def state(self):
        return {"idle": MediaPlayerState.IDLE, "buffering": MediaPlayerState.BUFFERING,
                "playing": MediaPlayerState.PLAYING, "paused": MediaPlayerState.PAUSED
                }.get(self.coordinator.data["state"], MediaPlayerState.IDLE)

    @property
    def supported_features(self):
        return FEATURES | (MediaPlayerEntityFeature.SEEK
                           if self.coordinator.data.get("duration") is not None else 0)

    @property
    def volume_level(self):
        return self.coordinator.data["volume_level"]

    @property
    def is_volume_muted(self):
        return self.volume_level == 0

    @property
    def media_duration(self):
        return self.coordinator.data.get("duration")

    @property
    def media_position(self):
        return self.coordinator.data.get("position")

    @property
    def media_position_updated_at(self):
        return self._position_updated_at

    async def async_added_to_hass(self):
        self._position_updated_at = dt_util.utcnow()
        await super().async_added_to_hass()

    def _handle_coordinator_update(self):
        self._position_updated_at = dt_util.utcnow()
        super()._handle_coordinator_update()

    @property
    def extra_state_attributes(self):
        return {key: self.coordinator.data.get(key) for key in (
            "last_error", "duck_warning", "ducked_streams", "pcm_buffered_ms",
            "pcm_underruns", "pcm_prebuffer_ms", "pcm_buffer_limit_ms")}

    async def _command(self, endpoint, data=None):
        await self.api.request(endpoint, data or {})
        # Commands are queued asynchronously; normal polling observes completion.
        await self.coordinator.async_request_refresh()

    async def async_media_play(self):
        await self._command("play")

    async def async_media_pause(self):
        await self._command("pause", {"paused": True})

    async def async_media_stop(self):
        await self._command("stop")

    async def async_set_volume_level(self, volume):
        await self._command("volume", {"volume_level": volume})

    async def async_mute_volume(self, mute):
        if mute:
            if self.volume_level > 0:
                self._unmuted_volume = self.volume_level
            await self.async_set_volume_level(0)
        else:
            await self.async_set_volume_level(self._unmuted_volume)

    async def async_media_seek(self, position):
        await self._command("seek", {"position": position})

    async def async_play_media(self, media_type, media_id, **kwargs):
        if media_id.startswith("media-source://tts/"):
            parsed = parse_media_source_id(media_id)
            if "stream" not in parsed:
                opts = parsed["options"]
                engine = get_engine_instance(self.hass, opts["engine"])
                entry = getattr(engine, "config_entry", None)
                if entry is not None and entry.domain == "wyoming":
                    language, options = self.hass.data[DATA_TTS_MANAGER].process_options(
                        engine, opts["language"], opts["options"])
                    # Audio format hints apply to HA's encoder, bypassed here.
                    # Unsupported provider-specific output modes use HA's normal path.
                    if options.get("audio_output") is None:
                        service = engine.service
                        server = URL.build(scheme="tcp", host=service.host,
                                           port=service.port).authority
                        await self._command("speak", {
                            "text": parsed["message"], "voice": options.get("voice"),
                            "speaker": options.get("speaker"), "language": language,
                            "server": server,
                        })
                        return
        if media_source.is_media_source_id(media_id):
            resolved = await media_source.async_resolve_media(self.hass, media_id, self.entity_id)
            media_id = resolved.url
        if media_type != "text" and not media_id.startswith("wyoming://"):
            media_id = async_process_play_media_url(self.hass, media_id)
        await self._command("play_media", {
            "url": media_id, "volume_level": self.volume_level,
        })
