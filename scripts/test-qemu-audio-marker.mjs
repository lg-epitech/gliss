// Opt-in source/output component proof. Owned guest; no existing VM or services.
import test from 'node:test';
import assert from 'node:assert/strict';
import {performance} from 'node:perf_hooks';
import {startAudioMarkerVm} from '../test-support/qemu-audio-marker/fixture.mjs';
import {installMarkerObserver} from '../test-support/qemu-audio-marker/browser.mjs';
import {startServer, launchBrowser, loginPage} from '../tests/helpers.mjs';

test('real guest serial markers reach actual stereo Web Audio output in both codecs', {timeout: 60000}, async t => {
  const mixFrequency = process.env.GLISS_MARKER_MIX_FREQUENCY === undefined ? undefined :
    Number(process.env.GLISS_MARKER_MIX_FREQUENCY);
  const vm = await startAudioMarkerVm(t, {cpu: 19, mixFrequency});
  const server = await startServer({vnc: vm.vnc, codec: 'h264', fps: 30, h264Bitrate: 2000000, inFlightFrames: 4});
  t.after(server.stop);
  const browser = await launchBrowser();t.after(() => browser.close());
  let token = 0;
  for (const codec of ['opus', 'pcm']) {
    const page = await browser.newPage(), messages = [], errors = [];
    page.on('pageerror', error => errors.push(error.name));
    if (codec === 'pcm') await page.addInitScript(() => {window.AudioDecoder = undefined;});
    await loginPage(page, server);
    await page.waitForFunction(() => window.gliss.audio.available);
    await page.evaluate(() => {
      const button = document.createElement('button');button.id = 'marker-sound';button.textContent = 'Sound';
      button.onclick = () => {window.markerSound = window.gliss.setAudioEnabled(true);};document.body.append(button);
    });
    await page.click('#marker-sound');await page.evaluate(() => window.markerSound);
    assert.equal(await page.evaluate(() => window.gliss.audio.codec), codec);
    await page.waitForFunction(() => window.gliss.audio.scheduled >= 5);
    const observer = await installMarkerObserver(page, message => messages.push(message));
    await new Promise(resolve => setTimeout(resolve, 300));
    assert.equal(messages.length, 0, 'The baseline must not generate a marker');
    const pulses = [];
    for (const code of [1, 10, 20, 30, 40, 48]) {
      assert.ok(!messages.some(message => message.code === code), 'Reject a preexisting expected code');
      const submitted = performance.now(), pulseToken = ++token;
      await vm.pulse(pulseToken, code);
      const due = submitted + 1500;
      while (!messages.some(message => message.code === code) && performance.now() < due)
        await new Promise(resolve => setTimeout(resolve, 5));
      const observed = messages.find(message => message.code === code);
      assert.ok(observed && observed.host_received_ms < due, `Codec ${codec} did not render exact marker ${code}`);
      const note = vm.notes.get(pulseToken);
      assert.equal(note?.code, code, 'The guest must confirm the exact submitted token/code');
      pulses.push({token: pulseToken, code, submitted_host_ms: submitted, source_note: note,
        observation: observed, trigger_to_observed_ms: observed.host_received_ms - submitted});
      await new Promise(resolve => setTimeout(resolve, 200));
    }
    const state = await page.evaluate(() => ({probe_failure: window.glissMarkerProbe.failure,
      audio: window.gliss.audio.snapshot(), video_errors: window.gliss.metrics.errors,
      quality_windows: window.glissMarkerProbe.quality_windows,
      quality_windows_dropped: window.glissMarkerProbe.quality_windows_dropped,
      max_poll_gap_ms: window.glissMarkerProbe.max_poll_gap_ms}));
    assert.equal(state.probe_failure, null);assert.equal(state.video_errors, 0);assert.deepEqual(errors, []);
    t.diagnostic(JSON.stringify({codec, mix_frequency: mixFrequency ?? null, audio_device: vm.audioDevice, observer, pulses, state}));
    await page.evaluate(() => {window.glissMarkerProbe.stop();window.gliss.dispose();});await page.close();
  }
});
