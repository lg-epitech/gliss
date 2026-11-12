# A viewer on a WAN

The [Distribox path notes](distribox-path.md) measure on one host, with nothing
between the server and the browser. This page puts a link in between: 100 ms of
round trip, about what Europe to eastern North America takes, and less
bandwidth than the stream wants. It records what 0.1.0-beta.1 did there, what Gliss does now, and
what a benchmark against a real QEMU guest measured.

## The link

`scripts/bench-distribox-path.mjs --link` moves the run into a user and network
namespace of its own and the browser into a second one. A veth pair joins the
two. On each end the kernel holds every packet for half the round trip, then
lets them out at the direction's rate and drops what a 200 ms buffer cannot
hold. The kernel's own TCP runs across it, so slow start, the restart after a
pause, loss recovery and the queue at the bottleneck are real. A TCP relay
stands where a reverse proxy or Distribox's backend does: it ends the browser's
connection and opens a local one to the server, so the server never feels the
link through its own socket.

The scenes are those of the Distribox path notes, which need about 0.7 Mbit/s
(scroll), 1.2 Mbit/s (drag) and 1.5 Mbit/s (video), and one more: in `pages`,
each key repaints the whole terminal with new text, as a window switch or a
page load does. That is one frame of 65 to 85 kB about once a second.

## What 0.1.0-beta.1 did

- **On a link slower than the stream, frames queued.** The server sent each
  frame as soon as the window of eight had room. At 1 Mbit/s the video scene's
  frames were 387 ms old when painted and a dragged window trailed the pointer
  by 804 ms, at 20 and 11 frames a second. At 0.5 Mbit/s those were 723 ms and
  2.3 seconds.
