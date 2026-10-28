// Opt-in owned-guest marker latency. All eight positions and censored probes survive failures.
import test from 'node:test';
import assert from 'node:assert/strict';
import {readFile, lstat, readlink, open, rename} from 'node:fs/promises';
import {existsSync} from 'node:fs';
import {getPriority} from 'node:os';
import {createHash} from 'node:crypto';
import {isAbsolute} from 'node:path';
import {performance} from 'node:perf_hooks';
import {freePort, serverBinary, launchBrowser, loginPage} from '../tests/helpers.mjs';
import {startDurationServer} from '../test-support/audio-packet-duration-study/server.mjs';
import {assessAudioWindow} from '../test-support/audio-packet-duration-study/assessment.mjs';
import {validateSocketPolicy} from '../test-support/browser/audio-policy-server.mjs';
import {installMotionObserver, startMotionWindow} from '../test-support/audio-packet-duration-study/observer.mjs';
import {rawStamp} from '../test-support/qemu-audio-motion/raw-stamp.mjs';
import {processCounter, processCpu, browserCounter, browserCpu} from '../test-support/qemu-audio-motion/cpu.mjs';
import {startKernelLink} from '../test-support/browser/audio-kernel-link.mjs';
import {startAudioMotionVm} from '../test-support/qemu-audio-motion/fixture-budget.mjs';
import {installMarkerObserver} from '../test-support/qemu-audio-marker/browser.mjs';
import {finishMarker} from '../test-support/qemu-audio-marker/receipt.mjs';

assert.equal(process.argv.length, 6);assert.equal(process.argv[2], '--plan');assert.equal(process.argv[4], '--output');
const planName=process.argv[3];assert.match(planName,/^benchmarks\/[a-z0-9-]+\/plan\.json$/);
const output=process.argv[5];assert.ok(isAbsolute(output));
try {await lstat(output+'.next');throw new Error('Checkpoint exists');} catch(error){if(error.code!=='ENOENT')throw error;}
try {await lstat(output);throw new Error('Output exists');} catch (error) {if (error.code !== 'ENOENT') throw error;}
assert.ok(process.env.GLISS_BINARY && isAbsolute(serverBinary()));assert.equal(getPriority(), 19);
assert.equal((await readFile('/proc/self/status', 'utf8')).match(/^Cpus_allowed_list:\s*(.+)$/m)?.[1], '0');
assert.ok(!['/dev/dri', '/dev/nvidiactl', '/dev/kfd'].some(existsSync));
assert.match(process.env.GLISS_AUDIO_HOST_NETNS ?? '', /^net:\[[0-9]+\]$/);
assert.notEqual(await readlink('/proc/self/ns/net'), process.env.GLISS_AUDIO_HOST_NETNS);
const planRaw = await readFile(new URL('../'+planName, import.meta.url));
const plan = JSON.parse(planRaw), hash = raw => createHash('sha256').update(raw).digest('hex');
assert.equal(plan.schema,'gliss-audio-packet-duration-plan-v1');assert.equal(plan.window_ms,20000);
assert.deepEqual(plan.order,['ms20','ms40','ms40','ms20']);assert.deepEqual(plan.audio_packet_ms,{ms20:20,ms40:40});
assert.deepEqual(plan.qp_bounds,{min:42,max:51});
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
async function until(deadline) {while (performance.now() < deadline) await sleep(Math.min(50, deadline - performance.now()));}

