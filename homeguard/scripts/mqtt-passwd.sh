#!/usr/bin/env bash
# Generates config/mosquitto/passwd from the MQTT_*_USER / MQTT_*_PASSWORD
# pairs in .env, using mosquitto_passwd from the pinned Mosquitto image (so
# nothing needs to be installed on the host). Re-run after changing a
# password, then `docker compose restart mosquitto`.
set -euo pipefail
cd "$(dirname "$0")/.."

IMAGE=eclipse-mosquitto:2.0.22
ENV_FILE=${ENV_FILE:-.env}
OUT=config/mosquitto/passwd

[ -f "$ENV_FILE" ] || { echo "missing $ENV_FILE (copy .env.example first)" >&2; exit 1; }

# Read KEY=VALUE lines without executing the file.
get() {
  local line
  line=$(grep -E "^$1=" "$ENV_FILE" | tail -n 1 || true)
  line=${line#*=}
  line=${line%\"}; line=${line#\"}
  printf '%s' "$line"
}

accounts=()
for who in HOMEGUARD FRIGATE Z2M HA; do
  user=$(get "MQTT_${who}_USER")
  pass=$(get "MQTT_${who}_PASSWORD")
  if [ -z "$user" ] && [ -z "$pass" ]; then
    [ "$who" = HA ] && continue   # Home Assistant is optional
    echo "MQTT_${who}_USER / MQTT_${who}_PASSWORD are not set in $ENV_FILE" >&2; exit 1
  fi
  case "$pass" in
    ""|change-me*) echo "set a real MQTT_${who}_PASSWORD in $ENV_FILE first" >&2; exit 1 ;;
  esac
  accounts+=("$user" "$pass")
done

mkdir -p "$(dirname "$OUT")"
# Passwords go through stdin, not argv (argv is visible in `ps`). The file
# ends up owned by the broker's user (uid 1883) with mode 0600.
printf '%s\n' "${accounts[@]}" | docker run --rm -i \
  -v "$PWD/$(dirname "$OUT"):/work" "$IMAGE" sh -c '
  set -e
  rm -f /work/passwd.new
  (umask 077; touch /work/passwd.new)
  while read -r user && read -r pass; do
    mosquitto_passwd -b /work/passwd.new "$user" "$pass"
  done
  chown mosquitto:mosquitto /work/passwd.new
  chmod 0600 /work/passwd.new
  mv /work/passwd.new /work/passwd
'
echo "wrote $OUT with $(( ${#accounts[@]} / 2 )) accounts"
