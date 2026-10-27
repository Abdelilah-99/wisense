# Setup guide: from a fresh Ubuntu machine to the first alert

This walks through installing homeguard on a hub running a fresh
**Ubuntu 24.04 LTS** (Server or Desktop), step by step, with every command
explained. Allow an hour or two; most of it is waiting for downloads and
pairing devices.

You need: the hub (see [hardware.md](hardware.md)), wired to your router;
another computer or phone on the same network; and, for the first alert, a
Telegram account. Cameras, Zigbee devices and phones can be added later;
skip their steps until you have them.

Commands are run on the hub, in a terminal (or over SSH). Lines starting
with `#` are comments.

## 1. Update the system

```sh
sudo apt update && sudo apt upgrade -y
sudo apt install -y git curl openssl
sudo apt install -y unattended-upgrades   # automatic security updates
```

`apt update` refreshes the package list, `apt upgrade` installs available
updates, and `unattended-upgrades` keeps applying security updates by itself.

## 2. Give the hub a fixed address

The dashboard, the Frigate UI and the cameras' routes all rely on the hub's
IP address staying the same. In your router's admin page, find the DHCP
settings and create a **reservation** (sometimes "static lease") for the
hub. Then find the address:

```sh
ip -4 addr show scope global
```

Look for `inet 192.168.x.y/24` on the wired interface (`eth0`, `enp…`). That
`192.168.x.y` is your `HUB_LAN_IP` below.

## 3. Install Docker Engine

Install Docker from Docker's own repository (Ubuntu's `docker.io` package
lags behind and has no Compose v2 plugin):

```sh
# Docker's signing key and repository
sudo install -m 0755 -d /etc/apt/keyrings
sudo curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc
sudo chmod a+r /etc/apt/keyrings/docker.asc
echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] \
https://download.docker.com/linux/ubuntu $(. /etc/os-release && echo "$VERSION_CODENAME") stable" \
  | sudo tee /etc/apt/sources.list.d/docker.list > /dev/null

# Docker Engine and the Compose plugin
sudo apt update
sudo apt install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin

# Check it works
sudo docker run --rm hello-world
docker compose version
```

So that you do not need `sudo` for every Docker command, add yourself to the
`docker` group, then log out and back in:

```sh
sudo usermod -aG docker "$USER"
```

Being in the `docker` group is equivalent to having root on the hub; only add
accounts you trust.

Docker starts at boot by itself (`systemctl is-enabled docker` says
`enabled`), and the stack's `restart: unless-stopped` brings every service
back after a reboot.

## 4. Get homeguard

```sh
cd ~
git clone https://github.com/Abdelilah-99/wisense.git
cd wisense/homeguard
```

Everything below happens in this `homeguard` directory.

## 5. Fill in `.env`

`.env` holds every secret. It is never committed.

```sh
cp .env.example .env
chmod 600 .env        # readable by you only
nano .env             # or your editor of choice
```

Set, at least:

| Variable | Value |
|---|---|
| `HUB_LAN_IP` | the address from step 2 |
| `TZ` | your time zone, e.g. `Africa/Casablanca` |
| `HOMEGUARD_UID`, `HOMEGUARD_GID` | the output of `id -u` and `id -g` |
| `MQTT_*_PASSWORD` | a different generated password each: `openssl rand -base64 24` |
| `HOMEGUARD_DASHBOARD_PASSWORD` | another generated password |
| `HOMEGUARD_DISARM_PIN` | a PIN you can type on a phone: `shuf -i 100000-999999 -n 1` |
| `Z2M_FRONTEND_TOKEN` | another generated password |

Leave the camera, Telegram and phone values for their steps below.

## 6. Initialize and start

```sh
scripts/init.sh
```

This creates `data/` (recordings, databases, Zigbee state), generates the
Mosquitto password file from the `MQTT_*` pairs in `.env` (using the
Mosquitto image, so nothing to install), and copies the Zigbee2MQTT
configuration template into `data/zigbee2mqtt/`.

```sh
docker compose up -d
docker compose ps
```

`up -d` downloads the images (a few GB the first time, mostly Frigate),
builds homeguard, and starts everything in the background. `ps` lists the
services; after a minute or two each should say `healthy`, except:

