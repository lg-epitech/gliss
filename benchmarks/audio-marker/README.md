# Guest audio markers and mixer fidelity

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
