# The Distribox path

[Distribox](https://github.com/PoCInnovation/Distribox) starts one server per
viewer and streams H.264 over WebSocket through its own relay:

```sh
gliss-server --vnc 127.0.0.1:PORT --bind 127.0.0.1:P --max-clients 1 \
  --codec h264 --h264-bitrate 2000000 --fps 30 --in-flight-frames 8
```

The display comes from QEMU's built-in VNC server, on a VM with a virtio GPU, a
PS/2 keyboard, a virtio tablet and an audio backend on the VNC display. This
page records how that server behaves, what Gliss does about it, and what a
benchmark against a real QEMU guest measured.

## How QEMU's VNC server behaves

These come from `ui/vnc.c` in QEMU 10 and 11, and from the traces below.

- **It looks for changes on a timer.** The timer runs every 30 ms at best. A
  tick sends an update only if the guest changed pixels, the client has an
  update request pending and the previous update has left. Every tick that
  sends nothing adds 50 ms to the timer, up to three seconds.
- **Only some client messages reset the timer.** Key, pointer, clipboard and
  QEMU's own extension messages set it back to 30 ms. Update requests do not.
- **A key press resets it too early.** The tick it triggers runs about half a
  millisecond after the key arrives, before the guest has drawn its reaction.
  That tick sends nothing, so the next one is 80 ms away.
- **It stores BGRX.** Asked for any other pixel format, it converts one pixel
  at a time. A probe measured 8.4 ns of QEMU CPU per pixel for RGBX against
  2.6 ns for BGRX: about 17 ms against 5 ms for a full 1920×1080 update.

## What Gliss does

1. **Takes the server's pixel format.** A server that advertises 32-bit
   little-endian BGRX is asked for BGRX, and Gliss swaps the bytes itself.
   Any other server is asked for RGBX as before. Full-width Raw rectangles are
   read in pieces of up to 256 KiB instead of one read per row.
2. **Requests the next update when the previous one arrives**, at most once
   per 25 ms at this 30 fps cap, so a request is pending at each 30 ms tick.
   The former fixed 33.3 ms clock drifted against QEMU's timer and missed a
   tick several times a second; each miss cost 110 ms instead of 30 ms.
   `--rfb-request-policy tick` restores the fixed clock.
3. **Holds QEMU's timer at 30 ms while the desktop is in use.** For one second
   after input or a display update, Gliss repeats QEMU's audio format message
   every 25 ms. It is the one client message that resets the timer and changes
   nothing else. It needs the audio extension, so a VNC display without an
   audio backend keeps QEMU's own timing. A screen that changes at least once a
   second stays held for as long as it is viewed, which cost QEMU 0.3% of one
   core on an idle display.
4. **Drops a frame instead of delaying every frame at the cap.** QEMU delivers
   33 updates a second at best, against a 30 fps cap. Encodes start on a
   schedule with one period of slack: a frame within the slack starts at once,
   and one frame in ten is dropped. The last frame of a burst is always
   encoded, one period late at most.
5. **Sends a keyframe every 3600 frames instead of every 240.** The WebSocket
   stream loses nothing and the client asks for a keyframe after a decode
   error, so a periodic keyframe only costs a burst of bytes and resets the
   picture to a coarser quality.

## Measurements

`scripts/bench-distribox-path.mjs` boots a diskless guest on QEMU/KVM with the
devices above. The guest draws a small desktop at 1920×1080 through KMS, the
way a compositor would, with marker strips that say which key press, pointer
position and animation frame the picture shows. A page in headless Chromium
runs the server's own browser client and reads those markers from the Canvas
after each paint. Latency runs from the client sending an input to the first
painted frame whose marker shows the guest reacted. Each server runs with the
flags above; the relay is not in the path.

| Measure | alpha.8 | Changes 1 to 4 | Changes 1 to 5 |
| --- | ---: | ---: | ---: |
| Typing echo, isolated keys, p50 / p95 | 89 / 102 ms | 42 / 55 ms | 40 / 55 ms |
| Typing echo, 5 to 9 keys per second, p50 / p95 | 90 / 103 ms | 39 / 52 ms | 38 / 49 ms |
| Key pressed during scroll, p50 | 65 ms | 51 ms | 57 ms |
| Window drag, pointer to paint, p50 / p95 | 55 / 89 ms | 55 / 73 ms | 55 / 75 ms |
| Scroll: distinct frames per second | 20.6 | 30.0 | 30.0 |
| Scroll: gap between frames, p95 | 112 ms | 55 ms | 55 ms |
| Scroll: frame age at paint, p50 | 47 ms | 45 ms | 46 ms |
| Drag: distinct frames per second | 27.8 | 29.9 | 29.9 |
| Video region: distinct frames per second | 24.7 | 30.0 | 30.0 |
| Video region: gap between frames, p95 | 116 ms | 57 ms | 59 ms |
| Video region: frame age at paint, p50 | 48 ms | 45 ms | 46 ms |
| CPU per painted frame, scroll / drag / video | 50.6 / 41.0 / 38.7 ms | 46.1 / 40.1 / 35.4 ms | 46.2 / 41.0 / 36.1 ms |
| of which QEMU | 13.3 / 8.3 / 6.6 ms | 8.1 / 6.7 / 4.8 ms | 8.3 / 6.7 / 4.7 ms |
| of which gliss-server | 22.2 / 16.8 / 17.4 ms | 22.4 / 16.8 / 15.2 ms | 22.4 / 17.3 / 16.2 ms |
| of which browser | 15.4 / 15.9 / 14.8 ms | 15.8 / 16.1 / 15.6 ms | 15.4 / 17.0 / 15.5 ms |
| Media bitrate, scroll / drag / video | 0.58 / 1.31 / 1.27 Mbit/s | 0.81 / 1.35 / 1.59 Mbit/s | 0.67 / 1.20 / 1.54 Mbit/s |
| Bytes per painted frame, scroll / drag / video | 3518 / 5837 / 6434 | 3391 / 5604 / 6605 | 2810 / 4970 / 6420 |
| QEMU timer at 30 ms, scroll / drag / video | 54% / 86% / 67% | 100% / 100% / 92% | 100% / 100% / 92% |
| Idle desktop, 5 s: gliss-server / QEMU CPU | 5 / 45 ms | 0 / 50 ms | 0 / 50 ms |

Each cell is the median of four rounds; the three builds alternated within one
run on one guest. The scroll scene moves a text region at 60 Hz, the drag scene
moves a window with the pointer, and the video scene redraws a 640×360 region
at 30 Hz. The host was an Intel i5-13600KF running Linux 7.1, QEMU 11.1 and
Chromium 149 with software rendering, shared with other load.

Scroll and video send more bits per second after the change because they show
46% and 21% more frames; bytes per frame fall. The remaining 55 ms gap in
scroll is the one frame in ten that the 30 fps cap drops from QEMU's 33. QEMU
cannot send more than that, so `--fps 34` or higher would remove the drop.

A separate ablation on the same guest, three rounds per step:

| Cumulative change | Typing p50 | Scroll fps | Scroll gap p95 | Drag p50 | Video fps | QEMU CPU per scroll frame |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| alpha.8 | 91 ms | 20.9 | 111 ms | 55 ms | 17.4 | 12.8 ms |
| Native pixel format | 88 ms | 21.9 | 113 ms | 55 ms | 16.1 | 9.4 ms |
| Request on arrival | 91 ms | 30.0 | 49 ms | 70 ms | 24.6 | 8.0 ms |
| Refresh hold | 37 ms | 30.0 | 49 ms | 70 ms | 30.0 | 8.3 ms |
| Drop instead of delay | 40 ms | 30.1 | 55 ms | 55 ms | 30.0 | 8.0 ms |

Requesting on arrival alone made drag latency worse, because every frame then
queued behind the cap; dropping instead of delaying took that back. On alpha.8
a session settled at about 16 or 25 frames per second in the video scene for
its whole length, which is why its video rate differs between the two tables.

For the keyframe interval, an encoder-only probe on synthetic 1920×1080 content
at the Distribox settings compared 240 with 3600 frames over ten seconds.
Bitrate fell by 52% for typing, 33% for scroll and 16% for a video region, and
mean luma PSNR rose by 0.6 to 1.0 dB. Each periodic keyframe was about 0.4 MB
and reset the picture to a coarser quality.

A hill-climb session scored the first four changes together on the latencies,
the time between distinct frames and the CPU per frame above, as a ratio to
alpha.8 measured in the same run. Three samples scored 0.703, 0.703 and 0.723
against 0.985, 1.027 and 1.036 for alpha.8 against itself. The [raw
record](../benchmarks/distribox-path-2026-11-11.json.gz) holds every sample of
the final run, the ablation summaries, the session log and the evaluator.

## Limits

- One host, one guest, one resolution, a synthetic desktop and a CPU-only
  browser. The numbers describe these builds on this workload, not Distribox
  deployments in general.
- Distribox's WebSocket relay, a WAN and a real display are not in the path.
  Paint means Canvas submission in an animation frame, not light from a screen.
- Frame age is short by the serial line's shortest delivery time, the same for
  every build.
- A pointer position counts as shown when the window reaches or passes it, so
  drag latency is the time to catch up, not the time to show each position.
- The benchmark checks the marker strips, not the whole picture. Pixel
  correctness is covered by the test suites.
- The refresh hold relies on QEMU's handling of client messages. Another VNC
  server, or QEMU without an audio backend on its VNC display, gets changes 1,
  2, 4 and 5 only.
- Sliced multi-threaded encoding was tried in the encoder probe and did not
  shorten encode time on this host, so the encoder still uses one thread.

## Reproduce

```sh
cargo build --locked --release -p gliss-server -j 2
TMPDIR=/dev/shm node scripts/bench-distribox-path.mjs \
  --binary /path/to/older/gliss-server --label before \
  --binary "$PWD/target/release/gliss-server" --label after \
  --rounds 4 --trace-refresh --output /dev/shm/distribox-path.json
```

The [development guide](development.md#distribox-path-benchmark) lists the
prerequisites. `--trace-refresh` adds the time QEMU spent at each refresh
interval.
