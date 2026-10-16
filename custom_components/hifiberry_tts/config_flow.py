"""Set up the bridge using its own HTTP API, not the MPD or Wyoming port."""
import probatio
from homeassistant.config_entries import ConfigFlow
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers import selector
from .api import BridgeApi, BridgeAuthError, BridgeError
from .const import CONF_TOKEN, DEFAULT_PORT, DOMAIN


class BridgeConfigFlow(ConfigFlow, domain=DOMAIN):
    VERSION = 1

    async def async_step_user(self, user_input=None):
        errors = {}
        if user_input:
            host = user_input["host"].strip()
            port = user_input["port"]
            if not host or not 1 <= port <= 65535 or "://" in host or "/" in host:
                errors["base"] = "cannot_connect"
            else:
                self._async_abort_entries_match({"host": host, "port": port})
                api = BridgeApi(async_get_clientsession(self.hass), host, port,
                                user_input[CONF_TOKEN])
                try:
                    await api.request()
                except BridgeAuthError:
                    errors["base"] = "invalid_auth"
                except (BridgeError, ValueError):
                    errors["base"] = "cannot_connect"
                else:
                    return self.async_create_entry(
                        title=user_input["name"], data={**user_input, "host": host},
                    )
        return self.async_show_form(
            step_id="user", errors=errors,
            data_schema=probatio.Schema({
                probatio.Required("host"): str,
                probatio.Required("port", default=DEFAULT_PORT): int,
                probatio.Required(CONF_TOKEN): selector.TextSelector(
                    selector.TextSelectorConfig(type=selector.TextSelectorType.PASSWORD)),
                probatio.Required("name", default="Kitchen Audio"): str,
            }),
        )
