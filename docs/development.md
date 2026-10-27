# Development

Run these commands from the repository root on Linux. The release toolchain is Rust 1.98.0 with rustfmt and Clippy, Node.js 24.10.0 with npm, and Python 3.14. Keep Cargo.lock and package-lock.json in place.

Install a C/C++ compiler, linker, make, NASM and CMake. The workspace builds OpenH264 and Opus from bundled source; no installed FFmpeg, OpenH264 or Opus library is needed. On Debian or Ubuntu, install `build-essential nasm cmake`; Python virtual environments require the matching Python venv package.

```sh
rustup toolchain install 1.98.0 --profile minimal --component rustfmt --component clippy
rustup override set 1.98.0
python3.14 -m venv .venv
.venv/bin/python -m pip install -r integration/distribox/prototype/requirements.txt
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
npm run test:vendor
```

`GLISS_BINARY` selects the freshly built executable. The public browser helper otherwise uses `target/release/gliss-server` in this checkout. The Linux native-FD test uses an owned mock RFB peer and requires Python, `/proc` and pidfds; it needs no prepared VM. Codec and tile interoperability fixtures are retained under [test-support](../test-support/).

`npm test` runs the portable `tests/*.test.mjs` suite and the bounded inflater contracts serially. The suite needs no VM; it does not cover real QEMU or D-Bus guest integration. `test:vendor` runs the inflater contracts separately, including malformed-input and provenance checks.

## Reference gateway

The adapter unit tests require no native worker or browser:

```sh
.venv/bin/python -B -m unittest discover -s tests/integration-adapter -v
.venv/bin/python -B -m unittest \
  integration.distribox.prototype.tests.test_codec_presets \
  integration.distribox.prototype.tests.test_gateway.SchemaTests -v
node --test integration/distribox/prototype/tests/viewer-media.test.mjs
```

After the release build, run the owned-peer integration cases sequentially:

```sh
export GLISS_BINARY="$PWD/target/release/gliss-server"
export GLISS_PROTOTYPE_PYTHON="$PWD/.venv/bin/python"
.venv/bin/python -B -m unittest discover -s integration/distribox/prototype/tests -v
node --test --test-concurrency=1 integration/distribox/prototype/tests/browser.test.mjs
node --test --test-concurrency=1 integration/distribox/prototype/tests/browser-glh4.test.mjs
```

Without `GLISS_BINARY`, some Python native cases and the lossless browser case skip; those skips do not establish integration correctness. The GLH4 browser case fails without the explicit executable. These tests use fresh mock RFB peers, not real VM attachment. See the [prototype guide](../integration/distribox/prototype/README.md) for the precise coverage.

## Benchmarks

The verifier recomputes the published terminal table and exclusion counts from the retained measurement extract. It does not rerun the historical experiments:

```sh
python3.14 -B benchmarks/verify.py
```

The component benchmark generates deterministic synthetic frames and checks decoded pixels while measuring codec CPU time and bytes. It replaces the selected output file if it exists:

```sh
cargo run --locked --release -j 2 -p gliss-codec --bin codec-bench -- \
  --repetitions 3 --output output.json
```

Its output is a local component measurement, not a live desktop, browser latency or network comparison. See [benchmarks](benchmarks.md) for the published results and their limits.

## Real QEMU audio smoke

This opt-in test boots a temporary diskless Linux guest with an output-only HDA
device. Its ALSA player generates different tones on the left and right channels.
The test checks those tones at the browser's actual Web Audio output after QEMU
VNC capture, Gliss Opus encoding and native browser decoding; it repeats with the
PCM fallback. It also checks wire bitrate, bounded queues, video decoding and
mute cleanup. It runs outside the portable `npm test` suite.

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

The cross-repository smoke also exercises Distribox's real React sound button,
backend process startup, exported browser modules and WebSocket relay for local
credential and slave VM access:

```sh
export DISTRIBOX_ROOT=/absolute/path/to/Distribox
export DISTRIBOX_PYTHON=/absolute/path/to/Distribox/backend/venv/bin/python
node --test integration/distribox/audio-client.test.mjs
```

Install the frontend dependencies in that Distribox checkout and the backend
requirements, including the audio-capable Gliss wheel, in its Python environment
first. This test uses the wheel's actual executable and embedded web assets. It
stubs authentication/database lookups and VM discovery to target only its own
disposable QEMU guest; it does not validate deployed credentials, networks or VM
provisioning. VM-id access is entered through client navigation after hydration.

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
