"""Authenticated transport to the Rust bridge; no synthesis in HA."""
from asyncio import timeout
from aiohttp import ClientError
from yarl import URL
from homeassistant.exceptions import HomeAssistantError


class BridgeError(HomeAssistantError):
    """Bridge unreachable, incompatible, or rejected the request."""


class BridgeAuthError(BridgeError):
    """Incorrect bridge token."""


class BridgeApi:
    def __init__(self, session, host, port, token):
        self.session = session
        self.base = URL.build(scheme="http", host=host, port=port)
        self.headers = {"Authorization": f"Bearer {token}"}

    async def request(self, endpoint="status", data=None):
        try:
            async with timeout(5):
                async with self.session.request(
                    "GET" if endpoint == "status" else "POST",
                    self.base / "v1" / endpoint, headers=self.headers, json=data,
                ) as response:
                    if response.status == 401:
                        raise BridgeAuthError("Incorrect bridge API token")
                    if response.status not in (200, 202):
                        raise BridgeError(f"Bridge returned HTTP {response.status}")
                    result = await response.json()
        except (ClientError, TimeoutError, ValueError) as err:
            raise BridgeError("Cannot communicate with HiFiBerry TTS bridge") from err
        if not isinstance(result, dict):
            raise BridgeError("Invalid bridge response")
        if endpoint == "status" and (
            result.get("product") != "hifiberry-tts"
            or "pcm_buffer_limit_ms" not in result
        ):
            raise BridgeError("HiFiBerry TTS bridge 0.3.0 or newer is required")
        return result
