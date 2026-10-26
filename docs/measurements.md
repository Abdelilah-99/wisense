# Measured on the author's laptop

Part of the [wisense documentation](../README.md#learn-how-it-works).

| Fact | Value |
|---|---|
| Card / driver | Realtek RTL8822CE, `rtw88_8822ce` (mac80211) |
| Interface / link | `wlp2s0`, 5 GHz channel 64 (5320 MHz), 80 MHz, 802.11ac (VHT) |
| Router | gateway 192.168.1.1, beacon interval 100 TU (≈ 9.8 beacons/s), power save on |
| Signal | about −46 to −58 dBm |
| Frames from the router | 21/s (no keepalive), 24/s (ping 0.2 s), 43/s (ping 0.05 s) |
| RSSI value changes | 7.6 / 8.8 / 14.2 per second (read at 100 Hz) |
| Idle RSSI spread | about 1.3–1.6 dB standard deviation |
| Example calibration (60 s) | idle score 0.570 ± 0.155 dB → threshold 1.19 dB, end level 0.83 dB |
| nl80211 | works without root; two antenna chains |
| ping | iputils 20240905, non-root minimum interval 2 ms |
