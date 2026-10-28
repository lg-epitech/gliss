// Opt-in, fixed-position audio cost/control-RTT study. See benchmarks/audio-transport/plan.json.
// Uses its own diskless guest; never connects to Distribox or an existing VM.
import test from 'node:test';
import assert from 'node:assert/strict';
import {readFile, writeFile, lstat} from 'node:fs/promises';
import {existsSync} from 'node:fs';
import {getPriority} from 'node:os';
import {createHash} from 'node:crypto';
import {execFileSync} from 'node:child_process';
import {isAbsolute} from 'node:path';
import {performance} from 'node:perf_hooks';
import {freePort, launchBrowser, loginPage, serverBinary} from '../tests/helpers.mjs';
import {startPolicyServer, validateSocketPolicy} from '../test-support/browser/audio-policy-server.mjs';
import {startAudioVm} from '../test-support/qemu-audio/fixture.mjs';
import {startLimitedProxy} from '../test-support/browser/limited-proxy.mjs';

const args = process.argv.slice(2), options = {};
for (let i = 0; i < args.length; i += 2) {
  assert.ok(['--output', '--label', '--guest-cpu'].includes(args[i]) && args[i + 1] && !(args[i] in options), 'Unknown or duplicate benchmark argument');
  options[args[i]] = args[i + 1];
}
assert.ok(options['--output'] && /^[a-zA-Z0-9.-]{1,64}$/.test(options['--label'] ?? ''), 'Supply a fresh output file and a short label');
const guestCpu = Number(options['--guest-cpu']);
assert.ok(Number.isSafeInteger(guestCpu) && guestCpu > 0, 'Guest CPU must be explicit and distinct from native/browser CPU 0');
assert.ok(process.env.GLISS_BINARY && isAbsolute(serverBinary()), 'Explicit freshly built binary required');
assert.equal(getPriority(), 19, 'Run the complete study at nice 19');
assert.equal((await readFile('/proc/self/status', 'utf8')).match(/^Cpus_allowed_list:\s*(.+)$/m)?.[1], '0', 'Pin the controller/native/browser to CPU 0');
assert.ok(!['/dev/dri', '/dev/nvidiactl', '/dev/kfd'].some(existsSync), 'Run inside the private CPU-only device namespace');
try {await lstat(options['--output']);throw new Error('Output already exists');} catch (error) {if (error.code !== 'ENOENT') throw error;}
const planRaw = await readFile(new URL('../benchmarks/audio-transport/plan.json', import.meta.url));
const plan = JSON.parse(planRaw), hash = bytes => createHash('sha256').update(bytes).digest('hex');
const hz = Number(execFileSync('getconf', ['CLK_TCK'], {encoding: 'utf8'}));
assert.ok(Number.isSafeInteger(hz) && hz > 0);
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const percentile = (values, fraction) => {
  const sorted = values.toSorted((a, b) => a - b);
  return sorted.length ? sorted[Math.min(sorted.length - 1, Math.ceil(sorted.length * fraction) - 1)] : null;
};

async function processCounter(pid) {
  const begin = performance.now(), stat = await readFile(`/proc/${pid}/stat`, 'utf8');
  const fields = stat.slice(stat.lastIndexOf(')') + 2).trim().split(/\s+/);
  const allowed = (await readFile(`/proc/${pid}/status`, 'utf8')).match(/^Cpus_allowed_list:\s*(.+)$/m)?.[1];
  assert.equal(Number(fields[16]), 19, 'Owned measured process must retain nice 19');
  return {begin_ms: begin, end_ms: performance.now(), start: fields[19], allowed_cpus: allowed, ticks: Number(fields[11]) + Number(fields[12])};
}

function processCpu(before, after) {
  assert.equal(before.start, after.start);assert.ok(after.ticks >= before.ticks);
  assert.equal(before.allowed_cpus, after.allowed_cpus);
  const elapsed = (after.begin_ms + after.end_ms - before.begin_ms - before.end_ms) / 2;
  return {scope: 'Owned process and its threads; independent midpoint counter window',
    allowed_cpus: before.allowed_cpus, ticks: after.ticks - before.ticks, ticks_per_second: hz, elapsed_ms: elapsed,
    percent_of_one_core: (after.ticks - before.ticks) / hz * 100000 / elapsed};
}

