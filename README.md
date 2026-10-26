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

Compare rows within a workload only. Each cell covers two short runs, and image quality is not matched. Guacamole uses the least bandwidth. Batch is the only stack with pixel-exact text. On a clean link, WebRTC H.264 is about as fast as Gliss. See [benchmarks](docs/benchmarks.md).

**Spec in brief.** [Batch](docs/codec-spec.md) sends changed 64-pixel tiles as raw RGBA or copies from the previous frame, compressed as one zlib stream per update over WebSocket. It is lossless. [Live video](docs/live-video-spec.md) makes every frame an OpenH264 IDR and splits it into QUIC datagrams of up to 1,000 bytes over WebTransport. It has no retransmission or frame ACK, and the browser paints the newest complete frame. Input travels over a reliable channel.

```sh
cargo build --locked --release   # Linux, Rust 1.98, C toolchain, NASM
GLISS_TOKEN=$(openssl rand -hex 32) target/release/gliss-server --demo --codec batch
```

Open `http://127.0.0.1:8080` and enter the token. To stream a VM, use `--vnc HOST:PORT` instead of `--demo`. Read the [security](docs/security.md) guide before exposing the server; also see [architecture](docs/architecture.md) and [development](docs/development.md). MIT licensed ([third-party notices](THIRD_PARTY_NOTICES.md)).
