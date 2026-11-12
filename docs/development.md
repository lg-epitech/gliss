# Development

Run these commands from the repository root on Linux. The toolchain is Rust 1.98.0 with rustfmt and Clippy, which `rust-toolchain.toml` selects, and Node.js 24.10.0 with npm. Keep Cargo.lock and package-lock.json in place.

Install a C/C++ compiler, linker, make, NASM and CMake. The workspace builds OpenH264 and Opus from bundled source; no installed FFmpeg, OpenH264 or Opus library is needed. On Debian or Ubuntu, install `build-essential nasm cmake`.

```sh
rustup toolchain install 1.98.0 --profile minimal --component rustfmt --component clippy
npm ci
npx playwright install --with-deps chromium
```

Playwright's `--with-deps` installs Chromium's Linux system dependencies and can require administrator access. The repository pins Playwright and uses its installed Chromium by default. Set `CHROMIUM_PATH` only to select an installed Chromium explicitly. Browser tests use the shared [CPU browser flags](../tests/helpers.mjs), which disable GPU rendering, compositing and hardware video encoding and decoding.

## Build and check

Run builds and suites sequentially. Cargo compilation is limited to two jobs, and the repository configuration defaults Rust tests to two threads.

```sh
cargo fmt --all -- --check
cargo test --locked --workspace -j 2
cargo clippy --locked --workspace --all-targets -j 2 -- -D warnings
cargo build --locked --release --workspace -j 2
export GLISS_BINARY="$PWD/target/release/gliss-server"
npm test
```

`GLISS_BINARY` selects the server the browser tests start and must be an absolute path. Without it they use `target/release/gliss-server` in this checkout.

`npm test` runs the `tests/*.test.mjs` suite serially. Its browser cases drive headless Chromium against the release server, fed by the `--demo` source or by a [mock RFB peer](../tests/mock-rfb.mjs). The suite needs no VM and does not cover a real QEMU guest; the two smoke tests and the benchmark below do.

