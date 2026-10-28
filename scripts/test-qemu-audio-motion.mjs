// Owned framebuffer/audio readiness; no production guest or service is used.
import test from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {performance} from 'node:perf_hooks';
import {startAudioMotionVm} from '../test-support/qemu-audio-motion/fixture.mjs';
import {startMotionServer} from '../test-support/qemu-audio-motion/server.mjs';
import {installMarkerObserver} from '../test-support/qemu-audio-marker/browser.mjs';
import {rawStamp} from '../test-support/qemu-audio-motion/raw-stamp.mjs';
import {freePort, launchBrowser, loginPage} from '../tests/helpers.mjs';

test('owned guest: procedural motion and exact audio markers reach the browser together', {timeout: 60000}, async t => {
  const vm = await startAudioMotionVm(t, {cpu: 19});
  const raw = await rawStamp(vm.vnc);assert.equal(raw.width,1280);assert.equal(raw.height,800);
  t.diagnostic(JSON.stringify({raw_source:raw}));
  const server = await startMotionServer({port: await freePort(), vnc: vm.vnc,
    codec: 'h264', fps: 30, h264Bitrate: 2000000, inFlightFrames: 8, tcpNodelay: true});
  t.after(server.stop);
  const browser = await launchBrowser();t.after(() => browser.close());
  const page = await browser.newPage(), errors = [], messages = [];
  page.on('pageerror', error => errors.push(error.name));
  const stampSource = await readFile(new URL('../test-support/qemu-audio-motion/stamp.js', import.meta.url), 'utf8');
  await page.route('**/gliss-motion-probe/stamp.js', route => route.fulfill({contentType: 'text/javascript', body: stampSource}));
  await loginPage(page, server);
  await page.waitForFunction(() => window.gliss.audio.available);
  await page.evaluate(async () => {
    window.readMotionStamp = (await import('/gliss-motion-probe/stamp.js')).readMotionStamp;
    const button = document.createElement('button');button.id = 'motion-sound';button.textContent = 'Sound';
    button.onclick = () => {window.motionSound = window.gliss.setAudioEnabled(true);};document.body.append(button);
  });
  t.diagnostic(JSON.stringify({first_paint: await page.evaluate(() => {
    const context = window.gliss.ctx;
    return {width:context.canvas.width,height:context.canvas.height,colors:[18,37,277,370].map(x =>
      ({x,rgba:[...context.getImageData(x,18,1,1).data]}))};
  })}));
  if (process.env.GLISS_MOTION_SCREENSHOT)
    await page.locator('canvas').screenshot({path:process.env.GLISS_MOTION_SCREENSHOT});
  t.diagnostic(JSON.stringify({guest_startup:vm.log(),source_frames:vm.frames}));
  const currentWait = performance.now();
  await page.waitForFunction(() => window.readMotionStamp(window.gliss.ctx) === 0, null, {timeout:5000});
  t.diagnostic(JSON.stringify({current_source_ready_ms:performance.now()-currentWait}));
  assert.equal(await page.evaluate(() => window.readMotionStamp(window.gliss.ctx)), 0);
  await page.click('#motion-sound');await page.evaluate(() => window.motionSound);
  assert.equal(await page.evaluate(() => window.gliss.audio.codec), 'opus');
  await page.waitForFunction(() => window.gliss.audio.scheduled >= 5);
  await installMarkerObserver(page, value => messages.push(value));
  const before = await page.evaluate(() => ({frame: window.readMotionStamp(window.gliss.ctx), paints: window.gliss.metrics.frames}));
  await vm.setMotion(true);
  await new Promise(resolve => setTimeout(resolve, 2000));
  const active = await page.evaluate(() => ({frame: window.readMotionStamp(window.gliss.ctx), paints: window.gliss.metrics.frames}));
  assert.ok(active.frame !== null && active.frame - before.frame >= 20, 'Current painted source counter must advance');
  assert.ok(active.paints - before.paints >= 20, 'At least twenty motion paints must arrive in two seconds');
  const pulses = [];
  let token = 0;
  for (const code of [1,10,20,30,40,48]) {
    assert.ok(!messages.some(value => value.code === code));
    const submitted = performance.now();await vm.pulse(++token, code);
    const due = submitted + 1500;
    while (!messages.some(value => value.code === code) && performance.now() < due)
      await new Promise(resolve => setTimeout(resolve, 5));
    const observation = messages.find(value => value.code === code);
    assert.ok(observation && observation.host_received_ms < due, 'Exact moving-desktop audio marker must arrive');
    assert.equal(vm.notes.get(token)?.code, code);
    pulses.push({token,code,submitted_host_ms:submitted,source_note:vm.notes.get(token),observation,
      trigger_to_observed_ms:observation.host_received_ms-submitted});
    await new Promise(resolve => setTimeout(resolve, 200));
  }
  await vm.setMotion(false);await new Promise(resolve => setTimeout(resolve, 500));
  const frozen = await page.evaluate(() => window.readMotionStamp(window.gliss.ctx));
  await new Promise(resolve => setTimeout(resolve, 1000));
  assert.equal(await page.evaluate(() => window.readMotionStamp(window.gliss.ctx)), frozen);
  assert.deepEqual(errors, []);
  const state = await page.evaluate(() => ({audio: window.gliss.audio.snapshot(),video_errors:window.gliss.metrics.errors}));
  assert.equal(state.video_errors, 0);
  t.diagnostic(JSON.stringify({before,active,frozen,source_frames:vm.frames.length,pulses,state}));
  await page.evaluate(() => {window.glissMarkerProbe.stop();window.gliss.dispose();});await page.close();
});
