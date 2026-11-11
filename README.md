# Gliss

Streams a VM's desktop to the browser. A Rust server captures the display and audio of a QEMU VM over VNC, encodes H.264 on the CPU, sends it over a WebSocket and forwards keyboard and mouse input back. It needs no GPU, guest agent or browser plugin. Alpha quality. The code and docs were written entirely by AI agents. Laurent Gonzalez provided the infrastructure and oversaw the work.

| 1920×1080 QEMU guest, CPU only, 30 fps cap | Result |
| --- | ---: |
| Typing echo, p50 / p95 | 40 / 55 ms |
| Window drag, pointer to paint, p50 / p95 | 55 / 75 ms |
| Scroll and video frame rate | 30 fps |
| Bitrate during scroll / drag / video | 0.67 / 1.20 / 1.54 Mbit/s |

Measured at 0.1.0-alpha.9 on one host; [how](docs/distribox-path.md). See also the [architecture](docs/architecture.md), [deployment and limits](docs/security.md) and [development](docs/development.md).
