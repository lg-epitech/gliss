# Guest audio markers and mixer fidelity

## Mixer component

On 28 October 2026, four owned QEMU 11.1.0 guests ran explicit mixer frequencies
in **44.1/48/48/44.1 kHz order**. Each tested six source codes over Opus, then
PCM. All **48 marker responses** arrived inside the 1.5-second component limit.
Guest/native/browser CPU placement, nice 19, private devices and resource bounds
match the earlier audio studies. No existing VMs or Distribox services changed.

The 48 kHz guest source and Gliss capture expose a resampling cost in QEMU's
default fixed 44.1 kHz mixer. For code 1's 11 kHz right-channel tone, the source's
nominal amplitude is 0.244141. In PCM, the strongest steady observation was
**0.166085–0.166950 at 44.1 kHz**, versus **0.244125 at 48 kHz**. That is roughly
32% amplitude attenuation, about 3.3 dB, which the 48 kHz setting avoided in this
tone test. It occurs even without Opus compression. This is a narrow frequency
response observation, not a perceptual music score or a bit-perfect audio claim.

Both settings' trigger-to-analyser response times overlapped around 200 ms.
This experiment establishes no latency or CPU speedup from changing the mixer.
It includes serial dispatch, the guest's requested 40 ms ALSA buffer, capture,
encoding, browser buffering, analyser confirmation, polling and IPC. It does not
measure physical speaker latency. See the [fixture description](../../test-support/qemu-audio-marker/README.md).

QEMU documents a fixed mixer frequency of 44.1 kHz by default. For a 48 kHz
Gliss/VNC path, use `out.frequency=48000,out.channels=2,out.format=s16` when
configuring the `none` backend. This avoids the forced 44.1 kHz stage for a
48 kHz guest stream. [QEMU audio options](https://www.qemu.org/docs/master/system/qemu-manpage.html),
[libvirt audio settings](https://libvirt.org/formatdomain.html#audio-backends).

For a new libvirt VM, the corresponding backend is:

```xml
<audio id='1' type='none'>
  <output mixingEngine='yes' fixedSettings='yes'>
    <settings frequency='48000' channels='2' format='s16'/>
  </output>
</audio>
```

Keep the sound device and VNC graphics mapped to that audio ID. Other guest
sample rates can still require conversion; the setting does not remove every
resampling step in every VM configuration.

[The plan](mixer-plan.json) records the predeclared order and source hashes.
[The inventory](results.json) binds all original logs, including every coded
observation and analysis-quality window. Peak amplitudes use code-1 windows with
both channel concentrations above 0.95; all such windows are retained. Verify:

```sh
python3 -B benchmarks/audio-marker/verify.py
```

## Trigger-to-observer latency

The subsequent [latency plan](latency-plan.json) fixed eight six-second
positions: Opus, PCM, PCM, Opus on loopback, then the same order on the kernel
link. Every one of the 48 stereo codes was used once across the cohort. Each
position had six commands at fixed offsets, with a predeclared 700 ms observer
deadline. The source used explicit 48 kHz mixing and native TCP sockets verified
`TCP_NODELAY=true`. All eight positions were measured with valid source/timing
evidence; native workers exited normally and all owned kernel queues were removed.

On loopback, both codecs returned **12/12 timely markers**. Opus responses were
200.1–226.1 ms and PCM responses 200.4–217.4 ms. On the nominal 80 ms RTT,
1 Mbit/s-per-direction kernel link:

- **Opus:** 12/12 timely markers, 254.6–272.8 ms.
- **PCM:** 0/12 timely markers. Three were identified 1,672.8–3,090.8 ms after
  submission; nine were not identified before their position's six-second
  cutoff. All twelve source commands were confirmed by the guest.

The late PCM observations are identifiable old sound, not just delayed pongs.
They occurred despite the browser's local buffer/age checks. Both directional
kernel queues recorded zero packet drops. This supports reducing the fallback's
bandwidth requirement or changing its delivery path; shortening the playback
jitter buffer cannot remove sound already queued upstream.

The observer's longest accepted-analysis gap was 36.4 ms. Startup clock-bracket
widths were 2.2–2.9 ms, and the largest inferred analysis-to-controller callback
upper bound was 11.1 ms, assuming the host/browser clock offset stayed constant
over each six-second position. Those observations help distinguish the seconds
of PCM delay from observer scheduling overhead. They do not synchronize a
physical speaker clock or remove the analyser's confirmation delay.

Individual frequencies differ so codes are never reused. Reversed codec order
equalizes mean frequencies within each network, but the waveforms are not
identical and this is not a matched-quality or CPU comparison. Missing and late
responses remain in the [complete result](results.json); no overall PCM p95 is
inferred from its three identified late markers.

Run `scripts/bench-audio-latency.mjs --output NEW.json` inside the bounded kernel
study launch described in the [transport runbook](../audio-transport/README.md#reproduction).
It requires a fresh user/network namespace, private devices, CPU 0 for the
controller/native/browser, CPU 19 for its owned guest, nice 19, and the captured
host network-namespace ID. The plan and all executable source hashes are retained.

The complete portable suite passed 157 tests, including six new detector and
receipt regressions. [Validation counts](validation.json) separate successful
execution from the PCM deadline failures.
