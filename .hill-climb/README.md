# Lossless decoder experiment

Run `bash .hill-climb/checks.sh` and `bash .hill-climb/benchmark.sh` from the
checkout. The benchmark needs Linux, Playwright Chromium and CPUs 0,2,4,6.
It uses the repository's pinned CPU browser flags.

The primary metric is `decoder_log_ms`, the natural logarithm of the geometric
mean decode milliseconds across 24 fixed synthetic cases. Lower is better.
Each case has equal weight. A gain greater than `ln(1.03)` requires more than a
3% ratio improvement; the session also requires gain beyond three times the
median absolute deviation of either set of repeated samples. This is a noise
heuristic, not a statistical confidence guarantee.

Cases cover two resolutions and raw tiles, full-width strips, legacy and global
palettes, compressed palettes, solid tiles, compressed and global XOR, copies,
sparse raw updates, sparse patches and empty deltas. Packet generation, warmup,
and independent scalar pixel checks are excluded from timing. Keyframes are
untimed. Raw logs include per-case samples, packet and source hashes, runtime,
hardware and affinity. CPU Chromium decoder time is a component measurement;
it does not measure a native encoder, network, Canvas submission or display.

Independent checks use retained Rust wire/pixel fixtures, derive damage and
metadata from the wire, verify Canvas composition, preserve earlier returned
frames, and reject malformed plain/compressed packets without changing the
reference. They also check palette alpha/bounds, recovery and sequence wrap.
Before freezing, these checks rejected deliberately broken overlap and damage
handling. They do not cover real guest capture, GPU decoding, or every possible
malformed packet.

The session freezes this directory, dependencies and correctness fixtures. Only
`web/decoder.js` is editable. Its state and raw logs live outside the checkout
under `~/.local/state/hill-climb/gliss/lossless-2026-11-05/`. The default limit is
25 experiments and nine consecutive unsuccessful experiments. A retained proxy
gain still needs per-case inspection and an independent client/browser
comparison before publication; aggregate wins can hide regressions.
