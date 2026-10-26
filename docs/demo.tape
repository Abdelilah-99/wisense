# Records docs/demo.gif: `wisense demo --tui` on synthetic data, no WiFi
# hardware needed. Generate it from the repository root with:
#
#   vhs docs/demo.tape
#
# VHS: https://github.com/charmbracelet/vhs (it also needs ttyd and ffmpeg).
#
# Timeline (seed 1, 45 s of synthetic data played at 2x, so ~22 s):
#   0-10 s   calibrating (the first 20 s of data)
#   ~14 s    MOTION started (simulated motion from +26.4 s of data)
#   ~19 s    motion ended
# Check it with: wisense demo --seconds 45 --speed 50 --json

Output docs/demo.gif

Require cargo

Set Shell "bash"
Set FontSize 16
Set Width 1200
Set Height 720
Set Padding 16
Set TypingSpeed 40ms

# Build first, off camera. The marker is computed so the regex can't match
# the typed command itself.
Hide
Type "cargo build --release --features tui && echo build-$((40 + 2))-done"
Enter
Wait@10m /build-42-done/
Type "clear"
Enter
Sleep 500ms
Show

Type "./target/release/wisense demo --tui --speed 2 --seconds 45"
Sleep 600ms
Enter
# Calibration, MOTION starting and ending; stop just before the demo exits.
Sleep 21s
