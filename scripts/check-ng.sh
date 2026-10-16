#!/bin/sh
# Read-only checks. Run as the NG player user while background audio is playing.
set -u
echo "OS:"
cat /etc/os-release
echo "Architecture: $(uname -m)"
echo "Player user: $(cat /etc/hifiberry.user 2>/dev/null || id -un)"
echo "Current user: $(id -un), runtime: ${XDG_RUNTIME_DIR:-unset}"
echo "User audio services:"
systemctl --user --no-pager status pipewire.service wireplumber.service 2>&1 || true
echo "System PipeWire service (for installations using a system instance):"
systemctl --no-pager status pipewire.service 2>&1 || true
echo "PipeWire version:"
pw-cli --version 2>&1 || true
echo "Playback streams, running state, binary, and channel gain:"
task_dump=$(mktemp)
trap 'rm -f "$task_dump"' EXIT HUP INT TERM
if pw-dump > "$task_dump"; then
    python3 - "$task_dump" <<'PY'
import json, sys
objects = json.load(open(sys.argv[1]))
clients = {n['id']: n.get('info', {}).get('props', {}) for n in objects if n['type'] == 'PipeWire:Interface:Client'}
for node in objects:
    info = node.get("info", {})
    props = info.get("props", {})
    if props.get("media.class") == "Stream/Output/Audio":
        gains = [p.get("channelVolumes") for p in info.get("params", {}).get("Props", []) if "channelVolumes" in p]
        client = clients.get(props.get('client.id'), {})
        group = props.get('node.link-group')
        shared = bool(group and any(n.get('info', {}).get('props', {}).get('media.class') == 'Audio/Sink'
            and n.get('info', {}).get('props', {}).get('node.link-group') == group for n in objects))
        print({"id": node["id"], "serial": props.get("object.serial"), "state": info.get("state"),
               "binary": props.get("application.process.binary", client.get("application.process.binary")),
               "name": props.get("application.name"), "shared_processing_stage": shared, "gain": gains})
PY
else
    echo "Cannot reach PipeWire. Use the same user/runtime/remote as NG's players." >&2
    exit 1
fi
