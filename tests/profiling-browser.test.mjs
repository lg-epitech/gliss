import test from 'node:test';
import assert from 'node:assert/strict';
import {startServer, launchBrowser, loginPage} from './helpers.mjs';

async function desktop(t, options = {}) {
  const server = await startServer({codec: 'h264', workload: 'drag', fps: 20, ...options});t.after(server.stop);
  const browser = await launchBrowser();t.after(() => browser.close());
  const page = await browser.newPage();t.after(() => page.close());
  const errors = [], imports = [];
  page.on('pageerror', error => errors.push(error.message));
  page.on('request', request => {if (new URL(request.url()).pathname === '/profiling.js') imports.push(true);});
  await loginPage(page, server);
  await page.waitForFunction(() => window.gliss.metrics.frames >= 4);
  t.after(() => assert.deepEqual(errors, []));
  return {page, imports, server};
}

test('profiling is dormant by default; explicit recording captures actual browser stages and native CPU', {timeout: 20000}, async t => {
  const {page, imports, server} = await desktop(t, {streamStats: true});
  assert.equal(imports.length, 0);
  await page.evaluate(() => {
    window.profilePings = 0;
    const socket = window.gliss.socket, send = socket.send.bind(socket);
    socket.send = text => {if (JSON.parse(text).type === 'ping') window.profilePings++;return send(text);};
  });
  await page.waitForFunction(() => window.gliss.metrics.frames >= 8);
  assert.equal(await page.evaluate(() => window.profilePings), 0);
  await page.evaluate(async () => {window.profile = await window.gliss.startProfiling({durationMs: 4000, probeIntervalMs: 1000});});
  assert.equal(imports.length, 1);
  assert.equal(await page.evaluate(async () => {
    try {await window.gliss.startProfiling();return false;} catch {return true;}
  }), true);
  await page.waitForFunction(() => window.profile.snapshot().state === 'stopped');
  const result = await page.evaluate(() => ({report: window.profile.snapshot(), errors: window.gliss.metrics.errors, frames: window.gliss.metrics.frames}));
  const report = result.report;
  assert.equal(report.stop_reason, 'deadline');assert.equal(report.duration_ms, 4000);
  assert.ok(report.frames.length >= 5);assert.ok(report.server.length >= 2);
  assert.equal(report.capabilities.native_process_cpu, true);assert.equal(report.capabilities.native_encode_thread_cpu, true);
  assert.ok(report.summary.process_cpu_percent_one_core >= 0);
  assert.ok(report.summary.native.encode_cpu_ms.count > 0);
  assert.ok(report.summary.probes_sent >= 2);assert.ok(report.summary.probes_received >= 1);
  assert.equal(result.errors, 0);
  for (const row of report.frames) {
    const sum = row.queue_ms + row.decode_ms + row.raf_wait_ms + row.canvas_submit_ms;
    assert.ok(Math.abs(sum - row.receive_to_canvas_ms) <= 0.005);
  }
  assert.ok(!JSON.stringify(report).includes(server.url));assert.ok(!JSON.stringify(report).includes(server.token));
  await page.waitForFunction(before => window.gliss.metrics.frames > before, result.frames);
});

test('profiling captures a real serial-queue stall while control probes bypass the stalled decode', {timeout: 15000}, async t => {
  const {page} = await desktop(t, {inFlightFrames: 4});
  await page.evaluate(async () => {
    window.profile = await window.gliss.startProfiling({durationMs: 10000, probeIntervalMs: 1000});
    const decoder = window.gliss.decoder, decode = decoder.decode.bind(decoder);
    let once = true;
    decoder.decode = async bytes => {
      const frame = await decode(bytes);
      if (once) {once = false;await new Promise(resolve => {window.releaseDecode = resolve;});}
      return frame;
    };
  });
  await page.waitForFunction(() => window.releaseDecode && window.profile.snapshot().summary.probes_received >= 1);
  await page.evaluate(() => window.releaseDecode());
  await page.waitForFunction(() => window.profile.snapshot().frames.length >= 3);
  const report = await page.evaluate(() => window.profile.stop());
  assert.ok(report.summary.decode_ms.max >= 100);
  assert.ok(report.summary.queue_ms.max >= 100);
  assert.equal(report.capabilities.native_stats, false);
  assert.equal(report.summary.process_cpu_percent_one_core, null);
  assert.equal(await page.evaluate(() => window.gliss.metrics.errors), 0);
});

test('profiling cancels a pending start on disconnect and finalizes an active session on disposal', {timeout: 15000}, async t => {
  const {page} = await desktop(t);
  const explicitStop = await page.evaluate(async () => {
    const client = window.gliss, pending = client.startProfiling({probeIntervalMs: 1000});
    client.stopProfiling();
    let cancelled = false;try {await pending;} catch {cancelled = true;}
    return {cancelled, active: client.profiler !== null, pending: client.profilingStart !== null};
  });
  assert.deepEqual(explicitStop, {cancelled: true, active: false, pending: false});
  const result = await page.evaluate(async () => {
    const client = window.gliss;
    const pending = client.startProfiling();client.disconnect();
    let cancelled = false;try {await pending;} catch {cancelled = true;}
    await client.connect();return {cancelled, active: client.profiler !== null};
  });
  assert.deepEqual(result, {cancelled: true, active: false});
  await page.waitForFunction(() => window.gliss.socket?.readyState === 1 && window.gliss.codec === 'h264');
  const stopped = await page.evaluate(async () => {
    const profile = await window.gliss.startProfiling();window.gliss.dispose();
    const report = profile.snapshot();return {reason: report.stop_reason, state: report.state, same: profile.stop() === report, frozen: Object.isFrozen(report)};
  });
  assert.deepEqual(stopped, {reason: 'disconnect', state: 'stopped', same: true, frozen: true});
});
