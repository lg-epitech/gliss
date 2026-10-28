# Benchmarks

All runs used CPU-only encoding and Chromium with software rendering and decoding, on one shared host. Networks are local models, not Internet paths. Each cell covers two short runs, so the figures describe recorded binaries and workloads rather than population statistics or capacity. Nearest-rank p95 over ten samples is the sample maximum.

## Terminal typing: Gliss and Guacamole

A QEMU guest (one vCPU, 768 MiB, 1280×800) ran a 24-second idle, color, typing, scroll and drag sequence. Each row has 20/20 responses across two reversed-order repetitions. Latency runs from Enter keydown to the returned root-window pixels at the next animation frame. Traffic covers the whole sequence and is shown as repetitions 0/1.

| Network | Pipeline | Response p50 / p95, ms | Downstream Mbit/s, r0 / r1 |
| --- | --- | ---: | ---: |
| Local | Gliss Deflate, lossless | 195.7 / 212.4 | 0.213 / 0.215 |
| Local | Gliss Adaptive, lossless | 253.2 / 268.1 | 0.103 / 0.102 |
| Local | Gliss Batch, lossless | 145.9 / 159.9 | 0.191 / 0.191 |
| Local | Gliss software H.264 | 159.4 / 165.6 | 0.104 / 0.103 |
| Local | Guacamole | 136.1 / 169.9 | 0.049 / 0.049 |
| Modeled WAN | Gliss Deflate, lossless | 245.0 / 278.6 | 0.211 / 0.216 |
| Modeled WAN | Gliss Adaptive, lossless | 280.3 / 314.1 | 0.100 / 0.099 |
| Modeled WAN | Gliss Batch, lossless | 194.4 / 211.6 | 0.193 / 0.192 |
| Modeled WAN | Gliss software H.264 | 206.3 / 213.3 | 0.103 / 0.103 |
| Modeled WAN | Guacamole | 191.9 / 218.2 | 0.050 / 0.051 |

The WAN model schedules ordered TCP chunks at a nominal 40 ms RTT with per-chunk jitter and 20 Mbit/s per connection. It injects no loss. Rates count browser-facing HTTP/WebSocket bytes and exclude TCP/IP/TLS overhead. Guacamole's capture cap was not independently matched, so Batch's lower p95 is not a general latency advantage.

Stream-service CPU over the WAN sequence was 0.55–0.61 s for Batch, 0.70–0.76 s for H.264 and 0.44 s for guacd. Browser CPU was 2.0–2.3 s for all three.

After typing ended, a 480×24 static terminal crop was compared with a guest screendump:

| Pipeline | Crops | RGB mean absolute error | Maximum channel error | Pixels differing |
| --- | ---: | ---: | ---: | ---: |
| Gliss Deflate / Adaptive / Batch | 12 | 0 | 0 | 0% |
| Gliss software H.264 | 4 | 3.209–3.214 | 34 | 100% |
| Guacamole | 4 | 0.212–0.240 | 12–16 | 9.17–10.83% |

## Moving desktop: Gliss and WebRTC

A 1280×800 guest drew a 15 fps animated scene with numbered markers. Ten keyed inputs per run were timed from key press to the matching marker on the browser Canvas. Gliss used all-IDR OpenH264 at a 2 Mbit/s target over WebTransport datagrams, with BBR and a 256 KiB queue. The WebRTC baseline used aiortc 1.15 with media retransmission disabled; H.264 started at 1 Mbit/s and VP8 at 500 kbit/s with REMB adaptation. Traffic is UDP payload in both directions.

| Network | Stack | Slowest of 10 responses | Traffic, Mbit/s |
| --- | --- | ---: | ---: |
| Local | Gliss all-IDR H.264 | 177–178 ms | 4.12–4.26 |
| Local | WebRTC H.264 | 187–210 ms | 0.95 |
| Local | WebRTC VP8 | 446–489 ms | 0.47–0.53 |
| 150 ms RTT | Gliss all-IDR H.264 | 343–368 ms | 4.12–4.23 |
| 150 ms RTT | WebRTC H.264 | 355–357 ms | 0.98 |
| 150 ms RTT | WebRTC VP8 | 547–558 ms | 0.54 |
| 150 ms RTT, 1% loss | Gliss all-IDR H.264 | 460–464 ms | 4.21–4.25 |
| 150 ms RTT, 1% loss | WebRTC H.264 | failed: 6/10 answered, worst 1,524 ms; second run did not start | 0.85 |
| 150 ms RTT, 1% loss | WebRTC VP8 | failed: 6–8/10 answered, worst 741–1,276 ms | 0.49–0.55 |

Loss drops every 100th UDP datagram. Locally, Gliss used 13.5–14.6% of one server core, against 28% for WebRTC H.264 and 85% for VP8. Every Gliss frame is independent by design, so a lost fragment costs one picture instead of corrupting later frames. That costs about four times WebRTC H.264's bandwidth. Image quality was not matched across stacks. In the same harness, a 4 Mbit/s Gliss target was slower under loss (744–779 ms) than 2 Mbit/s.

An earlier moving-desktop cohort on a kernel netem link (20 ms delay, 5 ms jitter, 0.5% loss, 20 Mbit/s) also ran Guacamole. Gliss full-frame H.264 passed both impaired trials at 0.44–0.50 Mbit/s. Guacamole failed both on drag restoration, using 4.1–4.3 Mbit/s.

## Data and reproduction

- [Audio performance](performance/audio.md) compares the audio release with two freshness candidates across 36 real-QEMU browser positions. It includes CPU cost, Opus/PCM bandwidth, control contention on a 1 Mbit/s link, and the retained rejected candidate.
- [`benchmarks/reference.json`](../benchmarks/reference.json) holds the terminal and kernel-loss measurements. `python3 benchmarks/verify.py` recomputes the terminal table and exclusion counts from it.
- [`benchmarks/motion/`](../benchmarks/motion/) holds the per-run Gliss and WebRTC moving-desktop results.
- [`benchmarks/component-smoke.json`](../benchmarks/component-smoke.json) is a deterministic codec component run: roundtrips, bytes and CPU on synthetic frames, with no network or browser. Regenerate it with `cargo run --locked --release -p gliss-codec --bin codec-bench -- --repetitions 3 --output output.json`.