async function browserCounter(cdp) {
  const begin = performance.now(), {processInfo} = await cdp.send('SystemInfo.getProcessInfo');
  const rows = await Promise.all(processInfo.map(async row => {
    const measured = await processCounter(row.id);assert.equal(measured.allowed_cpus, '0');
    return {start: measured.start, pid: row.id, cpu: row.cpuTime};
  }));
  return {begin_ms: begin, end_ms: performance.now(), rows};
}

function browserCpu(before, after) {
  const first = new Map(before.rows.map(row => [`${row.pid}:${row.start}`, row.cpu]));
  const common = after.rows.filter(row => first.has(`${row.pid}:${row.start}`));
  const seconds = common.reduce((sum, row) => sum + row.cpu - first.get(`${row.pid}:${row.start}`), 0);
  const elapsed = (after.begin_ms + after.end_ms - before.begin_ms - before.end_ms) / 2;
  return {scope: 'CDP endpoint PID/start intersection; independent midpoint counter window',
    matched_processes: common.length, before_processes: before.rows.length, after_processes: after.rows.length,
    complete_endpoint_set: common.length === before.rows.length && common.length === after.rows.length,
    cpu_seconds: seconds, elapsed_ms: elapsed, percent_of_one_core: seconds * 100000 / elapsed};
}

