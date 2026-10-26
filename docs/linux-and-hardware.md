# Linux and hardware details

Part of the [wisense documentation](../README.md#learn-how-it-works).

## The WiFi stack

```text
  wisense ── /proc/net/wireless (legacy "wireless extensions") ──┐
  wisense ── nl80211 over generic netlink (modern API) ──────────┤
                                                                  v
                                cfg80211   kernel WiFi configuration layer
                                    |
                                mac80211   shared "soft MAC" used by most drivers
                                    |
                                rtw88_8822ce   driver for the author's Realtek RTL8822CE card
```

The author's card is a **Realtek RTL8822CE** on the in-kernel **rtw88** driver, which
is built on mac80211. Broadcom's proprietary `wl` driver doesn't use
mac80211, which is why it often exposes less information.

## `/proc/net/wireless`

```text
Inter-| sta-|   Quality        |   Discarded packets               | Missed | WE
 face | tus | link level noise |  nwid  crypt   frag  retry   misc | beacon | 22
wlp2s0: 0000   62.  -48.  -256        0      0      0      0     32        0
```

- Two header lines, then one line per *connected* wireless interface. A
  disconnected interface is simply missing.
- Columns: status (hex), link quality, **signal level**, noise level,
  discarded-packet counters, missed beacons.
- A trailing `.` means "updated" and is stripped before parsing.
- The signal is in dBm when negative. Some drivers report a positive
  0–100/0–255 scale instead; wisense warns and still uses it, since
  fluctuations don't care about units.
- Noise `-256` means "not reported".
- This file is provided by cfg80211's compatibility layer. On mac80211
  drivers the signal is that of the last frame from the router.

## `/proc/net/route` and byte order

```text
Iface   Destination  Gateway   Flags  RefCnt  Use  Metric  Mask      ...
wlp2s0  00000000     0101A8C0  0003   0       0    600     00000000
```

- A **default route** has destination and mask `00000000`.
- Flags `0003` means `RTF_UP | RTF_GATEWAY`.
- The kernel prints the address's raw 32-bit value as hex. On a
  **little-endian** CPU (x86, most ARM) the bytes come out reversed:
  `0101A8C0` → bytes `C0 A8 01 01` → **192.168.1.1**.
- The code parses the hex into a `u32` and takes its *native-endian* bytes,
  which gives the right answer on any CPU.
- It picks the gateway **of the WiFi interface**: pinging a gateway reached
  over Ethernet would create no WiFi frames at all.

## nl80211 and netlink

- **Netlink** is how user programs exchange messages with the kernel through
  a socket. **Generic netlink** hosts named "families"; WiFi's is
  `nl80211`.
- wisense sends `NL80211_CMD_GET_STATION` with the *dump* flag for the
  interface. In client mode, the only "station" is the router.
- Replies are lists of **attributes** in TLV form (type, length, value,
  padded to 4 bytes). Some attributes are **nested** lists themselves.
- `STA_INFO` contains `SIGNAL`, `SIGNAL_AVG`, `CHAIN_SIGNAL` (one value per
  antenna) and `RX_PACKETS`, among others.
- Reading station info needs no root.
- **A real bug found on the author's laptop**: the `wl-nl80211` crate (0.8) returns
  `CHAIN_SIGNAL` as the raw bytes of its nested attributes,
  `[5,0,0,0,-56,0,0,0, 5,0,1,0,-54,0,0,0]`. That's two attributes: length 5,
  type 0 and 1, carrying −56 and −54 dBm.
  `nl80211::decode_chain_signal` decodes these, and leaves plain values
  alone if a future crate version fixes it.

## ping, ICMP and root

- `ping` sends ICMP *echo request* packets; the router answers with *echo
  replies*, and each reply refreshes RSSI.
- wisense runs `ping -i <interval> -q -n <gateway>`:
  - `-i` sets the interval;
  - `-q` stays quiet;
  - `-n` uses no DNS.
- Older ping versions refuse intervals below 0.2 s without root. The iputils
  on the author's laptop (20240905) allows down to 2 ms.
- If your router ignores pings, use `--keepalive-target auto`, or
  `--keepalive-target` with another device on the network: traffic to it
  still passes through the access point.

## Processes and signals

- **Ctrl+C** makes the terminal send **SIGINT** to the whole *foreground
  process group*, so wisense *and* its `ping` child receive it. wisense
  treats a ping exit with code 0/1 (its normal reaction to SIGINT) as a
  clean stop, and any other exit as a warning.
- **SIGTERM** (from `kill`) and **SIGHUP** (terminal closed) also trigger a
  clean shutdown that kills `ping`.
- **SIGKILL** can't be caught; `ping` could then outlive wisense.
  `kill_on_drop` covers panics and early returns.
- **SIGTTOU**: a program in a *background* process group that tries to
  change terminal settings gets stopped by the kernel. We hit this while
  testing the TUI under `timeout`, which puts its child in the background:
  the process just froze in state `T`. `timeout --foreground` fixes it.

## Terminals

- **Is it a terminal?** `std::io::IsTerminal` decides whether to draw the
  live line, use colours, or allow the TUI.
- **ANSI escape codes** control the terminal: `\r` returns to the line start,
  `\x1b[2K` erases the line, `\x1b[33m` sets a colour. The live line redraws
  itself with `\r\x1b[2K`. Captured logs are stripped of colour codes for
  the TUI.
- **Raw mode** turns off line buffering, echo and signal keys, so Ctrl+C
  arrives as an ordinary key, which the TUI handles.
- The **alternate screen** (`\x1b[?1049h` and `\x1b[?1049l`) is a separate
  screen the TUI draws on, so your scrollback is untouched afterwards.
- The terminal must be **restored** however the program ends.
  `ratatui::try_init` installs a panic hook that restores it before the
  panic message is printed, and `TuiSink` restores it on exit and in `Drop`.

## Serial ports

- An ESP32 talks over a **UART**: bytes framed by a fixed **baud rate**,
  which both sides must agree on. 921600 baud is roughly 92 KB/s.
- USB-serial chips show up as `/dev/ttyUSB0` or `/dev/ttyACM0`. The
  `dialout` group may open them, and `/dev/serial/by-id/…` gives stable
  names.
- Data arrives in arbitrary chunks, not lines, so lines must be reassembled.
  Boards also print boot messages and log lines that must be skipped, and a
  wrong baud rate produces garbage.

## Scanning (`iw scan`)

An active scan makes the card hop across channels and listen. It needs
`CAP_NET_ADMIN` (root), takes seconds, and disturbs the connection, so it's
never used for sensing. `iw … scan dump` shows the kernel's cached results
without root.
