// Opt-in owned-guest marker latency. All eight positions and censored probes survive failures.
import test from 'node:test';
import assert from 'node:assert/strict';
import {readFile, writeFile, lstat, readlink} from 'node:fs/promises';
import {existsSync} from 'node:fs';
import {getPriority} from 'node:os';
import {createHash} from 'node:crypto';
import {isAbsolute} from 'node:path';
import {performance} from 'node:perf_hooks';
import {freePort, serverBinary, launchBrowser, loginPage} from '../tests/helpers.mjs';
import {startPolicyServer, validateSocketPolicy} from '../test-support/browser/audio-policy-server.mjs';
import {startKernelLink} from '../test-support/browser/audio-kernel-link.mjs';
import {startAudioMarkerVm} from '../test-support/qemu-audio-marker/fixture.mjs';
import {installMarkerObserver} from '../test-support/qemu-audio-marker/browser.mjs';
import {finishMarker} from '../test-support/qemu-audio-marker/receipt.mjs';

assert.equal(process.argv.length, 4);assert.equal(process.argv[2], '--output');
const output = process.argv[3];
try {await lstat(output);throw new Error('Output exists');} catch (error) {if (error.code !== 'ENOENT') throw error;}
assert.ok(process.env.GLISS_BINARY && isAbsolute(serverBinary()));assert.equal(getPriority(), 19);
assert.equal((await readFile('/proc/self/status', 'utf8')).match(/^Cpus_allowed_list:\s*(.+)$/m)?.[1], '0');
assert.ok(!['/dev/dri', '/dev/nvidiactl', '/dev/kfd'].some(existsSync));
assert.match(process.env.GLISS_AUDIO_HOST_NETNS ?? '', /^net:\[[0-9]+\]$/);
assert.notEqual(await readlink('/proc/self/ns/net'), process.env.GLISS_AUDIO_HOST_NETNS);
const planRaw = await readFile(new URL('../benchmarks/audio-marker/latency-plan.json', import.meta.url));
const plan = JSON.parse(planRaw), hash = raw => createHash('sha256').update(raw).digest('hex');
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
async function until(deadline) {while (performance.now() < deadline) await sleep(Math.min(50, deadline - performance.now()));}