- **zigbee2mqtt** restarts in a loop until a Zigbee dongle is attached
  (step 9). Stop it until then: `docker compose stop zigbee2mqtt`.
- **frigate** is unhealthy until a camera is configured (step 8).

Logs, when something is not right:

```sh
docker compose logs -f homeguard     # Ctrl-C to stop following
docker compose logs --tail 50 frigate
```

Open the dashboard from another device: `http://HUB_LAN_IP:8080`. Log in with
any user name and `HOMEGUARD_DASHBOARD_PASSWORD`.

## 7. Telegram: the bot and your chat id

1. In Telegram, open [@BotFather](https://t.me/BotFather), send `/newbot`,
   choose a display name and a user name ending in `bot`. Copy the token it
   returns (`123456789:AA…`) into `.env` as `TELEGRAM_BOT_TOKEN`.
2. Open a chat with your new bot and send it any message (bots cannot write
   first).
3. Find your chat id. On the hub:

   ```sh
   TOKEN=$(grep '^TELEGRAM_BOT_TOKEN=' .env | cut -d= -f2-)
   curl -s "https://api.telegram.org/bot$TOKEN/getUpdates" | grep -o '"chat":{"id":-\?[0-9]*'
   ```

   It prints `"chat":{"id":123456789`. Repeat for every household member
   (each sends a message to the bot first). For a group, add the bot to the
   group and send a message there; group ids are negative.
4. Put the ids in `.env`, comma-separated: `TELEGRAM_CHAT_IDS=123456789,987654321`.
5. Apply and test:

   ```sh
   docker compose up -d homeguard     # recreates it with the new .env
   ```

   Send `/status` to the bot. It answers with the arm state.

### Or (and) Discord

To get alerts in a Discord channel, and optionally command homeguard from
Discord, follow the README's "Discord" section: a channel webhook for alerts
(`DISCORD_WEBHOOK_URL`), a bot with the Message Content intent for commands
(`DISCORD_BOT_TOKEN`, `DISCORD_ALLOWED_USER_IDS`). Then check every channel
at once:

```sh
docker compose up -d homeguard
docker compose run --rm --no-deps homeguard test-alert
```

## 8. Add a camera

1. Give the camera a fixed IP (a DHCP reservation, as in step 2), enable RTSP
   on it (see [hardware.md](hardware.md#2-cameras)), and create a viewer
   account if it supports one.
2. In `.env`: `FRIGATE_CAMERA_FRONT_HOST` (its IP), `FRIGATE_RTSP_USER`,
   `FRIGATE_RTSP_PASSWORD`.
3. In `config/frigate/config.yml`, adjust the RTSP path for your brand and the
   `detect` width/height to the substream's resolution. Check the URL first
   with any video player, e.g.
   `ffplay rtsp://user:pass@192.168.1.20:554/stream2` from a computer.
4. Restart Frigate and open its UI at `https://HUB_LAN_IP:8971` (accept the
   self-signed certificate). The first admin password is in the logs:

   ```sh
   docker compose up -d frigate
   docker compose logs frigate | grep -i password
   ```

5. The camera's id (`front_cam` in the example) must match a `[[cameras]]`
   entry in `config/homeguard.toml`, with its zone. To add more cameras, copy
   the camera block in both files and add an environment variable for each
   address in `docker-compose.yml` and `.env`.

## 9. Pair Zigbee devices

1. Plug the coordinator into a USB extension cable, then into the hub. Find
   its stable name:

   ```sh
   ls -l /dev/serial/by-id/
   ```

2. In `docker-compose.yml`, under `zigbee2mqtt`, uncomment `devices:` and put
   that path on the left: `- /dev/serial/by-id/usb-ITead_…-if00-port0:/dev/ttyACM0`.
   For a Sonoff ZBDongle-E, also set `adapter: ember` in
   `data/zigbee2mqtt/configuration.yaml`.
3. Start it and watch it come up:

   ```sh
   docker compose up -d zigbee2mqtt
   docker compose logs -f zigbee2mqtt
   ```

4. Open its frontend. It listens on the hub's loopback only, so tunnel to it
   from your computer:

   ```sh
   ssh -L 8081:127.0.0.1:8081 you@HUB_LAN_IP
   ```

   then browse to `http://localhost:8081` and log in with `Z2M_FRONTEND_TOKEN`.
5. Click **Permit join** (it closes by itself after a few minutes), then put
   each device in pairing mode (usually: hold its button for 5 seconds).
6. **Rename each device to its homeguard sensor id** (e.g. `front_door`,
   `hall_pir`, `kitchen_smoke`). Then no `[zigbee.devices]` mapping is needed.
7. Add each one to `config/homeguard.toml` (`[[sensors]]` with `id`, `kind`,
   `zone`), check the file, and restart homeguard:

   ```sh
   docker compose run --rm --no-deps homeguard check-config
   docker compose restart homeguard
   ```

8. Open and close a door: the dashboard shows it, and the event log lists it.
   For a siren or plugs, name them in `[actuators]`.

## 10. Phones (presence)

On each phone, for **your home WiFi network only**, turn off MAC
randomization and read the phone's WiFi MAC address:

- **Android**: Settings → Network & internet → Internet → (gear next to your
  network) → Privacy → *Use device MAC*. The MAC is shown on the same screen
  (or in About phone → Wi-Fi MAC address).
- **iPhone**: Settings → Wi-Fi → (i) next to your network → *Private Wi-Fi
  Address* off (or *Fixed*). The address is shown below it.

Then reconnect the phone to the WiFi, and for each person:

1. In `.env`: `HOMEGUARD_PHONE_ALICE=aa:bb:cc:dd:ee:ff` (one variable each).
2. In `config/homeguard.toml`, uncomment and fill a `[[presence.people]]`
   entry with `name` and `mac_env`. A DHCP reservation for the phone plus
   `ip = "..."` makes the first sighting faster.
3. `docker compose up -d homeguard`; the dashboard's People list shows who is
   home. `auto_arm` and `auto_disarm` stay off until you turn them on.

## 11. wisense and its bridge (systemd)

If the hub has a WiFi card and you want WiFi motion detection, follow
["wisense bridge"](../README.md#wisense-bridge) in the README. In short:
install Rust (`curl https://sh.rustup.rs -sSf | sh`), build wisense and the
bridge, run `wisense calibrate` with the room empty, put the MQTT login in
`/etc/homeguard/wisense-bridge.env`, and enable the
`homeguard-wisense` systemd service. Declare the link in
`config/homeguard.toml` (`kind = "wisense"` and `[wisense.links]`).

## 12. The first alert

First, see the whole chain without touching anything real, from a computer
with Rust installed (or on the hub after step 11):

```sh
cargo run -- simulate scenarios/burglar_front_door.toml
```

Then for real, with a door sensor paired:

1. Send `/arm away` to the bot. It replies that it is arming; one minute later
   (the exit delay) you get "Armed (away)".
2. Open the front door. You get a ⚠️ "Entry: Front door opened. Disarm within
   30s…" alert.
3. Wait 30 seconds without disarming: 🚨 "ALARM", the siren and lights if you
   have them.
4. `/disarm` (or the dashboard with the PIN). Done: it works end to end.

To test without alerting the household, set short delays in
`config/homeguard.toml` and run `homeguard run --dry-run` instead of the
service, or try the scenarios in `scenarios/`.

## 13. Finish the job

- Work through [hardening.md](hardening.md): router passwords, cameras off the
  internet, VPN for remote access, backups, UPS.
- Read [privacy.md](privacy.md) and tell everyone at home what is monitored.
- Put a reminder in your calendar to test the alarm and check backups every
  few months.

## Updating

```sh
cd ~/wisense/homeguard
git pull
docker compose build homeguard
docker compose up -d
```

To move to newer Frigate, Zigbee2MQTT or Mosquitto versions, change the
pinned tags in `docker-compose.yml` after reading their release notes, then
`docker compose pull && docker compose up -d`.

## A note on the firewall

If you enable `ufw`, know that **Docker publishes ports by writing its own
iptables rules, which bypass ufw**. homeguard's stack already binds every
published port to `HUB_LAN_IP` or to loopback, so nothing is exposed beyond
the LAN, but do not rely on ufw to hide a Docker port you publish yourself.
