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
import {startIsolationRelay} from '../test-support/audio-isolation/relay.mjs';
import {startWindowServer} from '../test-support/qemu-audio-motion/server-window.mjs';
import {validateSocketPolicy} from '../test-support/browser/audio-policy-server.mjs';
import {installMotionObserver, startMotionWindow} from '../test-support/qemu-audio-motion/observer-skip.mjs';
import {rawStamp} from '../test-support/qemu-audio-motion/raw-stamp.mjs';
import {processCounter, processCpu, browserCounter, browserCpu} from '../test-support/qemu-audio-motion/cpu.mjs';
import {startKernelLink} from '../test-support/browser/audio-kernel-link.mjs';
import {startAudioMotionVm} from '../test-support/qemu-audio-motion/fixture-budget.mjs';
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
const planRaw = await readFile(new URL('../benchmarks/audio-isolation/plan.json', import.meta.url));
const socketSource = await readFile(new URL('../test-support/audio-isolation/socket.js',import.meta.url),'utf8');
const plan = JSON.parse(planRaw), hash = raw => createHash('sha256').update(raw).digest('hex');
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
async function until(deadline) {while (performance.now() < deadline) await sleep(Math.min(50, deadline - performance.now()));}

test('guest marker latency: eight fixed same-budget audio-isolation prototype positions', {timeout: 150000}, async t => {
  const result = {schema: 'gliss-audio-isolation-results-v1', plan_sha256: hash(planRaw),
    binary_sha256: hash(await readFile(serverBinary())), source_pins: {},
    qualification: plan.qualification, physical_speaker_latency_ms: null, cleanup_complete: false, positions: []};
  for (const source of ['scripts/bench-audio-isolation.mjs','test-support/audio-isolation/relay.mjs','test-support/audio-isolation/socket.js', 'test-support/qemu-audio-motion/fixture-budget.mjs',
    'test-support/qemu-audio-motion/init.c','test-support/qemu-audio-motion/build.py',
    'test-support/qemu-audio-motion/server-window.mjs','test-support/qemu-audio-motion/observer-skip.mjs',
    'test-support/qemu-audio-motion/stamp.js','test-support/qemu-audio-motion/raw-stamp.mjs',
    'test-support/qemu-audio-motion/cpu.mjs',
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
    vm = await startAudioMotionVm({after: close => cleanup.push(close)}, {cpu: plan.guest_cpu, mixFrequency: plan.source_mix_frequency});
    result.audio_device = vm.audioDevice;result.guest_build = vm.buildIdentity;
    result.raw_source = await rawStamp(vm.vnc);assert.equal(result.raw_source.width,1280);assert.equal(result.raw_source.height,800);
    browser = await launchBrowser({handleSIGINT: false, handleSIGTERM: false});
    for (const position of result.positions) {
      let link, server, relay, page, active = true, overflow = false;
      const observations = [], errors = [];
      try {
        const profile = plan.profiles.find(row => row.name === position.profile), port = await freePort(), relayPort = await freePort();
        assert.notEqual(port,relayPort);
        relay = await startIsolationRelay({port:relayPort,targetPort:port,mode:position.mode});
        await vm.setMotion(false);await sleep(200);const frozenFrame = vm.frames.at(-1).frame;
        if (profile.mbps !== null) link = await startKernelLink({targetPort: relayPort, rttMs: profile.rttMs, mbps: profile.mbps});
        server = await startWindowServer({tcpNodelay: true, port, vnc: vm.vnc, codec: 'h264', fps: 30,
          h264Bitrate: plan.video_bitrate_target, inFlightFrames: plan.video_in_flight_frames, frameSkip: false, origin:relay.url});
        assert.equal(getPriority(server.child.pid), 19);assert.equal(getPriority(vm.child.pid), 19);
        const allowed = async pid => (await readFile(`/proc/${pid}/status`, 'utf8')).match(/^Cpus_allowed_list:\s*(.+)$/m)?.[1];
        position.cpu_admission = {native: await allowed(server.child.pid), guest: await allowed(vm.child.pid)};
        assert.equal(position.cpu_admission.native, '0');assert.equal(position.cpu_admission.guest, '19');
        page = await browser.newPage();page.on('pageerror', error => errors.push(error.name));
        const probeInit = () => {
          window.markerSocketPolicy = null;window.markerFrameSkip = null;
          window.WebSocket = new Proxy(WebSocket, {construct(Target, args) {
            const socket = new Target(...args);
            socket.addEventListener('message', event => {
              window.motionWireTap?.(event);
              if (typeof event.data !== 'string') return;
              const value = JSON.parse(event.data);if (value.type === 'hello') {window.markerSocketPolicy = value.tcp_listener;window.markerFrameSkip = value.video_frame_skip ?? false;}
            });return socket;
          }});
        };
        await page.addInitScript({content:`window.glissIsolationMode=${JSON.stringify(position.mode)};\n${socketSource}\n(${probeInit.toString()})();`});
        await loginPage(page, {...server,url:relay.url});
        position.socket_policy = validateSocketPolicy(await page.evaluate(() => window.markerSocketPolicy), true);
        position.frame_skip = await page.evaluate(() => window.markerFrameSkip);
        assert.equal(position.frame_skip, false);
        position.in_flight_frames = plan.video_in_flight_frames;
        position.native_argv = (await readFile(`/proc/${server.child.pid}/cmdline`,'utf8')).split('\0').filter(Boolean).slice(1);
        const argumentIndex = position.native_argv.indexOf('--in-flight-frames');
        assert.ok(argumentIndex >= 0);assert.equal(position.native_argv[argumentIndex+1],String(position.in_flight_frames));
        await page.waitForFunction(() => window.gliss.audio.available);
        await installMotionObserver(page);
        await page.waitForFunction(expected => window.readMotionStamp(window.gliss.ctx) === expected,frozenFrame,{timeout:5000});
        await page.evaluate(() => {
          const button = document.createElement('button');button.id = 'marker-sound';button.textContent = 'Sound';
          button.onclick = () => {window.markerSound = window.gliss.setAudioEnabled(true);};document.body.append(button);
        });
        await page.click('#marker-sound');await page.evaluate(() => window.markerSound);
        assert.equal(await page.evaluate(() => window.gliss.audio.codec), plan.audio_codec);
        await page.waitForFunction(() => window.gliss.audio.scheduled >= 5);
        position.observer = await installMarkerObserver(page, message => {
          if (!active) return;
          if (observations.length < plan.observation_cap_per_position) observations.push(message);else overflow = true;
        });
        position.h264_bitrate = plan.video_bitrate_target;
        await vm.setMotion(true);
        await sleep(plan.warmup_ms);
        const cdp = await browser.newBrowserCDPSession();
        position.relay_before = relay.snapshot();
        const beforeCpu = {controller:await processCounter(process.pid),native:await processCounter(server.child.pid),guest:await processCounter(vm.child.pid),browser:await browserCounter(cdp)};
        const beforeClock = performance.now(), browserClock = await page.evaluate(() => performance.now()), afterClock = performance.now();
        position.clock = {host_before_ms: beforeClock, browser_ms: browserClock, host_after_ms: afterClock,
          offset_min_ms: beforeClock - browserClock, offset_max_ms: afterClock - browserClock};
        position.browser_window = await startMotionWindow(page,{...plan,current_mode:position.mode});
        position.host_window_start_ms = performance.now();
        for (const pulse of position.pulses) {
          const due = position.host_window_start_ms + pulse.planned_offset_ms;await until(due);
          pulse.submitted_host_ms = performance.now();pulse.submission_lateness_ms = pulse.submitted_host_ms - due;
          pulse.status = 'submitted';await vm.pulse(pulse.token, pulse.code);pulse.serial_write_done_host_ms = performance.now();
        }
        await until(position.host_window_start_ms + plan.window_ms);
        active = false;position.observations = observations;
        position.metrics = await page.evaluate(() => window.motionStudy.finish());
        position.relay_after = relay.snapshot();
        const identities = position.relay_after.identities;
        assert.equal(position.relay_after.failure,null);assert.equal(position.relay_after.browser_sockets,2);
        assert.equal(position.relay_after.native_sessions,1);assert.equal(identities.length,2);
        assert.notEqual(identities[0].remote_port,identities[1].remote_port);
        assert.ok(identities.every(row => row.local_port === relayPort && row.loopback));
        assert.equal(position.relay_after.counters.overflows,0);assert.equal(position.relay_after.counters.protocol_errors,0);
        const relayCounters=position.relay_after.counters;
        assert.ok(relayCounters.upstream_audio > 0);assert.equal(relayCounters.main_video,relayCounters.upstream_video);
        assert.equal(relayCounters.main_audio,position.mode==='combined'?relayCounters.upstream_audio:0);
        assert.equal(relayCounters.companion_audio,position.mode==='split'?relayCounters.upstream_audio:0);
        const afterCpu = {controller:await processCounter(process.pid),native:await processCounter(server.child.pid),guest:await processCounter(vm.child.pid),browser:await browserCounter(cdp)};
        position.cpu = {controller:processCpu(beforeCpu.controller,afterCpu.controller),native:processCpu(beforeCpu.native,afterCpu.native),guest:processCpu(beforeCpu.guest,afterCpu.guest),browser:browserCpu(beforeCpu.browser,afterCpu.browser)};
        await cdp.detach();
        position.source_frames = vm.frames.filter(row => row.host_received_ms >= position.host_window_start_ms && row.host_received_ms < position.host_window_start_ms + plan.window_ms);
        position.pulses = position.pulses.map(pulse => finishMarker(pulse, observations, vm.notes.get(pulse.token),
          position.clock, position.browser_window, plan.pulse_deadline_ms));
        position.measurement_valid = !overflow && !position.metrics.overflow && errors.length === 0 && position.metrics.observer_failure === null &&
          position.metrics.video_errors_delta === 0 && position.metrics.video_marker_eligible &&
          position.metrics.sequence_gaps === 0 && position.metrics.invalid_skip_stats === 0 &&
          (!position.frame_skip || position.metrics.skip_samples.length > 0) && position.pulses.every(pulse => pulse.source_valid &&
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
          native_skips:position.metrics.skip_samples.at(-1)?.count ?? null,sequence_gaps:position.metrics.sequence_gaps,
          paints:position.metrics.paints.length,unique_source_frames:position.metrics.unique_source_frames,
          native_cpu:position.cpu.native.percent_of_one_core,browser_cpu:position.cpu.browser.percent_of_one_core,
          outcomes: position.pulses.map(pulse => [pulse.code, pulse.status, pulse.trigger_to_observed_ms])}));
      } catch (error) {
        position.status = 'failed';position.failure = error.name;position.relay_failure = relay?.snapshot() ?? null;position.observations = observations;
        throw error;
      } finally {
        active = false;
        if (page) {await page.evaluate(() => {window.glissMarkerProbe?.stop();window.gliss?.dispose();}).catch(() => {});await page.close();}
        if (relay) {await relay.stop();position.relay_cleanup = true;}
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
