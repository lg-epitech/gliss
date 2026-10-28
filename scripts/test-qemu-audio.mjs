// Opt-in Linux/KVM smoke test. Boots its own diskless HDA guest; no existing
// VM, disk, libvirt definition or host audio service is used or modified.
// Run after cargo build: GLISS_BINARY="$PWD/target/debug/gliss-server" \
//   node scripts/test-qemu-audio.mjs
// Prerequisites: readable host kernel + matching snd-hda modules, gcc, ALSA
// headers/library, Python, cpio, zstd (if modules are compressed), Playwright.
// GLISS_AUDIO_KERNEL and GLISS_QEMU_ACCEL=tcg override the kernel and accelerator.
import test from 'node:test';
import assert from 'node:assert/strict';
import {startServer, launchBrowser, loginPage} from '../tests/helpers.mjs';
import {startAudioVm} from '../test-support/qemu-audio/fixture.mjs';

test('real QEMU HDA → VNC → Gliss → browser: stereo Opus/PCM, bounded playback and mute', {timeout: 90000}, async t => {
  const vm = await startAudioVm(t);
  const server = await startServer({vnc: vm.vnc, codec: 'h264', fps: 30,
    h264Bitrate: 2000000, inFlightFrames: 4});
  t.after(server.stop);
  const browser = await launchBrowser();t.after(() => browser.close());
  for (const codec of ['opus', 'pcm']) {
    const page = await browser.newPage(), errors = [];
    page.on('pageerror', error => errors.push(error.message));
    if (codec === 'pcm') await page.addInitScript(() => {window.AudioDecoder = undefined;});
    await loginPage(page, server);
    await page.waitForFunction(() => window.gliss.audio.available);
    if (codec === 'pcm') await page.evaluate(() => {window.gliss.audio.codecs = ['pcm'];});
    await page.evaluate(() => {
      window.audioWire = {bytes: 0, frames: 0, packets: 0, first: null, last: null};
      window.gliss.socket.addEventListener('message', event => {
        if (!(event.data instanceof ArrayBuffer) || event.data.byteLength < 20 ||
            new TextDecoder().decode(new Uint8Array(event.data, 0, 4)) !== 'GLA1') return;
        const wire = window.audioWire, header = new DataView(event.data);
        wire.bytes += event.data.byteLength;wire.frames += header.getUint32(16, true);wire.packets++;
        wire.first ??= performance.now();wire.last = performance.now();
      });
      const button = document.createElement('button');button.id = 'test-audio';button.textContent = 'Test audio';
      button.onclick = () => {window.soundRequest = window.gliss.setAudioEnabled(!window.gliss.audio.enabled);};
      document.body.append(button);
    });
    await page.click('#test-audio');await page.evaluate(() => window.soundRequest);
    assert.equal(await page.evaluate(() => window.gliss.audio.codec), codec);
    await page.evaluate(() => {
      const context = window.gliss.audio.context, split = context.createChannelSplitter(2), mute = context.createGain();
      mute.gain.value = 0;mute.connect(context.destination);
      window.audioAnalysers = [context.createAnalyser(), context.createAnalyser()];
      window.audioAnalysers.forEach((analyser, channel) => {
        analyser.fftSize = 4096;split.connect(analyser, channel);analyser.connect(mute);
      });
      const create = context.createBufferSource.bind(context);
      context.createBufferSource = () => {
        const source = create(), connect = source.connect.bind(source);
        source.connect = destination => {connect(split);return connect(destination);};return source;
      };
      window.audioSignal = () => window.audioAnalysers.map(analyser => {
        const samples = new Float32Array(analyser.fftSize);analyser.getFloatTimeDomainData(samples);
        let energy = 0, crossings = 0;
        for (let n = 0; n < samples.length; n++) {
          energy += samples[n] ** 2;
          if (n && samples[n - 1] < 0 && samples[n] >= 0) crossings++;
        }
        return {rms: Math.sqrt(energy / samples.length), hz: crossings * context.sampleRate / samples.length};
      });
    });
    await page.waitForFunction(() => window.audioWire.frames >= 48000 * 3, null, {timeout: 12000});
    await page.waitForFunction(() => window.audioSignal().every(signal => signal.rms > 0.05));
    const result = await page.evaluate(() => ({wire: window.audioWire, player: window.gliss.audio.snapshot(),
      signal: window.audioSignal(), videoFrames: window.gliss.metrics.frames, videoErrors: window.gliss.metrics.errors}));
    const bitrate = result.wire.bytes * 8 * 48000 / result.wire.frames;
    assert.ok(result.videoFrames > 0);assert.equal(result.videoErrors, 0);
    assert.ok(result.player.sources <= 12);assert.ok(result.player.queuedSeconds <= 0.2);
    assert.ok(result.player.decodeQueue <= 8);
    assert.ok(Math.abs(result.signal[0].hz - 440) < 35, JSON.stringify(result));
    assert.ok(Math.abs(result.signal[1].hz - 880) < 35, JSON.stringify(result));
    if (codec === 'opus') assert.ok(bitrate < 160000, `Opus bitrate ${bitrate}`);
    else assert.ok(bitrate < 1600000, `PCM bitrate ${bitrate}`);
    await page.click('#test-audio');await page.evaluate(() => window.soundRequest);
    const muted = await page.evaluate(() => window.gliss.audio.snapshot());
    assert.equal(muted.contextState, 'closed');assert.equal(muted.sources, 0);assert.equal(muted.enabled, false);
    assert.deepEqual(errors, []);
    t.diagnostic(JSON.stringify({codec, bitrate: Math.round(bitrate), ...result}));
    await page.close();
  }
});
