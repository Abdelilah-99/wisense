#!/usr/bin/env bash
# One-time setup on the hub, from homeguard/: creates the data directories,
# the Mosquitto password file and the Zigbee2MQTT working configuration.
# Safe to re-run; it never overwrites existing data.
set -euo pipefail
cd "$(dirname "$0")/.."

[ -f .env ] || { echo "copy .env.example to .env and fill it in first" >&2; exit 1; }

mkdir -p data/homeguard data/frigate data/mosquitto data/zigbee2mqtt data/homeassistant

if [ ! -f data/zigbee2mqtt/configuration.yaml ]; then
  cp config/zigbee2mqtt/configuration.yaml data/zigbee2mqtt/configuration.yaml
  echo "copied the Zigbee2MQTT configuration template to data/zigbee2mqtt/"
fi

scripts/mqtt-passwd.sh
echo "done: now run 'docker compose up -d'"