test('guest marker latency: eight fixed Opus/PCM positions', {timeout: 150000}, async t => {
  const result = {schema: 'gliss-audio-marker-latency-results-v1', plan_sha256: hash(planRaw),
    binary_sha256: hash(await readFile(serverBinary())), source_pins: {},
    qualification: plan.qualification, physical_speaker_latency_ms: null, cleanup_complete: false, positions: []};
  for (const source of ['scripts/bench-audio-latency.mjs', 'test-support/qemu-audio-marker/fixture.mjs',
    'test-support/qemu-audio-marker/init.c', 'test-support/qemu-audio-marker/build.py',
    'test-support/qemu-audio-marker/browser.mjs', 'test-support/qemu-audio-marker/detector.js',
    'test-support/qemu-audio-marker/receipt.mjs', 'test-support/browser/audio-kernel-link.mjs',
    'test-support/browser/audio-policy-server.mjs', 'tests/helpers.mjs'])
    result.source_pins[source] = hash(await readFile(new URL('../' + source, import.meta.url)));
  for (const profile of plan.profiles) for (const mode of plan.order) {
    const slot = result.positions.length + 1;
    result.positions.push({slot, profile: profile.name, mode, status: 'blocked', pulses: plan.pulse_offsets_ms.map((offset, i) =>
      ({token: (slot - 1) * 6 + i + 1, code: (slot - 1) * 6 + i + 1, planned_offset_ms: offset,
        submitted_host_ms: null, status: 'not-submitted'}))});
  }
  const cleanup = [];let vm, browser;
  try {
    vm = await startAudioMarkerVm({after: close => cleanup.push(close)}, {cpu: plan.guest_cpu, mixFrequency: plan.source_mix_frequency});
    result.audio_device = vm.audioDevice;
    browser = await launchBrowser({handleSIGINT: false, handleSIGTERM: false});
    for (const position of result.positions) {
      let link, server, page, active = true, overflow = false;
      const observations = [], errors = [];
      try {
        const profile = plan.profiles.find(row => row.name === position.profile), port = await freePort();
        if (profile.mbps !== null) link = await startKernelLink({targetPort: port, rttMs: profile.rttMs, mbps: profile.mbps});
        server = await startPolicyServer({tcpNodelay: true, port, vnc: vm.vnc, codec: 'h264', fps: 30,
          h264Bitrate: 2000000, inFlightFrames: 4, ...(link ? {origin: link.url} : {})});
        assert.equal(getPriority(server.child.pid), 19);assert.equal(getPriority(vm.child.pid), 19);
        const allowed = async pid => (await readFile(`/proc/${pid}/status`, 'utf8')).match(/^Cpus_allowed_list:\s*(.+)$/m)?.[1];
        position.cpu_admission = {native: await allowed(server.child.pid), guest: await allowed(vm.child.pid)};
        assert.equal(position.cpu_admission.native, '0');assert.equal(position.cpu_admission.guest, '19');
        page = await browser.newPage();page.on('pageerror', error => errors.push(error.name));
        await page.addInitScript(() => {
          window.markerSocketPolicy = null;
          window.WebSocket = new Proxy(WebSocket, {construct(Target, args) {
            const socket = new Target(...args);
            socket.addEventListener('message', event => {
              if (typeof event.data !== 'string') return;
              const value = JSON.parse(event.data);if (value.type === 'hello') window.markerSocketPolicy = value.tcp_listener;
            });return socket;
          }});
        });
        if (position.mode === 'pcm') await page.addInitScript(() => {window.AudioDecoder = undefined;});
        await loginPage(page, server);
        position.socket_policy = validateSocketPolicy(await page.evaluate(() => window.markerSocketPolicy), true);
        await page.waitForFunction(() => window.gliss.audio.available);
        // PCM is an explicit comparison arm, even when the peer offers ADPCM.
        if (position.mode === 'pcm') await page.evaluate(() => {window.gliss.audio.codecs = ['pcm'];});
        await page.evaluate(() => {
          const button = document.createElement('button');button.id = 'marker-sound';button.textContent = 'Sound';
          button.onclick = () => {window.markerSound = window.gliss.setAudioEnabled(true);};document.body.append(button);
        });
        await page.click('#marker-sound');await page.evaluate(() => window.markerSound);
        assert.equal(await page.evaluate(() => window.gliss.audio.codec), position.mode);
        await page.waitForFunction(() => window.gliss.audio.scheduled >= 5);
        position.observer = await installMarkerObserver(page, message => {
          if (!active) return;
          if (observations.length < plan.observation_cap_per_position) observations.push(message);else overflow = true;
        });
        await sleep(plan.warmup_ms);
        const beforeClock = performance.now(), browserClock = await page.evaluate(() => performance.now()), afterClock = performance.now();
        position.clock = {host_before_ms: beforeClock, browser_ms: browserClock, host_after_ms: afterClock,
          offset_min_ms: beforeClock - browserClock, offset_max_ms: afterClock - browserClock};
        position.browser_window = await page.evaluate(plan => {
          const start = performance.now(), end = start + plan.window_ms, client = window.gliss;
          const state = window.markerStudy = {start_ms: start, end_ms: end, wire_bytes: 0, wire_frames: 0,
            audio_before: client.audio.snapshot(), video_before: {...client.metrics}, samples: []};
          const receive = event => {
            if (performance.now() >= end || !(event.data instanceof ArrayBuffer) || event.data.byteLength < 20) return;
            const view = new DataView(event.data);if (view.getUint32(0, true) !== 0x31414c47) return;
            state.wire_bytes += event.data.byteLength;state.wire_frames += view.getUint32(16, true);
          };
          client.socket.addEventListener('message', receive);
          const timer = setInterval(() => {if (performance.now() < end)
            state.samples.push({...client.audio.snapshot(), at_ms: performance.now() - start});}, plan.sample_interval_ms);
          state.finish = () => {
            clearInterval(timer);client.socket.removeEventListener('message', receive);
            return {start_ms: start, end_ms: end, wire_bytes: state.wire_bytes, wire_frames: state.wire_frames,
              samples: state.samples, audio_before: state.audio_before, audio_after: client.audio.snapshot(),
              video_errors_delta: client.metrics.errors - state.video_before.errors,
              observer_failure: window.glissMarkerProbe.failure,
              max_analysis_gap_ms: window.glissMarkerProbe.max_poll_gap_ms,
              quality_windows_dropped: window.glissMarkerProbe.quality_windows_dropped};
          };
          return {start_ms: start, end_ms: end};
        }, plan);
        position.host_window_start_ms = performance.now();
        for (const pulse of position.pulses) {
          const due = position.host_window_start_ms + pulse.planned_offset_ms;await until(due);
          pulse.submitted_host_ms = performance.now();pulse.submission_lateness_ms = pulse.submitted_host_ms - due;
          pulse.status = 'submitted';await vm.pulse(pulse.token, pulse.code);pulse.serial_write_done_host_ms = performance.now();
        }
        await until(position.host_window_start_ms + plan.window_ms);
        active = false;position.observations = observations;
        position.metrics = await page.evaluate(() => window.markerStudy.finish());
        position.pulses = position.pulses.map(pulse => finishMarker(pulse, observations, vm.notes.get(pulse.token),
          position.clock, position.browser_window, plan.pulse_deadline_ms));
        position.measurement_valid = !overflow && errors.length === 0 && position.metrics.observer_failure === null &&
          position.metrics.video_errors_delta === 0 && position.pulses.every(pulse => pulse.source_valid &&
            pulse.submission_lateness_ms <= plan.max_submission_lateness_ms &&
            !['invalid-observation', 'preexisting-or-ambiguous'].includes(pulse.status));
        position.timely_responses = position.pulses.filter(pulse => pulse.status === 'observed').length;
        position.complete_timely_set = position.timely_responses === plan.markers_per_position;
        if (link) {
          position.kernel_link = await link.snapshot();
          assert.ok(position.kernel_link.queues.every(row => row.packets > 0 && row.drops === 0));
        }
        position.status = 'measured';
        t.diagnostic(JSON.stringify({slot: position.slot, profile: position.profile, mode: position.mode,
          valid: position.measurement_valid, timely: position.timely_responses,
          outcomes: position.pulses.map(pulse => [pulse.code, pulse.status, pulse.trigger_to_observed_ms])}));
      } catch (error) {
        position.status = 'failed';position.failure = error.name;position.observations = observations;
        throw error;
      } finally {
        active = false;
        if (page) {await page.evaluate(() => {window.glissMarkerProbe?.stop();window.gliss?.dispose();}).catch(() => {});await page.close();}
        if (link) {await link.stop();position.kernel_cleanup = link.cleanup;}
        if (server) {await server.stop();position.native_exit = {code: server.child.exitCode, signal: server.child.signalCode};}
      }
    }
  } finally {
    await browser?.close();for (const close of cleanup.reverse()) await close();
    result.cleanup_complete = !vm || vm.child.exitCode !== null || vm.child.signalCode !== null;
    const raw = JSON.stringify(result) + '\n';assert.ok(Buffer.byteLength(raw) <= 1048576);
    await writeFile(output, raw, {flag: 'wx', mode: 0o600});assert.equal(result.cleanup_complete, true);
  }
});
