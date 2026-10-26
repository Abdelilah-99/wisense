# Glossary

Part of the [wisense documentation](../README.md#learn-how-it-works).

| Term | Meaning |
|---|---|
| **AP / router** | Access point: the device your laptop connects to over WiFi. |
| **STA** | Station: a WiFi client (your laptop, an ESP32 in `active_sta` mode). |
| **SSID / BSSID** | Network name / the access point's hardware address. |
| **dB, dBm** | Logarithmic power ratio / power relative to 1 mW ([§10](theory.md#10-radio-basics)). |
| **RSSI** | Received signal strength of a frame, in dBm. |
| **Frame** | One WiFi transmission unit (data, beacon, acknowledgement, …). |
| **Beacon** | Frame the router broadcasts about 10×/s to announce the network. |
| **TU** | Time unit = 1.024 ms (beacon interval 100 TU = 102.4 ms). |
| **DTIM / power save** | Mechanism letting clients sleep between beacons to save power. |
| **OFDM / subcarrier** | Splitting a channel into many narrow carriers ([§13](theory.md#13-csi-a-much-richer-view)). |
| **CSI** | Channel State Information: per-subcarrier amplitude and phase. |
| **LLTF / LTF** | Known training symbols at the start of a frame, from which CSI is computed. |
| **LOS / NLOS** | Line of sight / no line of sight between the antennas. |
| **Multipath** | The signal arriving over several paths at once. |
| **Fading** | Strength changes caused by those paths adding and cancelling. |
| **Fresnel zone** | Ellipsoid around the direct path that carries most of the energy. |
| **Doppler** | Frequency shift / fluctuation rate caused by moving reflectors. |
| **VHT / MCS** | 802.11ac "Very High Throughput" / modulation and coding scheme (link speed). |
| **ICMP echo** | What `ping` sends and what the router answers. |
| **Keepalive** | Steady traffic that keeps RSSI fresh. |
| **Poll rate** | How often wisense reads the RSSI. |
| **Zero-order hold** | A sampled value staying constant until the next update. |
| **Quantization** | Rounding to fixed steps (whole dBm). |
| **Window** | The last N seconds of samples a score is computed from. |
| **Score** | Number summarizing how much the signal fluctuates in the window. |
| **Baseline** | Mean and std of the score in an empty, still room. |
| **z-score / k** | "How many standard deviations above normal"; k = `--sensitivity`. |
| **False positive / negative** | False alarm / missed motion. |
| **Hysteresis** | Different levels for switching on and off ([§17](theory.md#17-hysteresis-and-debouncing)). |
| **Debounce** | Requiring a condition to hold for some time before acting. |
| **d′** | Separation between two score distributions ([§19](theory.md#19-measuring-how-well-it-works)). |
| **Netlink / nl80211** | Kernel–user messaging / its WiFi family ([Linux and hardware](linux-and-hardware.md#nl80211-and-netlink)). |
| **cfg80211 / mac80211** | Kernel WiFi configuration layer / shared driver framework. |
| **Process group** | Set of processes that receive terminal signals together. |
| **Raw mode / alternate screen** | Terminal modes used by the TUI ([Linux and hardware](linux-and-hardware.md#terminals)). |
| **UART / baud** | Serial link / its bit rate. |
| **Backpressure** | A full channel making producers wait, bounding memory. |
