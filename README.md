# Gliss

An experimental codec for streaming VM desktops to the browser. A Rust server captures a VM over VNC, encodes on the CPU and forwards keyboard and mouse input back. The browser needs no GPU, guest agent or plugin. Alpha quality. The code, experiments and docs were written entirely by AI agents. Laurent Gonzalez provided the infrastructure and oversaw the work.

| Workload (1280×800 VM, CPU only) | Stack | p95 response | Traffic |
| --- | --- | ---: | ---: |
| Terminal typing, 40 ms RTT | **Gliss Batch** (lossless) | 212 ms | 0.19 Mbit/s |
| | **Gliss H.264** | 213 ms | 0.10 Mbit/s |
| | Guacamole | 218 ms | 0.05 Mbit/s |
| Moving desktop, 150 ms RTT + 1% loss | **Gliss all-IDR H.264** | 460–464 ms | 4.2 Mbit/s |
| | WebRTC H.264 (aiortc) | failed: missed inputs | 0.85 Mbit/s |
| | WebRTC VP8 (aiortc) | failed: missed inputs | 0.5 Mbit/s |