Two more checks run on their own: the [video crate](../crates/gliss-video/README.md#validation) has decoder tests and a browser decode probe, and [verification/frame-ownership](../verification/frame-ownership/README.md) holds a Lean model of decoded-frame ownership in the client.

## Fuzz tests

The fuzzers are ordinary tests named `fuzz_...`: they run with `cargo test` and `npm test` on the stable toolchain, without cargo-fuzz. Each one writes inputs from a grammar of its protocol, compares what the code does with a reference model, then changes bytes of the input and compares again. Inputs come from a seeded generator in the test code, so every run is the same, and a failure prints the seed, the iteration and the variables that replay it alone. There is no coverage feedback: what a fuzzer reaches is what its grammar and its byte changes can write.

| Variable | Effect |
| --- | --- |
| `GLISS_FUZZ_ITERATIONS` | Iterations of each fuzz test that runs, instead of its default. |
| `GLISS_FUZZ_SEED` | Another sequence of inputs. The default seed is fixed for each target. |
| `GLISS_FUZZ_START` | The first iteration number. With one iteration it replays a failure. |
| `GLISS_FUZZ_VECTORS` | A directory. The Rust ADPCM and GLV1 fuzzers write their inputs and verdicts there, and the Node tests check the browser's decoders against them. |
| `GLISS_FUZZ_WRITE_VECTORS` | Rewrite the vector files in `test-support/fuzz`, after a deliberate change of a wire format. |

By default each fuzz test takes well under a second in a debug build. For a campaign, name one test and a count:

```sh
GLISS_FUZZ_ITERATIONS=30000 cargo test --locked -p gliss-rfb --lib fuzz_server_stream -- --nocapture
export GLISS_FUZZ_VECTORS=/dev/shm/gliss-vectors
GLISS_FUZZ_ITERATIONS=50000 cargo test --locked -p gliss-video --lib fuzz_ -- --nocapture
GLISS_FUZZ_ITERATIONS=100000 node --test tests/fuzz-video.test.mjs
```

The tests are `fuzz_server_stream` and `fuzz_clipboard` in gliss-rfb; `fuzz_media_window`, `fuzz_session_cookie`, `fuzz_login_allowance`, `fuzz_held_keys`, `fuzz_guest_events`, `fuzz_audio_packets` and `fuzz_adpcm_blocks` in gliss-server; `fuzz_packet_parser` and `fuzz_encoded_packets` in gliss-video; and `tests/fuzz-audio.test.mjs` and `tests/fuzz-video.test.mjs` for the browser's parsers. `test-support/fuzz` holds the vectors both languages are checked with; a Rust test fails when they no longer match what the Rust side produces.

When a campaign fails, reduce the input and keep it as a named test next to the fuzzer, with a sentence on what went wrong, so that every run checks it.

## Real QEMU audio smoke

This opt-in test boots a temporary diskless Linux guest with an output-only HDA
device. Its ALSA player generates different tones on the left and right channels.
The test checks those tones at the browser's actual Web Audio output after QEMU
VNC capture, Gliss Opus encoding and native browser decoding; it repeats with the
IMA ADPCM fallback. It also checks wire bitrate, bounded queues, video decoding
and mute cleanup. It runs outside the portable `npm test` suite.

```sh
export GLISS_BINARY="$PWD/target/release/gliss-server"
node scripts/test-qemu-audio.mjs
```

The fixture requires an x86_64 Linux host with a readable kernel and matching
`snd-hda-intel` and `snd-hda-codec-generic` modules, QEMU, KVM access, GCC, ALSA
development headers/library, binutils, Python, cpio and zstd if the modules use
that compression. On Debian or Ubuntu, the additional packages are
`qemu-system-x86 libasound2-dev cpio zstd kmod`. It uses the installed Playwright
Chromium. Set `GLISS_AUDIO_KERNEL` if the kernel is outside `/boot/vmlinuz-$(uname -r)`
or `/boot/vmlinuz-linux`; `GLISS_QEMU_ACCEL=tcg` selects software emulation.

The fixture creates no disk images or libvirt definitions and requires no host
audio daemon. It removes its temporary initramfs and stops its own QEMU and
Gliss processes on completion. The measured bitrate and playback queues describe
this local tone test; they do not establish WAN latency or music quality.

## Real QEMU clipboard smoke

This opt-in test boots a temporary diskless Linux guest with QEMU's `qemu-vdagent`
device. A small test agent stands where `spice-vdagent` normally sits and speaks
the same protocol, so the test compares the exact bytes the guest receives after
browser input, Gliss and QEMU's VNC server. It also copies text from the guest to
the browser. It runs outside the portable `npm test` suite and does not exercise
`spice-vdagent` or guest applications.

```sh
export GLISS_BINARY="$PWD/target/release/gliss-server"
node scripts/test-qemu-clipboard.mjs
```

The fixture requires an x86_64 Linux host with a readable kernel that has built-in
virtio-pci and virtio-console, QEMU with `qemu-vdagent`, KVM access, GCC with a
static libc and cpio. It uses the installed Playwright Chromium. Set
`GLISS_QEMU_KERNEL` to select another kernel; `GLISS_QEMU_ACCEL=tcg` selects
software emulation. It creates no disk images or libvirt definitions and removes
its own temporary files and QEMU process.

## Distribox path benchmark

This opt-in benchmark boots a temporary diskless guest on QEMU/KVM with a
virtio GPU, a PS/2 keyboard and a virtio tablet, as Distribox defines its VMs.
The guest draws a small desktop with machine-readable markers, so the browser
side can tell which key press, pointer position and animation frame each
painted frame shows. Each server binary runs in turn with Distribox's flags:

```sh
cargo build --locked --release -p gliss-server -j 2
TMPDIR=/dev/shm node scripts/bench-distribox-path.mjs \
  --binary "$PWD/target/release/gliss-server" --rounds 3 --output /dev/shm/run.json
```

Pass `--binary` again to compare builds in alternating order. It needs
`/dev/kvm`, QEMU, a readable host kernel with matching `virtio-gpu` and
`virtio_input` modules, GCC with a static libc, cpio, zstd and the installed
Playwright Chromium. The temporary directory must be on tmpfs, because the test
browser keeps its shared memory there and stalls when that is a busy disk. It
creates no disk images or libvirt definitions and removes its own temporary
files and processes. The [Distribox path notes](distribox-path.md) describe
what it measures and its limits.

`--link` puts an emulated WAN between the server and the browser:

```sh
TMPDIR=/dev/shm node scripts/bench-distribox-path.mjs \
  --binary "$PWD/target/release/gliss-server" --scenarios scroll,drag,video,pages \
  --link rtt=100,down=1 --output /dev/shm/wan.json
```

The run then moves into a user and network namespace of its own, which needs
no privilege, and the kernel delays and rate-limits the packets on their way
to a browser in a second namespace. Besides the above it needs unprivileged
user namespaces, `unshare`, `nsenter`, `ip`, `tc` and the `veth` and
`sch_netem` kernel modules. The settings are the round trip in ms (`rtt`),
the rates in Mbit/s towards the browser and back (`down`, `up`), the
bottleneck's buffer in ms (`queue`), random loss in percent (`loss`) and the
relay host's `tcp_slow_start_after_idle` (`idle`); `down=4/0.8@3` alternates
between two rates every three seconds. The [WAN notes](wan-path.md) describe
the model and what it measured.

## Streaming controls

For bounded browser stage timings and native CPU diagnostics, see [desktop stream profiling](profiling.md).

The stream sends a keyframe every 3600 encoded frames, two minutes at 30 FPS
and the longest interval the encoder accepts. The stream is reliable and the
client asks for a keyframe after a decode error, so a periodic keyframe only
costs a burst of bytes and resets the picture to a coarser quality. New
sessions, resyncs and resizes still force an immediate keyframe.
`--video-intra-period N` sets another interval, from 1 to 3600 frames.

The audio player prefers Opus, with advertised IMA ADPCM and PCM fallbacks. The
[audio wire format](audio-wire.md) describes codec negotiation, payload bounds
and freshness limits.

`--audio-packet-ms 20|40` selects a fixed packet duration before opening the
source. Omission keeps 20 ms. Longer packets reduce packet/header overhead and
increase assembly delay; they leave less time within the same 100 ms ingress-age
budget. `hello.audio_packet_ms` and the authenticated `/api/stats` response
report the selection.

With `--stream-stats`, the server sends a `stream_stats` message every second.
It includes `audio.packet_ms` and `audio.counters`, which count emitted
packets/frames/bytes, stale raw or partial audio, pending resets, raw/writer
broadcast lag, encoder errors and writer age rejections. Counts are cumulative
per viewer connection, and each field is sampled independently. Raw data
arriving while audio is disabled is uncounted. Pending resets combine
stale-input, capture, subscription and lag resets. Emission is not proof of
delivery; a pre-write age check cannot expire bytes already in TCP.

Accepted HTTP/WebSocket sockets enable `TCP_NODELAY` by default.
`--tcp-nodelay false` restores coalescing. The selected option is read back
before HTTP uses each accepted socket; `hello.tcp_listener` reports verification
counts. This setting does not remove congestion or retransmission delays.

## Releasing

`pyproject.toml` packages `gliss-server` as a binary-only Python wheel with [maturin](https://www.maturin.rs/), so Python projects can depend on it without a Rust toolchain. Build one locally with:

```sh
pip install "maturin>=1.15,<2"
maturin build --release --locked
```

The [release workflow](../.github/workflows/release.yml) builds manylinux_2_28 wheels and standalone tarballs for x86_64 and aarch64, then installs each wheel with pip on clean images and starts the demo server. Pull requests that touch packaging run those jobs without publishing. To release, set the version in the root `Cargo.toml`, merge, then push a matching tag:

```sh
git tag v0.1.0-alpha.1 && git push origin v0.1.0-alpha.1
```

The wheel and tarballs ship `licenses/rust-dependencies.md`, the license texts of every Rust crate linked into the binary. The release workflow fails if it no longer matches `Cargo.lock`. After changing dependencies or the workspace version, regenerate it with [cargo-about](https://github.com/EmbarkStudios/cargo-about) 0.9.2:

```sh
cargo fetch --locked
cargo about generate --frozen --fail -c licenses/about.toml -m crates/gliss-server/Cargo.toml \
  licenses/about.hbs -o licenses/rust-dependencies.md
```

The tag must equal `v` plus the Cargo version. The workflow then publishes the wheels to PyPI through trusted publishing and creates a GitHub release with the wheels, tarballs and `SHA256SUMS`. Cargo pre-release versions such as `0.1.0-alpha.1` become PEP 440 versions such as `0.1.0a1` on PyPI.
