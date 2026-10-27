# Hardening guide

A security system is only as strong as its weakest part. This guide lists
what to do, roughly in order of importance. Most steps take minutes.

## 1. Change every default password

- **Cameras**: set a strong, unique admin password on each camera, and create
  a separate low-privilege user for Frigate if the camera allows it (the
  `FRIGATE_RTSP_USER` account only needs to view streams).
- **Router**: change its admin password, and turn off remote administration
  from the internet.
- **homeguard**: every `change-me` in `.env` must be replaced. Generate
  passwords instead of inventing them:

  ```sh
  openssl rand -base64 24      # MQTT passwords, dashboard password
  shuf -i 100000-999999 -n 1   # a 6-digit disarm PIN
  ```

- **Frigate**: it prints a generated admin password on first start
  (`docker compose logs frigate | grep -i password`). Log in and change it.
- **Zigbee2MQTT frontend**: set `Z2M_FRONTEND_TOKEN`.

`scripts/mqtt-passwd.sh` refuses to run while an MQTT password is still
`change-me`.

## 2. Update firmware and software

- **Cameras and router**: check for firmware updates when you install them,
  then every few months. Camera firmware fixes real vulnerabilities.
- **Zigbee devices**: Zigbee2MQTT can update many devices over the air (OTA tab
  in its frontend).
- **The hub**: enable unattended security updates
  (`sudo apt install unattended-upgrades`), and update the containers by
  bumping the pinned versions in `docker-compose.yml`, reading each project's
  release notes, then `docker compose pull && docker compose up -d`.

## 3. Never port-forward the hub

Do not open any port of the hub (or of a camera) to the internet on your
router. Not the dashboard, not Frigate, not MQTT. Exposed cameras and NVRs
are scanned and attacked constantly.

For access from outside, use a VPN that connects your phone to your home
network:

- **[Tailscale](https://tailscale.com/)**: install it on the hub and your
  phone, log in on both, and open `http://<hub's Tailscale IP>:8080`. Nothing
  to open on the router.
- **WireGuard**: run it on your router (many support it) or on the hub, and
  connect from the phone's WireGuard app.

Telegram alerts work without any of this: homeguard connects *out* to
Telegram, nothing comes in.

## 4. Isolate the cameras

Cameras should talk to Frigate and nothing else. Many phone home to their
manufacturer's cloud or run old, vulnerable software.

- **Simplest**: in your router, **block internet access** for each camera's IP
  (often called "parental controls", "access control" or a firewall rule).
  Frigate still reaches them on the LAN.
- **Better**: put cameras (and other IoT devices) on a **separate network**,
  either the router's guest network or a dedicated VLAN, that cannot reach
  your computers and phones. The hub needs to reach that network (Frigate
  pulls the streams); the cameras never need to reach the hub's other ports.
- Turn off features you do not use on the cameras: UPnP, P2P/cloud access,
  and their own email/FTP alerts.
- Set cameras to use your router (or a public NTP server) for time, so
  recordings are timestamped correctly.

## 5. MQTT authentication

Already done by the stack: Mosquitto refuses anonymous clients, every
service has its own account, and the broker is published on the hub's
loopback only (and on the LAN only with the `mqtt-lan` profile).

To go further, restrict what each account may publish with a Mosquitto ACL
file (`acl_file` in `config/mosquitto/mosquitto.conf`), for example so only
`zigbee2mqtt` may publish under `zigbee2mqtt/#`. That stops a compromised
camera account from faking a door sensor. Anyone holding the `homeguard`
account can inject events, so keep `.env` readable by you only:

```sh
chmod 600 .env
```

## 6. The dashboard and Telegram

- The dashboard listens on `HUB_LAN_IP` only, needs a password, and locks a
  client out after five wrong passwords or PINs. Its login travels unencrypted
  on your LAN, which is one more reason to keep untrusted devices on a separate
  network.
- Keep `TELEGRAM_CHAT_IDS` short: anyone in an allowed chat can disarm.
  Prefer private chats with each household member over a group with guests.
- Lock Telegram on your phone (Telegram → Settings → Privacy and Security →
  Passcode Lock), since `/disarm` needs no PIN.
- If the bot token leaks, revoke it in @BotFather (`/revoke`) and put the new
  one in `.env`.

## 7. Backups

Back up, regularly and somewhere other than the hub:

| What | Why |
|---|---|
| `.env` | every password and secret (store it encrypted, e.g. in a password manager) |
| `config/` | your homeguard, Frigate and Mosquitto configuration |
| `data/zigbee2mqtt/` | **the Zigbee network key and device database**: without it, every device must be paired again |
| `data/homeguard/homeguard.db` | the event log |
| `data/frigate/` | recordings, if you want them kept off the hub |

A safe copy of the event log while homeguard runs:

```sh
sudo apt install sqlite3
sqlite3 data/homeguard/homeguard.db ".backup 'backup-homeguard.db'"
```

And of the rest:

```sh
tar czf homeguard-backup-$(date +%F).tgz .env config data/zigbee2mqtt backup-homeguard.db
```

Copy the archive to another machine or an encrypted cloud drive, and try a
restore once, so you know it works.

## 8. Physical security

- **Hide the hub.** A burglar who finds it can unplug it. Put it in a
  cupboard or another room than the entrance, ideally out of sight of
  windows. Do the same for the router and internet box if you can.
- homeguard reports cameras and critical sensors going offline while armed,
  and the wisense bridge going silent, as possible tampering, but only as long
  as the hub itself and the internet are up.
- A Zigbee siren with a backup battery keeps sounding even if mains power is
  cut.
- Radio can be jammed (WiFi, Zigbee). Wired PoE cameras and a 4G backup make
  that much harder.

## 9. Power

A UPS on the hub, the router, the internet box and the PoE switch keeps
everything running through short power cuts and protects the SSD from sudden
power loss. A laptop hub already has a battery: keep it plugged in, and set
it not to suspend on lid close or on battery.

After a power cut, homeguard comes back armed in the mode it was in (the
arm state is saved), and `restart: unless-stopped` restarts every container
when Docker starts.

## 10. Checklist

- [ ] No `change-me` left in `.env`; `chmod 600 .env`
- [ ] Camera, router and Frigate admin passwords changed
- [ ] Firmware updated on cameras and router
- [ ] Cameras blocked from the internet or on a separate network
- [ ] No port forwarding to the hub or cameras; VPN for remote access
- [ ] Telegram allow-list reviewed; Telegram passcode set
- [ ] Backup made, including `data/zigbee2mqtt/`, and copied off the hub
- [ ] Hub hidden; UPS in place