test('audio TCP scheduling: matched explicit false/true socket policy', {timeout: 280000}, async t => {
  const cleanup = [], binary = serverBinary();
  const result = {schema: 'gliss-audio-transport-results-v1', label: options['--label'], plan_sha256: hash(planRaw),
    binary_sha256: hash(await readFile(binary)), guest_cpu: guestCpu, native_browser_cpu: 0,
    qualification: plan.qualification, source_to_speaker_latency_ms: null, motion_fps: null,
    source_pins: {}, cleanup_complete: false, positions: []};
  for (const source of ['scripts/bench-audio-transport.mjs', 'test-support/browser/audio-policy-server.mjs', 'test-support/qemu-audio/fixture.mjs', 'test-support/qemu-audio/build.py',
    'test-support/qemu-audio/init.c', 'test-support/browser/limited-proxy.mjs', 'tests/helpers.mjs'])
    result.source_pins[source] = hash(await readFile(new URL('../' + source, import.meta.url)));
  for (const profile of plan.profiles) for (const treatment of plan.order)
    result.positions.push({slot: result.positions.length + 1, profile: profile.name, ...treatment, status: 'blocked', metrics: null});
  let vm, browser;
  try {
    vm = await startAudioVm({after: fn => cleanup.push(fn)}, {cpu: guestCpu});
    browser = await launchBrowser({handleSIGINT: false, handleSIGTERM: false});
    for (const position of result.positions) {
      const profile = plan.profiles.find(p => p.name === position.profile);
      let server, proxy, page, cdp;
      try {
        const port = await freePort();
        if (profile.mbps !== null) proxy = await startLimitedProxy({targetPort: port, ...profile,
          maxQueueBytes: plan.proxy_max_queued_bytes_per_direction});
        server = await startPolicyServer({tcpNodelay: position.tcp_nodelay, port, vnc: vm.vnc, codec: plan.codec, fps: plan.video_fps_cap,
          h264Bitrate: plan.video_bitrate_target, inFlightFrames: plan.video_in_flight_frames,
          ...(proxy ? {origin: proxy.url} : {})});
        page = await browser.newPage();const errors = [];page.on('pageerror', error => errors.push(error.name));
        if (position.mode === 'pcm') await page.addInitScript(() => {window.AudioDecoder = undefined;});
        await page.addInitScript(() => {
          window.benchSocketPolicy = null;
          window.WebSocket = new Proxy(WebSocket, {construct(Target, args) {
            const socket = new Target(...args);
            socket.addEventListener('message', event => {
              if (typeof event.data !== 'string') return;
              const value = JSON.parse(event.data);
              if (value.type === 'hello') window.benchSocketPolicy = value.tcp_listener;
            });
            return socket;
          }});
          const trace = window.audioDecodeTrace = {start: Infinity, end: -Infinity, delays: [], unmatched: 0, pendingAtClose: 0};
          if (!window.AudioDecoder) return;
          window.AudioDecoder = new Proxy(AudioDecoder, {construct(Target, [callbacks]) {
            const pending = new Map();
            const decoder = new Target({...callbacks, output(audio) {
              const now = performance.now();
              let timestamp = audio.timestamp;
              if (!pending.has(timestamp)) {
                if (pending.has(timestamp - 1)) timestamp--;
                else if (pending.has(timestamp + 1)) timestamp++;
              }
              const received = pending.get(timestamp);pending.delete(timestamp);
              if (now >= trace.start && now < trace.end) {
                if (received === undefined) trace.unmatched++;
                else if (trace.delays.length < 1000) trace.delays.push(now - received);
              }
              callbacks.output(audio);
            }});
            const decode = decoder.decode.bind(decoder), close = decoder.close.bind(decoder);
            decoder.decode = chunk => {
              if (pending.size >= 1024) throw new Error('Bounded decode trace capacity');
              pending.set(chunk.timestamp, performance.now());return decode(chunk);
            };
            decoder.close = () => {
              if (performance.now() >= trace.start && performance.now() < trace.end) trace.pendingAtClose += pending.size;
              pending.clear();return close();
            };
            return decoder;
          }});
        });
        await loginPage(page, server, proxy?.url ?? server.url);
        await page.waitForFunction(() => window.gliss.audio.available);
        position.socket_policy = validateSocketPolicy(await page.evaluate(() => window.benchSocketPolicy), position.tcp_nodelay);
        if (position.mode !== 'off') {
          await page.evaluate(() => {
            const button = document.createElement('button');button.id = 'bench-sound';button.textContent = 'Sound';
            button.onclick = () => {window.benchSound = window.gliss.setAudioEnabled(true);};document.body.append(button);
          });
          await page.click('#bench-sound');await page.evaluate(() => window.benchSound);
          assert.equal(await page.evaluate(() => window.gliss.audio.codec), position.mode);
          await page.waitForFunction(() => window.gliss.audio.scheduled >= 5);
        }
        await sleep(plan.warmup_ms);
        cdp = await browser.newBrowserCDPSession();
        const nativeBefore = await processCounter(server.child.pid), guestBefore = await processCounter(vm.child.pid);
        assert.equal(nativeBefore.allowed_cpus, '0');assert.equal(guestBefore.allowed_cpus, String(guestCpu));
        const browserBefore = await browserCounter(cdp);
        const observed = await page.evaluate(async plan => {
          const client = window.gliss, socket = client.socket, start = performance.now(), deadline = start + plan.measurement_ms;
          const pings = [], samples = [], wire = {bytes: 0, frames: 0, packets: 0};
          const before = client.audio.snapshot(), videoBefore = {...client.metrics};
          window.audioDecodeTrace.start = start;window.audioDecodeTrace.end = deadline;
          const receive = event => {
            const now = performance.now();if (now >= deadline) return;
            if (event.data instanceof ArrayBuffer && event.data.byteLength >= 20 && new DataView(event.data).getUint32(0, true) === 0x31414c47) {
              wire.bytes += event.data.byteLength;wire.frames += new DataView(event.data).getUint32(16, true);wire.packets++;
            } else if (typeof event.data === 'string') {
              const value = JSON.parse(event.data);
              if (value.type === 'pong' && Number.isInteger(value.id) && pings[value.id]?.received_ms === null) pings[value.id].received_ms = now - start;
            }
          };
          socket.addEventListener('message', receive);
          const sample = () => {if (performance.now() < deadline) samples.push({...client.audio.snapshot(), at_ms: performance.now() - start});};
          const ping = () => {
            const now = performance.now();
            if (now >= deadline || pings.length >= plan.measurement_ms / plan.ping_interval_ms) return;
            const id = pings.length;pings.push({sent_ms: now - start, received_ms: null});socket.send(JSON.stringify({type: 'ping', id}));
          };
          sample();ping();
          const sampling = setInterval(sample, plan.sample_interval_ms), probing = setInterval(ping, plan.ping_interval_ms);
          await new Promise(resolve => setTimeout(resolve, plan.measurement_ms));
          clearInterval(sampling);clearInterval(probing);socket.removeEventListener('message', receive);
          return {window_ms: plan.measurement_ms, controller_elapsed_ms: performance.now() - start, before,
            samples, pings, wire, decoder_trace: window.audioDecodeTrace,
            video_frames_delta: client.metrics.frames - videoBefore.frames,
            video_errors_delta: client.metrics.errors - videoBefore.errors};
        }, plan);
        const nativeAfter = await processCounter(server.child.pid), guestAfter = await processCounter(vm.child.pid);
        const browserAfter = await browserCounter(cdp);
        assert.deepEqual(errors, []);assert.equal(observed.video_errors_delta, 0);
        const replies = observed.pings.filter(p => p.received_ms !== null).map(p => p.received_ms - p.sent_ms);
        const queue = observed.samples.map(s => s.queuedSeconds * 1000), last = observed.samples.at(-1);
        assert.equal(last.enabled, position.mode !== 'off');
        assert.equal(last.codec, position.mode === 'off' ? null : position.mode);
        if (position.mode !== 'off') {
          assert.ok(observed.wire.packets > 0 && last.scheduled > observed.before.scheduled, 'Enabled audio must actually reach playback');
        } else assert.equal(observed.wire.packets, 0);
        position.status = 'measured';
        position.metrics = {native_cpu: processCpu(nativeBefore, nativeAfter), guest_cpu: processCpu(guestBefore, guestAfter),
          browser_cpu: browserCpu(browserBefore, browserAfter), window_ms: observed.window_ms,
          controller_elapsed_ms: observed.controller_elapsed_ms, audio_wire: observed.wire,
          audio_received_kbit_s: observed.wire.bytes * 8 / observed.window_ms,
          audio_kbit_per_second_of_received_samples: observed.wire.frames ? observed.wire.bytes * 8 * 48000 / observed.wire.frames / 1000 : null,
          playback_queue_ms: {p50: percentile(queue, 0.5), p95: percentile(queue, 0.95), max: Math.max(...queue)},
          decoder_pending_max: Math.max(...observed.samples.map(s => s.pendingDecodes ?? 0)),
          decoder_output_delay_ms: {scope: 'Local decode submission to native output callback; outputs inside the fixed browser window',
            p50: percentile(observed.decoder_trace.delays, 0.5), p95: percentile(observed.decoder_trace.delays, 0.95),
            max: observed.decoder_trace.delays.length ? Math.max(...observed.decoder_trace.delays) : null,
            matched_outputs: observed.decoder_trace.delays.length, unmatched_outputs: observed.decoder_trace.unmatched,
            pending_at_decoder_close: observed.decoder_trace.pendingAtClose},
          drop_counter_delta: last.dropped - observed.before.dropped,
          ping: {sent: observed.pings.length, completed: replies.length, missed_at_cutoff: observed.pings.length - replies.length,
            complete: replies.length === 20 && observed.pings.length === 20, responded_p50_ms: percentile(replies, 0.5), responded_p95_ms: percentile(replies, 0.95), responded_max_ms: replies.length ? Math.max(...replies) : null},
          video_frames_delta: observed.video_frames_delta, video_errors_delta: observed.video_errors_delta,
          proxy: proxy ? {scope: 'Cumulative session counters including authentication and warmup', ...proxy.stats} : null,
          samples: observed.samples, pings: observed.pings};
        t.diagnostic(JSON.stringify({slot: position.slot, profile: position.profile, mode: position.mode, tcp_nodelay: position.tcp_nodelay,
          native_cpu: position.metrics.native_cpu.percent_of_one_core, browser_cpu: position.metrics.browser_cpu.percent_of_one_core,
          audio_kbit_s: position.metrics.audio_received_kbit_s, ping: position.metrics.ping}));
      } catch (error) {
        position.status = 'failed';position.failure = error.name;
        throw error;
      } finally {
        await cdp?.detach();
        if (page) {await page.evaluate(() => window.gliss?.dispose()).catch(() => {});await page.close();}
        await proxy?.stop();
        if (proxy) position.proxy_cleanup = {...proxy.stats};
        if (server) {
          await server.stop();
          const due = performance.now() + 2000;
          while (server.child.exitCode === null && server.child.signalCode === null && performance.now() < due) await sleep(10);
          assert.ok(server.child.exitCode !== null || server.child.signalCode !== null, 'Owned server must exit before another position');
        }
      }
    }
  } finally {
    await browser?.close();
    for (const close of cleanup.reverse()) await close();
    result.cleanup_complete = !vm || vm.child.exitCode !== null || vm.child.signalCode !== null;
    const raw = JSON.stringify(result) + '\n';assert.ok(Buffer.byteLength(raw) <= 1048576);
    await writeFile(options['--output'], raw, {flag: 'wx', mode: 0o600});
    assert.equal(result.cleanup_complete, true);
  }
});
