# Desktop stream profiling

Profiling is opt-in. The normal client does not load the recorder, start timers,
observe long tasks or send probes. Native CPU clocks and publication timestamps
are sampled only with `--stream-stats`. Recording adds measurement overhead;
compare the same workload and settings with profiling both enabled and disabled.

## Record a browser session

Use the existing `GlissClient` instance; do not open a second desktop connection:

```js
const recording = await client.startProfiling({
  durationMs: 15000,
  probeIntervalMs: 1000,
});
// Inspect while recording, or stop early. Otherwise it stops automatically.
const progress = recording.snapshot();
const report = recording.stop();
```

On the standalone viewer, the instance is `window.gliss`. Embedded viewers own
and pass their instance directly. Omit `probeIntervalMs` to send no probes.
Control probes use the existing connection and never inject guest input.
Only one recorder can run per client. Recording stops on disconnect or disposal;
a pending start is cancelled and reconnects never merge into an old recording.
`client.stopProfiling()` also cancels a pending start or stops the active recorder.
The returned handle retains its immutable final report after automatic stop.

Record a short idle baseline, then a separate recording during window movement,
scrolling or sound playback. Keep resolution, codec, frame window and bitrate
constant between comparisons. A static desktop need not emit frames at the FPS
cap: report completed frames and their cadence without treating idle gaps as lag.

The `gliss-profile-v1` JSON export contains only fixed numeric fields and known
labels. It excludes URLs, credentials, VM/session IDs, pixels, clipboard text,
keystrokes, command lines and arbitrary server/error strings. Reports stay local;
the recorder has no upload endpoint. Collection is limited to 60 seconds, 1,024
frame rows, 60 native snapshots and 256 KiB of exported JSON. Omission counters
identify capped samples; timing distributions then describe the retained prefix,
not the whole window. Control probes are limited to one per second or slower,
with at most four unanswered probes and 60 sent probes.

## Attribute time and CPU

Browser frame rows split receipt-to-Canvas time into the serial queue, decoder
elapsed time, rAF wait and Canvas submission. These disjoint local stages can
be added for one frame. Decoder time includes parsing, configuration and waiting
for output. Canvas submission is not physical display presentation. Page long
tasks measure main-thread wall time across the whole page, not Gliss-only CPU.
Audio queues and client socket buffering are sampled every 250 ms and can miss
short peaks. Browser CPU, guest rendering and physical speaker/display latency
are not measured by this recorder.

Start the native server with `--stream-stats` to include its existing diagnostic
messages. This is an operator setting; the browser cannot enable it remotely.
Diagnostics now include native process user/system CPU, calling-thread encode
CPU, encode elapsed time, encoder-worker wait, publication-to-encode age,
write elapsed time and enqueue-to-ACK time. Unsupported clocks and missing
snapshots remain unavailable, not zero. Process CPU includes all sessions and
threads; 100% means one full core and totals can exceed 100%.

Native summaries use differences between the first and last retained native
snapshots, over their own measured interval. A cumulative maximum is labelled
`lifetime_max_ms`; it is not an interval maximum. Source age starts at native
RFB/private D-Bus publication, excluding guest rendering and upstream capture;
it is unavailable for demo/shared D-Bus sources. It includes worker wait.
Enqueue-to-ACK also overlaps browser processing and return traffic. Do not add
native timing fields as if they were disjoint CPU costs.

Browser and native clocks are separate. Control round trip includes outgoing
and return traffic, relays and event-loop scheduling; half of it is not proven
one-way latency. Relay residence and guest render time require separate evidence.
To inspect JavaScript stacks, use the browser's Performance panel on the same
workload. Raw browser traces can contain sensitive URLs; do not attach them to
public issues or commit them. Use the bounded JSON report for sharing.

This initial recorder supports WebSocket desktops. Datagram and tile transports
reject recording explicitly. Profiling does not change codec settings, buffers,
frame pacing, ACK order or recovery policy.