- **A full-page repaint took 371 ms on an unlimited link**, where the round
  trip and the pipeline explain about 170 ms. Linux shrinks a connection's
  sending window after a pause, so a frame that follows a second of stillness
  needs three round trips. [TCP after a pause](#tcp-after-a-pause) has the
  remedy, which is not in Gliss.
- **Everything else was the same host's figure plus the round trip.** Typing
  echo was 131 ms against 32 ms, scroll and video stayed at 30 frames a second.

## What Gliss does now

1. **It measures the link from the acknowledgements.** The browser
   acknowledges each frame after painting it and says how long it held the
   frame for the decode and the paint, so the server knows when each frame
   arrived. When frames arrive later than their size explains and were written
   close behind one another, they queued, and the bytes that arrive per second
   are what the link carries. Two measurements have to agree before a lower
   rate counts: one late frame is more often a pause on the way.
2. **It keeps frames from queueing.** The server counts how long the frames it
   has written will occupy the link and encodes the next one only when it would
   wait there for less than 30 ms. Frames wait at the source instead, where a
   newer capture replaces them.
3. **The encoder aims at nine tenths of the measured rate**, between an eighth
   of `--h264-bitrate` and all of it, so frames that fit the link are not held
   back. It takes a new target when that differs from its own by more than a
   tenth, and returns to `--h264-bitrate` ten seconds after the link last was
   behind: a desktop that changes only now and then never queues, so nothing
   would show that the link became faster.
4. **The rate follows the link.** Later measurements move it. When
   acknowledgements come on time while it is holding frames back, it grows by
   5% each, and by 15% once no frame has been late for a second.

A link that keeps up yields no rate, and then none of this applies: the stream
is the one 0.1.0-beta.1 sent. A browser that is slow to decode or paint is not
a slow link, because it reports that time itself. What does read as a slow link
is any delay on the way that grows with the bytes in flight, such as a relay
that cannot keep up. There is no flag. `--stream-stats` reports the measured
rate as `link_bitrate`.

## Measurements

One real QEMU guest per link, 1920×1080, Distribox's flags, headless Chromium
with software rendering. Each cell is 0.1.0-beta.1, then this change: the
median of three rounds, the two builds alternating. Every link has 100 ms of
round trip.

| Measure | Unlimited | 2 Mbit/s | 1 Mbit/s | 0.5 Mbit/s | 4 ↔ 0.8 Mbit/s every 3 s | 1 Mbit/s, 1% loss |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Video: frame age at paint, p50 | 90 → 88 ms | 113 → 117 | 387 → 118 | 723 → 267 | 115 → 110 | 443 → 137 |
| Video: frame age, p95 | 105 → 102 ms | 129 → 132 | 649 → 128 | 969 → 471 | 587 → 175 | 545 → 262 |
| Video: frames per second | 30.0 → 30.0 | 30.0 → 30.0 | 19.7 → 30.0 | 10.6 → 7.7 | 24.4 → 28.2 | 18.7 → 25.6 |
| Video: longest gap between frames | 64 → 64 ms | 64 → 68 | 464 → 62 | 369 → 420 | 366 → 91 | 283 → 187 |
| Video: bitrate | 1.53 → 1.54 Mbit/s | 1.54 → 1.51 | 0.92 → 0.80 | 0.48 → 0.44 | 1.28 → 0.78 | 0.92 → 0.74 |
| Drag: pointer to paint, p50 | 140 → 153 ms | 187 → 171 | 804 → 363 | 2304 → 670 | 288 → 254 | 738 → 354 |
| Drag: pointer to paint, p95 | 187 → 174 ms | 404 → 354 | 1122 → 606 | 2720 → 1022 | 737 → 689 | 1056 → 521 |
| Drag: frames per second | 29.5 → 29.5 | 27.8 → 26.3 | 11.3 → 14.4 | 4.1 → 4.8 | 22.5 → 14.4 | 11.9 → 14.5 |
| Scroll: frame age, p50 | 87 → 86 ms | 95 → 98 | 115 → 109 | 519 → 218 | 96 → 97 | 127 → 128 |
| Scroll: frame age, p95 | 97 → 100 ms | 113 → 119 | 148 → 143 | 796 → 361 | 239 → 164 | 277 → 229 |
| Scroll: frames per second | 30.0 → 30.0 | 30.0 → 30.0 | 30.0 → 29.9 | 13.6 → 12.1 | 29.3 → 29.5 | 28.9 → 28.6 |
| Typing echo, p50 | 131 → 133 ms | 143 → 144 | 154 → 153 | 172 → 173 | 147 → 147 | 153 → 151 |
| Page turn, p50 | 371 → 373 ms | 497 → 489 | 761 → 780 | 1479 → 1524 | 524 → 522 | 803 → 773 |

With no link at all, the two builds measured the same: typing echo 32 and
32 ms, video frames 39 and 37 ms old at 30.0 frames a second, drag 38 and
39 ms.

The drag median on the unlimited link sits on one side or the other of a
16.7 ms paint step, about 139 or 153 ms, from run to run with either build: a
second run of the same link, for the next section, had them the other way
round. At 0.5 Mbit/s, 0.1.0-beta.1 missed pointer positions in every round: a
dragged window that is two seconds behind does not show every position.

A hill-climb session scored the pacing and the encoder's target on the
1 Mbit/s, alternating and unlimited links, as the geometric mean of this
build's frame ages, drag and page-turn latencies and frame rates over those of
0.1.0-beta.1 in the same run.
Three samples each: 0.96, 1.05 and 1.06 for 0.1.0-beta.1 against itself; 0.84,
0.87 and 0.93 with pacing; 0.79, 0.75 and 0.73 with the encoder following the
rate as well. A first version of the pacing scored 1.05, 1.03 and 1.00 and was
discarded: it took each frame's own time on the link off its delay, which made
every frame look late once the link became faster than the measured rate, and
the rate ratcheted down. A review after the session added the time the browser
reports it held each frame, and the table above is from the code as it is. The
[raw record](../benchmarks/wan-path-2026-11-12.json.gz) holds every metric of
these runs, the session log and the evaluator.

## TCP after a pause

On the unlimited link a page turn took 372 ms with either build. With
`net.ipv4.tcp_slow_start_after_idle=0` in the relay's network namespace it took
172 ms (p95: 185 ms against 383 ms), and nothing else moved. Linux has the
setting on by default: after a pause of about one retransmission timeout it
cuts the sending window back towards ten packets, 14 kB, and a larger frame
then waits for acknowledgements twice or more before all of it is out.

Gliss cannot set this. It applies to the socket that ends the browser's
connection, which is the reverse proxy's or the relay's. On that host, or in
that container's network namespace:

```sh
sysctl -w net.ipv4.tcp_slow_start_after_idle=0
```

It helps once the connection has carried a large frame. The first frame of a
session still starts from ten packets and took 0.6 s on the unlimited link.

## Limits

- **Frames are not made coarser than before.** The encoder's screen mode keeps
  its quantiser between 26 and 35. Drag frames are already at 35 on a slow
  link, so they stay 8 to 16 kB and the frame rate stays low: 14.4 frames a
  second at 1 Mbit/s. A page turn is one frame of 65 to 85 kB and takes as long
  as the link needs for it.
- **Fewer frames in some cases.** Frames that would have queued are not sent.
  Drag on the alternating link shows 14.4 frames a second where it showed
  22.5; at 0.5 Mbit/s video shows 7.7 where it showed 10.6 and scroll 12.1
  where it showed 13.6.
- **The rate takes about a second to follow a link that became faster**, while
  frames are being held back. Until then frames wait that the link could carry.
- **A wrong rate cannot stop the stream.** Whatever the rate says, the link is
  never taken to be busy for more than two seconds.
- **A resize sends its keyframe at `--h264-bitrate`.** The encoder is rebuilt
  for the new size and takes a lower target only from its second frame on.
- **Loss looks like a slower link.** A retransmission delays the frames behind
  it, and the rate drops for a moment.
- **A still picture keeps the quality of its last frame.** The encoder sharpens
  what it coded coarsely only while new frames arrive. That is unchanged, and
  a lower target on a slow link makes it more visible.
- One host and one guest per link, a synthetic desktop, headless Chromium with
  software rendering, and a kernel's model of a link: fixed delay, a rate and a
  drop-tail buffer. The relay is a plain TCP relay, not Distribox's backend.

## Reproduce

```sh
cargo build --locked --release -p gliss-server -j 2
TMPDIR=/dev/shm node scripts/bench-distribox-path.mjs \
  --binary /path/to/older/gliss-server --label before \
  --binary "$PWD/target/release/gliss-server" --label after \
  --scenarios typing,scroll,drag,video,pages --rounds 3 \
  --link rtt=100,down=1 --output /dev/shm/wan.json
```

The [development guide](development.md#distribox-path-benchmark) lists the
prerequisites and the link settings.
