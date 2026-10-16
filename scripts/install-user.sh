#!/bin/sh
set -eu

if [ "$(uname -s)" != Linux ] || [ "$(id -u)" -eq 0 ]; then
    echo "Run as the normal HiFiBerryOS-NG player user on the Pi, not root." >&2
    exit 1
fi
if [ -f /etc/hifiberry.user ] && [ "$(cat /etc/hifiberry.user)" != "$(id -un)" ]; then
    echo "Log in as the player user named in /etc/hifiberry.user first." >&2
    exit 1
fi

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_dir=$(dirname "$script_dir")
binary=${1:-"$project_dir/target/release/hifiberry-tts"}
if [ ! -x "$binary" ]; then
    echo "Build with cargo build --release --locked, or pass the path to a Linux binary." >&2
    exit 1
fi
for program in pw-dump pw-cli openssl systemctl; do
    if ! command -v "$program" >/dev/null 2>&1; then
        echo "Missing dependency: $program. See README.md." >&2
        exit 1
    fi
done
if ! pw-dump >/dev/null 2>&1; then
    echo "Cannot connect to PipeWire as this user. Run scripts/check-ng.sh." >&2
    exit 1
fi

mkdir -p "$HOME/.local/bin" "$HOME/.config/hifiberry-tts" "$HOME/.config/systemd/user"
chmod 700 "$HOME/.config/hifiberry-tts"
install -m 755 "$binary" "$HOME/.local/bin/hifiberry-tts"
install -m 644 "$project_dir/deploy/hifiberry-tts.service" "$HOME/.config/systemd/user/hifiberry-tts.service"
env_file="$HOME/.config/hifiberry-tts/env"
if [ ! -f "$env_file" ]; then
    umask 077
    bridge_token=$(openssl rand -hex 32)
    sed "s/REPLACE_WITH_A_RANDOM_TOKEN_OF_AT_LEAST_24_CHARACTERS/$bridge_token/" "$project_dir/deploy/env.example" > "$env_file"
    unset bridge_token
fi
chmod 600 "$env_file"
systemctl --user daemon-reload
systemctl --user enable hifiberry-tts.service
systemctl --user restart hifiberry-tts.service
echo "Installed. Add HA's built-in MPD integration at PI-IP:6601; password is API_TOKEN from $env_file."
echo "For announcements also add HA's Wyoming Protocol integration at PI-IP:10700."
echo "Set MEDIA_HOSTS in that file, then run: systemctl --user restart hifiberry-tts"
echo "For boot startup, ensure linger: sudo loginctl enable-linger $(id -un)"