test('owned Opus packet-duration comparison: eight fixed20s positions', {timeout: 270000}, async t => {
  const result = {schema: 'gliss-audio-packet-duration-results-v1', plan_sha256: hash(planRaw),
    binary_sha256: hash(await readFile(serverBinary())), source_pins: {},
    qualification: plan.qualification, physical_speaker_latency_ms: null, cleanup_complete: false, positions: []};
  for (const source of ['scripts/bench-audio-packet-duration.mjs',planName,'benchmarks/audio-packet-duration/criteria.json','test-support/audio-packet-duration-study/assessment.mjs', 'test-support/qemu-audio-motion/fixture-budget.mjs',
    'test-support/qemu-audio-motion/init.c','test-support/qemu-audio-motion/build.py',
    'test-support/audio-packet-duration-study/server.mjs','test-support/audio-packet-duration-study/observer.mjs',
    'test-support/qemu-audio-motion/stamp.js','test-support/qemu-audio-motion/raw-stamp.mjs',
    'test-support/qemu-audio-motion/cpu.mjs',
    'test-support/qemu-audio-marker/init.c', 'test-support/qemu-audio-marker/build.py',
    'test-support/qemu-audio-marker/browser.mjs', 'test-support/qemu-audio-marker/detector.js',
    'test-support/qemu-audio-marker/receipt.mjs', 'test-support/browser/audio-kernel-link.mjs',
    'test-support/browser/audio-policy-server.mjs', 'tests/helpers.mjs', 'crates/gliss-server/src/audio.rs','crates/gliss-server/src/main.rs','web/audio-player.js'])
    result.source_pins[source] = hash(await readFile(new URL('../' + source, import.meta.url)));
  for (const profile of plan.profiles) for (const mode of plan.order) {
    const slot = result.positions.length + 1;
    result.positions.push({slot, profile: profile.name, mode, status: 'blocked', pulses: plan.pulse_offsets_ms.map((offset, i) =>
      ({token: (slot - 1) * 6 + i + 1, code: (slot - 1) * 6 + i + 1, planned_offset_ms: offset,
        submitted_host_ms: null, status: 'not-submitted'}))});
  }
  const cleanup = [];let vm, browser, activeProfile, cleanupFailed=false;
  result.guest_builds=[];result.raw_sources=[];
  async function persist() {
    const raw=JSON.stringify(result)+'\n';assert.ok(Buffer.byteLength(raw)<=8388608);
    const f=await open(output+'.next','w',0o600);
    try {await f.writeFile(raw);await f.sync();}finally{await f.close();}
    await rename(output+'.next',output);
  }
  async function closeGuest() {
    let failure;
    for (const close of cleanup.splice(0).reverse()) try {await close();}catch(e){failure??=e;}
    if(failure)throw failure;
    if(vm)assert.ok(vm.child.exitCode!==null||vm.child.signalCode!==null);
  }
  try {
    await persist();
    browser = await launchBrowser({handleSIGINT: false, handleSIGTERM: false});
    for (const position of result.positions) {
      if(position.profile!==activeProfile){
        await closeGuest();
        vm=await startAudioMotionVm({after:close=>cleanup.push(close)},{cpu:plan.guest_cpu,mixFrequency:plan.source_mix_frequency});
        result.audio_device=vm.audioDevice;result.guest_build??=vm.buildIdentity;
        result.guest_builds.push({profile:position.profile,...vm.buildIdentity});
        const raw=await rawStamp(vm.vnc);assert.equal(raw.width,1280);assert.equal(raw.height,800);result.raw_sources.push({profile:position.profile,...raw});activeProfile=position.profile;
      }
      position.status='running';await persist();
      let link, server, page, cdp, active = true, overflow = false;
      const observations = [], errors = [];
      try {
        position.phase='setup';
        const profile = plan.profiles.find(row => row.name === position.profile), port = await freePort();
        await vm.setMotion(false);await sleep(200);const frozenFrame = vm.frames.at(-1).frame;
        if (profile.mbps !== null) link = await startKernelLink({targetPort: port, rttMs: profile.rttMs, mbps: profile.mbps});
        server = await startDurationServer({tcpNodelay: true, port, vnc: vm.vnc, codec: 'h264', fps: 30,
          h264Bitrate: plan.video_bitrate_target, inFlightFrames: plan.video_in_flight_frames, frameSkip: false, qpBounds:plan.qp_bounds,audioPacketMs:plan.audio_packet_ms[position.mode], ...(link ? {origin: link.url} : {})});
        assert.equal(getPriority(server.child.pid), 19);assert.equal(getPriority(vm.child.pid), 19);
        const allowed = async pid => (await readFile(`/proc/${pid}/status`, 'utf8')).match(/^Cpus_allowed_list:\s*(.+)$/m)?.[1];
        position.cpu_admission = {native: await allowed(server.child.pid), guest: await allowed(vm.child.pid)};
        assert.equal(position.cpu_admission.native, '0');assert.equal(position.cpu_admission.guest, '19');
        page = await browser.newPage();page.on('pageerror', error => errors.push(error.name));
        await page.addInitScript(() => {
          window.markerSocketPolicy = null;window.markerFrameSkip = null;window.markerQpBounds = null;window.markerAudioPacketMs=null;
          window.WebSocket = new Proxy(WebSocket, {construct(Target, args) {
            const socket = new Target(...args);
            socket.addEventListener('message', event => {
              window.motionWireTap?.(event);
              if (typeof event.data !== 'string') return;
              const value = JSON.parse(event.data);if (value.type === 'hello') {window.markerSocketPolicy = value.tcp_listener;window.markerFrameSkip = value.video_frame_skip ?? false;window.markerQpBounds = value.video_qp_bounds ?? null;window.markerAudioPacketMs=value.audio_packet_ms;}
            });return socket;
          }});
        });
        await loginPage(page, server);
        position.socket_policy = validateSocketPolicy(await page.evaluate(() => window.markerSocketPolicy), true);
        position.frame_skip = await page.evaluate(() => window.markerFrameSkip);
        assert.equal(position.frame_skip, false);
        position.qp_bounds = await page.evaluate(() => window.markerQpBounds);
        assert.deepEqual(position.qp_bounds,{...plan.qp_bounds,scope:'requested native bounds; not coded-QP attestation'});
        position.audio_packet_ms=await page.evaluate(()=>window.markerAudioPacketMs);assert.equal(position.audio_packet_ms,plan.audio_packet_ms[position.mode]);
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
        position.phase='measurement';
        cdp = await browser.newBrowserCDPSession();
        const beforeCpu = {native:await processCounter(server.child.pid),guest:await processCounter(vm.child.pid),browser:await browserCounter(cdp)};
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
        const afterCpu = {native:await processCounter(server.child.pid),guest:await processCounter(vm.child.pid),browser:await browserCounter(cdp)};
        position.cpu_boundaries={before:beforeCpu,after:afterCpu};
        position.cpu = {native:processCpu(beforeCpu.native,afterCpu.native),guest:processCpu(beforeCpu.guest,afterCpu.guest),browser:browserCpu(beforeCpu.browser,afterCpu.browser)};
        await cdp.detach();cdp=null;
        position.source_frames = vm.frames.filter(row => row.host_received_ms >= position.host_window_start_ms && row.host_received_ms < position.host_window_start_ms + plan.window_ms);
        position.pulses = position.pulses.map(pulse => finishMarker(pulse, observations, vm.notes.get(pulse.token),
          position.clock, position.browser_window, plan.pulse_deadline_ms));
        position.page_errors=[...errors];position.observation_overflow=overflow;
        position.audio_assessment=assessAudioWindow(position.metrics,position.audio_packet_ms);
        position.measurement_valid = position.audio_assessment.valid && !overflow && !position.metrics.overflow && errors.length === 0 && position.metrics.observer_failure === null &&
          position.metrics.video_errors_delta === 0 && position.metrics.video_marker_eligible &&
          position.metrics.sequence_gaps === 0 && position.metrics.invalid_skip_stats === 0 && position.metrics.invalid_video_headers === 0 && position.metrics.invalid_audio_headers === 0 && position.metrics.audio_sequence_gaps === 0 &&
          position.cpu.browser.complete_endpoint_set && Object.values(position.cpu).every(c=>c.elapsed_ms>=20000&&c.elapsed_ms<=22000) &&
          (!position.frame_skip || position.metrics.skip_samples.length > 0) && position.pulses.every(pulse => pulse.source_valid &&
            pulse.submission_lateness_ms <= plan.max_submission_lateness_ms &&
            !['invalid-observation', 'preexisting-or-ambiguous'].includes(pulse.status));
        position.timely_responses = position.pulses.filter(pulse => pulse.status === 'observed').length;
        position.complete_timely_set = position.timely_responses === plan.markers_per_position;
        if (link) {
          position.kernel_link = await link.snapshot();
          assert.ok(position.kernel_link.queues.every(row => row.packets > 0 && row.drops === 0));
        }
        position.status = 'measured';position.phase='complete';
        t.diagnostic(JSON.stringify({slot: position.slot, profile: position.profile, mode: position.mode,
          valid: position.measurement_valid, timely: position.timely_responses,
          native_skips:position.metrics.skip_samples.at(-1)?.count ?? null,sequence_gaps:position.metrics.sequence_gaps,
          paints:position.metrics.paints.length,unique_source_frames:position.metrics.unique_source_frames,
          native_cpu:position.cpu.native.percent_of_one_core,browser_cpu:position.cpu.browser.percent_of_one_core,
          outcomes: position.pulses.map(pulse => [pulse.code, pulse.status, pulse.trigger_to_observed_ms])}));
      } catch (error) {
        position.status = 'failed';position.failure = error.name;position.observations = observations;
        throw error;
      } finally {
        active=false;let cleanupError;
        for(const close of [
          async()=>{if(cdp)await cdp.detach();},
          async()=>{if(page){await page.evaluate(()=>{window.glissMarkerProbe?.stop();window.gliss?.dispose();}).catch(()=>{});await page.close();}},
          async()=>{if(link){await link.stop();position.kernel_cleanup=link.cleanup;}},
          async()=>{if(server){await server.stop();position.native_exit={code:server.child.exitCode,signal:server.child.signalCode};}}
        ])try{await close();}catch(error){cleanupError??=error;}
        position.cleanup_complete=!cleanupError;
        if(cleanupError){cleanupFailed=true;position.cleanup_failure=cleanupError.name;}
        if(server&&(server.child.exitCode!==0||server.child.signalCode!==null)){
          position.measurement_valid=false;position.cleanup_failure??='NonNormalNativeExit';cleanupFailed=true;
        }
        await persist();
        if(cleanupFailed)throw cleanupError??new Error('Owned position cleanup gate failed');
      }
    }
  } catch(error){result.failure=error.name;throw error;} finally {
    let clean=!cleanupFailed;
    try{await browser?.close();}catch{clean=false;}
    try{await closeGuest();}catch{clean=false;}
    result.cleanup_complete=clean;
    await persist();assert.equal(clean,true);
  }
});
