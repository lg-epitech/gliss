# Buffering QEMU framebuffer reads

QEMU sends many small Raw rectangles when a terminal scrolls. Gliss used to
read the five fields of each rectangle header from the socket one at a time,
then read the pixel data for each partial-width row. Gliss now allocates a
32 KiB read buffer per VNC connection. The decoder can take several headers
and rows from one socket read. When the buffer is empty, large contiguous reads still
go straight into the framebuffer.

## Through the Distribox relays

On scroll, Gliss CPU per painted frame fell **33.8%, from 16.35 to 10.83
ms**. All four buffered runs used less CPU than any baseline run. Across
Gliss, QEMU, Chromium and the two Python processes combined, CPU fell from
33.81 to 28.28 ms per painted frame (16.4%). Chromium painted 30.0 and
30.1 distinct frames/s respectively. Median frame age at paint fell from
35.7 to 30.1 ms, and median key-to-paint latency during scroll fell from
47.3 to 42.3 ms.

Window dragging and page turns had smaller median CPU differences with
overlapping round measurements. Typing and video were within 0.01 ms per
painted frame of baseline. Drag media bitrate rose from 1.24 to 1.37 Mbit/s
at 29.9 distinct frames/s on both builds. The benchmark recorded no missed
key or pointer probes, unreadable markers, skipped guest ticks or client
decode errors.

A real QEMU/KVM guest drew a synthetic desktop at 1920×1080. The benchmark
started one Gliss process per viewer with a 30 fps cap, an 8 Mbit/s H.264
target and eight presentation credits, matching the local Distribox checkout.
It ran the slave and master forwarding functions in separate Uvicorn processes
on loopback. Chromium ran the Gliss client supplied by each binary and read
the guest markers back from Canvas. The harness supplied test routes and
authentication. The React shell, nginx, TLS, external networks and physical
display latency are outside this measurement.

Each build ran four times on the same guest. Their order reversed each round.
For each activity, we divided the Gliss process’s CPU time by the number of
frames the browser painted and took the median across rounds. CPU time comes
from Linux process accounting in 10 ms ticks. Sparse activities such as
isolated typing produce fewer frames and coarser CPU estimates. The host was
an Intel i5-13600KF shared with other processes. The benchmark used one
hardware thread from each of its six performance cores.

## Socket reads

A separate run counted successful socket reads on the Gliss-to-QEMU connection.
At the 2 Mbit/s Distribox default, successful reads per painted scroll frame
fell from **31,422 to 53** (99.8%). Both builds read about 1.7 MB of VNC data
per painted frame. This probe used a direct browser connection. The two-relay CPU
measurements used runs with the counter disabled.

A fixed loopback replay verified that both builds processed identical data.
Each update contained 64 short Raw rectangles and arrived in two writes. Across
18 updates, both builds consumed 161,352 bytes and emitted byte-identical
GLV1 packets. Successful reads fell from 388 to 2 per frame in all three
repetitions. A kernel trace confirmed the buffered reader consumed the two
pieces. Counts on a live connection depend on the rectangles and on how TCP
delivers them.

## Experiments

The hill climb used Gliss CPU milliseconds per painted scroll frame at 8 Mbit/s
on a direct connection. Each candidate needed three runs, readable guest
markers, all input probes, and at least 28 distinct fps. An improvement had
to exceed 0.2 ms and twice the observed median absolute deviation of both
builds. The baseline took 16.67 ms and the 32 KiB buffer took 10.66 ms.
Starting from that buffered reader, reading the header in one operation took
10.66 ms, AVX2 pixel conversion took 10.83 ms, and increasing the buffer to
128 KiB took 10.92 ms. All three fell short of the gain threshold.

## Reproduce

Build beta.2 and the PR in release mode with the same Rust flags. For the
two-relay run, install the dependencies into `~/projects/Distribox/backend/venv`
from Distribox commit `303df508a3f1ef46a4596e28c3058c04ed96f366`.
From the root of this Gliss checkout, extract the benchmark driver into a
fresh temporary directory and run it with both binaries. The driver imports
the relay code from `~/projects/Distribox`.

```sh
driver=$(mktemp -d)
python3 - "$driver" <<'PY'
import gzip, json, pathlib, sys
with gzip.open('benchmarks/distribox-rfb-reads-2026-11-21.json.gz', 'rt') as f:
    driver = json.load(f)['replayFiles']
for name in ('run.sh', 'prepare.py', 'relay.py', 'scripts/relays.mjs'):
    dest = pathlib.Path(sys.argv[1]) / name
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_text(driver[name])
PY
TMPDIR=/dev/shm bash "$driver/run.sh" \
  --binary /absolute/path/to/beta2 --label before \
  --binary /absolute/path/to/candidate --label after \
  --h264-bitrate 8000000 --rounds 4 --typing-keys 12 \
  --scenarios typing,scroll,drag,video,pages --output /dev/shm/relays.json
```

The [compressed record](../benchmarks/distribox-rfb-reads-2026-11-21.json.gz)
contains the binaries’ SHA-256 hashes, per-round timings, guest markers,
socket counters, environment and evaluator. The [development guide](development.md#distribox-path-benchmark)
lists the QEMU and Chromium prerequisites.
