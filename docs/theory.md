# The ideas behind it

Part of the [wisense documentation](../README.md#learn-how-it-works).

Contents:

- [10. Radio basics](#10-radio-basics)
- [11. Why a moving body changes the signal](#11-why-a-moving-body-changes-the-signal)
- [12. RSSI in practice](#12-rssi-in-practice)
- [13. CSI: a much richer view](#13-csi-a-much-richer-view)
- [14. Sampling the signal](#14-sampling-the-signal)
- [15. The motion score](#15-the-motion-score)
- [16. Baseline, threshold and false alarms](#16-baseline-threshold-and-false-alarms)
- [17. Hysteresis and debouncing](#17-hysteresis-and-debouncing)
- [18. Zones with several sensors](#18-zones-with-several-sensors)
- [19. Measuring how well it works](#19-measuring-how-well-it-works)

## 10. Radio basics

### Frequency and wavelength

WiFi uses radio waves around **2.4 GHz** and **5 GHz** (and 6 GHz on newer
gear). A wave's **wavelength** is λ = c / f, the speed of light divided by
the frequency:

| Band | Frequency | Wavelength |
|---|---|---|
| 2.4 GHz | 2.4 × 10⁹ Hz | 12.5 cm |
| 5 GHz (the author's link: 5320 MHz, channel 64) | 5.32 × 10⁹ Hz | 5.6 cm |

Wavelength matters because it sets the scale of everything that follows.
Moving something by a fraction of a wavelength, a few centimetres, is
already enough to change how the signal adds up at the antenna.

### Decibels and dBm

Radio power covers a huge range, so it is measured on a logarithmic scale.

- **dB** (decibel) compares two powers: +3 dB ≈ twice the power, +10 dB = 10×
  the power, −10 dB = one tenth.
- **dBm** is power relative to 1 milliwatt: P(mW) = 10^(dBm / 10).

| dBm | Power | Typical meaning for WiFi |
|---|---|---|
| −30 | 1 µW | right next to the router |
| −50 | 10 nW | excellent |
| −60 | 1 nW | good |
| −70 | 100 pW | usable |
| −80 | 10 pW | weak |
| −90 | 1 pW | barely connected |

Received WiFi power is tiny, so the numbers are negative. The author's link
sits around −48 to −58 dBm. On a log scale a 1 dB change is about a 26% change
in power, and that is the kind of change wisense watches for.

### RSSI

**RSSI** (Received Signal Strength Indicator) is the card's estimate of how
strong a received frame was, reported in dBm. It is **one number per
received frame**, rounded to whole dBm. It's crude, but every WiFi card has
it, which is why wisense starts there.

## 11. Why a moving body changes the signal

### Multipath and fading

The signal from the router doesn't take one path to your laptop. It takes
many at once:

- the direct line of sight (LOS);
- reflections off walls, the floor, furniture, and people.

```text
            wall
   ┌──────────────────────┐
   │   ╲  reflected     ╱  │
   │    ╲              ╱   │
 Router ───── direct ───── Laptop
   │        ╲      ╱       │
   │         person        │
   └──────────────────────┘
```

At the antenna these copies **add up as waves**. Where their peaks line up,
they reinforce; where peaks meet troughs, they cancel. The result depends on
the exact path lengths, compared to the 5.6 cm wavelength. If any path
changes by a couple of centimetres, the sum can swing by several dB. This
is called **fading**.

A human body is mostly water. It absorbs, reflects and blocks 5 GHz waves.
When it moves, it changes path lengths and blocks or unblocks paths, so the
received signal **fluctuates**.

### The Fresnel zone: why the body must cross the path

Most of the energy between two antennas travels inside an elongated
ellipsoid around the straight line between them, called the **first Fresnel
zone**. Its radius at a point that is d1 from one end and d2 from the other
is:

```text
r = √( λ · d1 · d2 / (d1 + d2) )
```

Example: laptop and router 5 m apart on a 5.3 GHz link like the author's
(λ ≈ 0.056 m):

- at the middle (d1 = d2 = 2.5 m): r = √(0.056 × 2.5 × 2.5 / 5) ≈ **0.26 m**,
  so the ellipsoid is about half a metre thick;
- 0.5 m from the laptop (d1 = 0.5, d2 = 4.5): r ≈ **0.16 m**.

A person (40–50 cm wide) crossing near the middle blocks a large part of it,
and the signal changes a lot. A person walking two metres to the side barely
touches it, so the change is much smaller. This is the physics behind the
placement advice: **people must walk between the laptop and the router.**

### Doppler: how fast the signal changes

When a reflector moves, the reflected path changes length continuously and
the fading pattern sweeps past the antenna. The rate is about f = v / λ:
walking at 1 m/s on 5.3 GHz gives about **18 Hz**, and up to about twice
that for reflections whose path length changes at twice the walking speed.
RSSI sampled 20 times a second can't follow such fast changes exactly, and
it doesn't need to: wisense only asks *how much* the signal is fluctuating,
not what the waveform looks like.

### Moving versus standing still

- A **moving** person keeps changing the paths, so the signal keeps
  fluctuating. This is easy to detect.
- A **still** person changes the paths once, then the new sum stays the same.
  The level may shift by a few dB, but it stops fluctuating, and a small
  level shift is hard to tell from normal drift (sun on a wall, a door left
  open, the router adjusting its power). So wisense treats a still person
  like an empty room.

wisense looks at **short-term fluctuation** (variance), not at the signal
level (mean). Detecting a still person needs much finer measurements, such
as tracking breathing with CSI phase, which is beyond this project.

## 12. RSSI in practice

Real RSSI has four quirks the code has to deal with:

1. **Quantized.** Drivers report whole dBm, so values jump in 1 dB steps.
   Small fluctuations either vanish or appear as a full 1 dB step.
2. **Noisy even when nothing moves.** Frames are sent at different rates and
   power levels, beacons differ from data frames, the card has two antennas,
   and its automatic gain control adjusts all the time. On the author's
   laptop an empty room still shows about 1.3–1.6 dB of spread between readings.
3. **Only updated when a frame arrives.** The kernel keeps the signal of the
   *last frame received from the router*. If nothing arrives, the value
   freezes and looks perfectly still, even with someone dancing in front of
   it. The router sends beacons about 10 times a second (every 102.4 ms),
   and data frames when traffic flows.
4. **Keepalive traffic keeps it fresh.** wisense runs `ping` against the
   router every 0.2 s. Each reply is a frame from the router, so RSSI gets
   refreshed regularly, whatever else the computer is doing.

Measured on the author's laptop over 40 s each, with normal traffic in the
background:

| Keepalive | Frames/s from the router | RSSI changes/s (read at 100 Hz) | Changes/s seen at 20 Hz | Longest time unchanged |
|---|---|---|---|---|
| off | 21 | 7.6 | 6.3 | 0.6 s |
| ping every 0.2 s | 24 | 8.8 | 7.0 | 0.7 s |
| ping every 0.05 s | 43 | 14.2 | 9.1 | 1.0 s |

Notice that RSSI changes less often than frames arrive: consecutive frames
often round to the same whole dBm. Notice also that 20 readings per second
miss some changes when frames come faster ([§14](#14-sampling-the-signal)).

## 13. CSI: a much richer view

### OFDM and subcarriers

Modern WiFi uses **OFDM**: a 20 MHz channel is split into **64 narrow
subcarriers** 312.5 kHz apart, each carrying part of the data. A few at the
edges (guard subcarriers) and one at the centre (DC) carry nothing. Every
subcarrier sees slightly different fading, because the path-length
differences translate into different phase shifts at slightly different
frequencies.

### What CSI is

**Channel State Information** describes, for each subcarrier, how the channel
changed the known training symbols at the start of a frame. For each
subcarrier it's a **complex number** `re + j·im`:

- its **amplitude** `√(re² + im²)` says how strongly that subcarrier got
  through;
- its **phase** `atan2(im, re)` says how much it was delayed.

So where RSSI gives one number per frame, CSI gives 64 amplitudes (and
phases), a "fingerprint" of the channel across frequency.

### How the ESP32 reports it

ESP32-CSI-Tool prints one line per frame:

```text
CSI_DATA,STA,AA:BB:CC:DD:EE:FF,-57,11,1,7,0,1,1,1,0,0,0,-93,0,6,0,3517632,0,48,0,0,3517.632,128,[3 4 0 0 -6 8 ...]
```

That's 25 comma-separated fields (field 4, index 3, is the RSSI), then the
raw CSI buffer in brackets. The buffer holds signed bytes in **(imaginary,
real) pairs**. wisense checked this against the firmware source
(`_components/csi_component.h`) and its `python_utils/parse_csi.py`. So
`[3 4 0 0 -6 8]` means amplitudes 5, 0 and 10. Guard and null subcarriers
come out as zeros.

### Why amplitudes only

Cheap radios don't share a clock with the router. Every frame's phase is
shifted by random offsets (carrier frequency offset, sampling offset, packet
detection delay). Cleaning that up is a research topic of its own, so
wisense uses amplitudes, which don't have this problem.

### Why CSI beats RSSI

A body moving through the room changes some subcarriers a lot and others
hardly at all. With 50+ independent views you're much more likely to catch
the change than with one rounded number. That's why wisense's CSI score
looks at the **most variant quarter** of the subcarriers
([§15](#the-csi-score)).

## 14. Sampling the signal

wisense reads RSSI from `/proc/net/wireless` at a fixed **poll rate**
(20 Hz by default).

- **Zero-order hold.** Between frames, the value just stays the same. Polling
  turns that step-shaped signal into an evenly spaced series of readings.
- **Emit every poll.** A sample is produced on every poll, even if the value
  didn't change. `/proc/net/wireless` has no per-frame counter, so "same
  value" can't be told apart from "no new frame". Throwing away repeats
  would discard real "nothing changed" readings and inflate the scores.
  Even spacing also keeps calibration and monitoring statistically
  identical, provided the poll rate stays the same. That's why the baseline
  file records `poll_hz`.
- **Staleness is tracked separately.** If the value hasn't changed for 3 s,
  wisense warns ([§9](../README.md#9-troubleshooting)).
- **Missed changes.** When frames arrive faster than the poll rate, some
  changes happen and revert between two polls and are never seen (the table
  in [§12](#12-rssi-in-practice): 14.2 → 9.1 changes/s). That makes the
  score a little less sensitive, but consistently so, because calibration
  sees the same thing.

ESP32 boards are not polled. Every received frame produces a sample.

## 15. The motion score

### Sliding window

For each link, wisense keeps the samples of the last **2 seconds** in a
queue (a `VecDeque`). New samples go in at the back and samples older than
the window fall out at the front. After every new sample, it computes a
**score** from what's in the window. At 20 Hz a 2 s window holds 41 readings,
so 40 steps between them.

### Mean absolute first difference (`--metric diff`, the default)

```text
score = ( |x₁ − x₀| + |x₂ − x₁| + … + |xₙ − xₙ₋₁| ) / n
```

It's the average size of a step between consecutive readings. Example with
six readings:

```text
readings: -54  -54  -56  -55  -55  -58
steps:        0    2    1    0    3      → (0+2+1+0+3) / 5 = 1.2 dB
```

### Standard deviation (`--metric std`)

```text
mean  = (x₀ + … + xₙ) / (n+1)
score = √( average of (xᵢ − mean)² )
```

For the same six readings: mean −55.33, std ≈ **1.37 dB**.

### Why diff is the default

Compare them on a **slow drift**: the level slides from −50 to −52 dBm over
the 2 s window with no noise at all.

- std ≈ **0.58 dB** (a straight ramp of height h has a std of h / √12);
- diff = 2 dB total / 40 steps = **0.05 dB**.

Slow level changes happen all the time and mean nothing: a door opens across
the house, the router changes its transmit power, someone sits down and
stays still. std reacts to them; diff hardly does. Diff reacts to *fast*
changes, which is what motion produces. Rounding to whole dB also hurts std
more: a single 1 dB step halfway through the window gives std 0.5 dB, but
diff only 1/40 = 0.025 dB.

On an example recording, diff separated the busy phase from the quiet phase
and std couldn't ([§19](#19-measuring-how-well-it-works)).

### Relation to noise

If readings were independent noise with standard deviation σ, the average
step would be E|xᵢ − xᵢ₋₁| = 2σ/√π ≈ **1.13 σ**. Real readings repeat between
frames, so many steps are zero and the diff score is lower. On the author's
laptop, 1.13 × 1.5 dB ≈ 1.7, times the ~40% of polls that see a new value,
gives ≈ 0.7 dB. That matches the measured idle scores of 0.55–0.9 dB, and it shows
why the poll rate and ping interval must be the same for calibration and
monitoring.

### The CSI score

For CSI links, for each subcarrier, wisense computes the **variance** of its
amplitude over the window. It skips subcarriers that are zero throughout
(guard and null ones), then averages the **top 25%** of the variances. Motion
usually affects some subcarriers much more than others, and averaging all of
them would dilute it. The unit is amplitude² and has no physical meaning on
its own; only the comparison with the baseline matters. Only CSI rows of the
same length as the newest one are compared, because the subcarrier count can
change with the frame type.

### Warm-up

A score from a handful of samples is unreliable. A window yields a score
only once its oldest sample is at least **75% of the window length** old
(1.5 s for a 2 s window). Until then, `Status` reports `score: null`. The
same rule applies after a gap in the data.

## 16. Baseline, threshold and false alarms

### Calibration

wisense can't know in advance what "quiet" looks like in your room: every
room, laptop position and router differs. So it measures it. During
calibration, every score is collected, and at the end two numbers summarize
them:

- **mean**: the typical idle score (in the example calibration from the
  author's laptop: 0.570 dB);
- **std**: how much the idle score itself wanders (in the same example:
  0.155 dB).

They're computed in one pass with **Welford's algorithm**, which is
numerically stable:

```text
for each score x:
    n    ← n + 1
    δ    ← x − mean
    mean ← mean + δ / n
    M2   ← M2 + δ · (x − mean)
variance = M2 / n,   std = √variance
```

### The threshold

```text
threshold = mean + k × std        (k = --sensitivity, default 4)
```

This is a **z-score** rule: "fire when the score is more than k standard
deviations above normal". The example baseline gives 0.570 + 4 × 0.155 =
**1.19 dB**.

### False alarms versus missed motion

Every threshold trades two kinds of mistake:

- **too low** gives false alarms: normal noise crosses it now and then;
- **too high** gives missed motion: gentle or distant movement never
  reaches it.

If idle scores were independent and bell-shaped (Gaussian), exceeding
mean + 4σ would happen about 3 times in 100,000. Real scores are worse:

- consecutive scores come from overlapping windows, so they're **strongly
  correlated**, and a spike lasts a whole window;
- the distribution has a **heavier upper tail** than a bell curve.

On synthetic noise ([`src/source/synthetic.rs`](../src/source/synthetic.rs)),
k = 4 with the diff metric gave about **1 false start per 100 minutes** after
a 60 s calibration, and about 1 per 33 minutes after a 20 s one. k = 5 gave
none. The `--on-ms` wait ([§17](#17-hysteresis-and-debouncing)) removes many
short spikes.

### Why calibrate for 60 seconds

Scores from overlapping 2 s windows aren't independent. 20 s of calibration
contains only about **10 independent windows**, so the estimated mean and
std can be off by chance. 60 s gives about 30, which is much more reliable.
That's why `calibrate` defaults to 60 s, and why the 20 s auto-calibration
warns you loudly.

### The safety floors

The threshold is also kept above two floors:

```text
threshold = max( mean + k × std,   2 × mean,   0.05 )
```

- **2 × mean.** MOTION ends when the score drops below **0.7 × threshold**
  ([§17](#17-hysteresis-and-debouncing)). If the idle score barely varies
  (tiny std), mean + 4·std could be just 1.3 × mean. 0.7 × 1.3 = 0.91 × mean
  would put the "end" level *below* normal idle scores, and motion would
  never end. A test with CSI data caught exactly this. With the floor, the
  end level is at least 1.4 × mean.
- **0.05.** A perfectly flat calibration (mean = std = 0) would otherwise
  give threshold 0, and the first tiny change would fire.

The consequence for tuning: with the example baseline, mean + k·std drops below
2 × mean (1.14) when k is below about 3.7. Lowering `--sensitivity` further
has no effect.

### Auto-calibration

Without a usable saved baseline, a link learns one from its first 20 s and
prints a loud warning to keep still. It needs at least 10 scores to finish.

## 17. Hysteresis and debouncing

A score hovering around the threshold would switch MOTION on and off many
times a second. Two classic tricks prevent that.

**Hysteresis**, the same idea as a thermostat or a Schmitt trigger, uses two
levels instead of one:

```text
                 score > threshold          for on-ms  (600 ms)
        IDLE  ─────────────────────────────────────────────▶  MOTION
          ▲                                                      │
          └──────────────────────────────────────────────────────┘
                 score < 0.7 × threshold    for off-ms (3000 ms)
```

Between 0.7 × threshold and the threshold there's a **dead band**: nothing
changes there, whichever state you're in. With the example baseline, MOTION starts
above **1.19** and ends below **0.83**.

**Debouncing** adds the timers: a condition must hold *continuously* for
`on-ms` or `off-ms`. One dip resets the clock.

A typical walk across the path:

```text
t = 0.0 s   you start walking between laptop and router
t ≈ 0.3 s   enough big steps in the 2 s window → score passes the threshold
t ≈ 0.9 s   still above after 600 ms          → "MOTION started"
t = 5.0 s   you stop
t ≈ 7.0 s   your movement has left the window → score drops below 0.83
t ≈ 10.0 s  still below after 3 s             → "motion ended after 6.7 s"
```

The reported duration runs from the first crossing (0.3 s) to the drop below
the end level (7.0 s). It leaves out the two timer waits, but it does include
the time your movement takes to leave the window.

## 18. Zones with several sensors

With two or more links (the laptop plus ESP32 boards):

1. **Normalize** each link: `score / threshold`. A ratio of 1.0 means "at its
   own threshold", which makes links with different units and noise levels
   comparable.
2. While **any** link is in MOTION, the zone is the link with the highest
   ratio.
3. **Hold time**: a different link must stay on top for **1 s** before the
   zone switches, so two similar links don't ping-pong. Entering and leaving
   "no zone" follows MOTION immediately, because MOTION is already debounced.
4. **Confidence** is the top ratio minus the runner-up's: how clearly one link
   wins.

This answers "which sensor path is most disturbed?", which roughly follows
where someone is moving. It's not positioning: a person standing between
two paths, or a large room with one board, will give misleading zones.

## 19. Measuring how well it works

### Ground truth

To judge a detector you need to know **what really happened when**: note the
times you walked, sat still or left. Then compare with the events. Without
ground truth, every result is a guess.

### Separation

A useful single number is the separation between two situations A and B
(say "walking" and "still"):

```text
d' = (mean score in A − mean score in B) / pooled standard deviation
```

Roughly: d' below 1 means the score distributions overlap heavily; around 2
is usable; above 3 separates cleanly.

### Worked example: a recording from the author's laptop

This 120 s recording (`walk.csv`) has three clearly different phases. The
diff score with a 2 s window, against the example calibration's idle score
of 0.57:

| Phase | Mean score | Detector with the example baseline (threshold 1.19) |
|---|---|---|
| 0–18 s | 1.94 dB | MOTION from +3.4 s |
| 19–86 s | 0.93 dB | stayed in MOTION (hovered near the threshold) |
| 87–120 s | 0.73 dB | idle |

How well each setting separates the middle phase from the quiet end:

| Window | d' with `diff` | d' with `std` |
|---|---|---|
| 1 s | 0.49 | 0.23 |
| 2 s | 0.74 | 0.19 |
| 4 s | 1.25 | 0.10 |
| 6 s | 1.60 | −0.03 |

Lessons:

- `std` can't tell these phases apart at all on the author's link; `diff` can.
- **Longer windows separate better.** Averaging over more steps makes the
  score steadier. The price: the score reacts later (the window must fill
  with motion) and MOTION lasts longer after motion stops (it takes a full
  window to leave).
- Which phase was the walking decides what to tune, which is why ground
  truth matters.
