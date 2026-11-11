// Opt-in Linux/KVM benchmark of what a Distribox viewer experiences, end to end:
// a real QEMU guest (virtio-gpu, PS/2 keyboard, virtio tablet) -> QEMU's own VNC
// server -> gliss-server with Distribox's flags -> the binary's embedded browser
// client over WebSocket -> H.264 -> Canvas. One guest and one browser serve the whole
// run; every round starts a fresh server and page per binary (order reversed on odd
// rounds, so two binaries run ABBA).
//
// node scripts/bench-distribox-path.mjs --binary /abs/gliss-server [--binary /abs/other ...]
//   [--label name ...] [--size 1920x1080] [--rounds 3] [--scenarios typing,burst,scroll,drag,video]
//   [--typing-keys 24] [--output file.json] [--trace-refresh]
//
// Scenarios: typing (isolated keys), burst (fast typing), scroll (60 Hz terminal
// scroll with probe keys), drag (a window following the pointer), video (30 Hz
// region); a 5 s idle window on the static scene is always recorded last.
// Latencies run from the client's send to the first painted frame whose marker strip
// (test-support/qemu-display/init.c) shows the guest drew that input. A probe that
// never shows is a miss, never a latency. --trace-refresh logs every change of QEMU's
// display refresh interval and reports the time spent at each. --output receives every
// raw sample; the tables print medians across rounds. Exits 1 if any scenario failed
// to observe its markers or missed a probe.
// Prerequisites: /dev/kvm, qemu-system-x86_64, a readable host kernel with matching
// virtio-gpu/virtio_input modules, gcc with static libc, cpio, zstd, Playwright, and a
// temporary directory on tmpfs.
import {execFileSync} from 'node:child_process';
import {createHash} from 'node:crypto';
import {existsSync, mkdtempSync, readFileSync, rmSync, statfsSync, writeFileSync} from 'node:fs';
import {cpus, loadavg, release, tmpdir} from 'node:os';
import {basename, isAbsolute, join, resolve} from 'node:path';
import {launchBrowser} from '../tests/helpers.mjs';
import {TICK_MS, childPids, clock, cpuBetween, openViewer, pageClockOffset, processTimes, startDisplayVm,
  startGliss} from '../test-support/qemu-display/fixture.mjs';

const options = {binary: [], label: [], size: '1920x1080', rounds: '3', scenarios: 'typing,burst,scroll,drag,video',
  'typing-keys': '24', output: null, 'trace-refresh': false};
const usage = message => {console.error(`${message}\nSee the header of scripts/bench-distribox-path.mjs`);process.exit(2);};
for (let i = 2; i < process.argv.length; i++) {
  const match = /^--([a-z-]+)(?:=(.*))?$/.exec(process.argv[i]);
  if (!match || !(match[1] in options)) usage(`Unknown argument: ${process.argv[i]}`);
  if (options[match[1]] === false) {options[match[1]] = true;continue;}
  const value = match[2] ?? process.argv[++i];
  if (value === undefined || value.startsWith('--')) usage(`Missing value for --${match[1]}`);
  if (Array.isArray(options[match[1]])) options[match[1]].push(value);else options[match[1]] = value;
}
const [width, height] = options.size.split('x').map(Number), rounds = Number(options.rounds), typingKeys = Number(options['typing-keys']);
const selected = options.scenarios.split(',').filter(Boolean);
if (!options.binary.length) usage('--binary /abs/path/gliss-server is required');
if (options.label.length > options.binary.length) usage('More --label than --binary');
if (!(width >= 1280 && height >= 720 && width % 2 === 0 && height % 2 === 0 && width <= 3840 && height <= 2160)) usage('--size must be even, from 1280x720 to 3840x2160');
if (!(Number.isInteger(rounds) && rounds > 0 && rounds <= 50)) usage('--rounds must be an integer from 1 to 50');
if (!(Number.isInteger(typingKeys) && typingKeys > 0 && typingKeys <= 200)) usage('--typing-keys must be an integer from 1 to 200');
const binaries = options.binary.map((path, n) => {
  if (!isAbsolute(path) || !existsSync(path)) usage(`--binary must name an existing absolute path: ${path}`);
  return {path, label: options.label[n] ?? basename(path), sha256: createHash('sha256').update(readFileSync(path)).digest('hex')};
});
if (new Set(binaries.map(binary => binary.label)).size !== binaries.length) usage('Binaries need distinct labels');

// The browser runs with --disable-dev-shm-usage and keeps its shared memory in the
// temporary directory. On a disk under I/O pressure the page then freezes for seconds.
if (statfsSync(tmpdir()).type !== 0x01021994) usage(`TMPDIR must be on tmpfs (for example /dev/shm): ${tmpdir()}`);

const GRACE_MS = 1500, WARMUP_MS = 500;
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const F = n => 0xffbd + n, key = (keysym, down) => ({type: 'key', keysym, down});
const press = keysym => [{at: 0, message: key(keysym, true), tag: 'scene'}, {at: 70, message: key(keysym, false), tag: 'scene'}];

// Every run replays the same schedule: all timing randomness comes from the scenario's name.
function seeded(name) {
  let state = [...name].reduce((hash, char) => Math.imul(hash ^ char.charCodeAt(0), 0x01000193), 0x811c9dc5) >>> 0;
  return (low, high) => {
    state = state + 0x6d2b79f5 >>> 0;
    let t = Math.imul(state ^ state >>> 15, 1 | state);
    t = t + Math.imul(t ^ t >>> 7, 61 | t) ^ t;
    return low + ((t ^ t >>> 14) >>> 0) / 4294967296 * (high - low);
  };
}

// Probe keys at seeded gaps. Each key-up follows its key-down by 60..120 ms and is a
// real message of its own. A probe's tag is its ordinal, the value marker K must reach.
function typed(name, count, minGap, maxGap, duration) {
  const random = seeded(name), events = [];
  for (let n = 1, at = 0; n <= count; n++) {
    at += random(minGap, maxGap);
    const keysym = 97 + n * 7 % 26;  // letters; never the same key twice in a row
    events.push({at, message: key(keysym, true), tag: n}, {at: at + random(60, 120), message: key(keysym, false), tag: 'up'});
  }
  events.sort((a, b) => a.at - b.at);
  return {events, duration: duration ?? events.at(-1).at + 400};
}

// 480 pointer positions at 60 Hz: horizontal sweeps at 9 px per step under a slow
// vertical sine. Each sweep is shifted by a different sub-step, so no x repeats and
// marker P names exactly one position of the path.
function dragPath() {
  const speed = 9, low = 364, sweep = Math.min(100, Math.floor((width - 2 * low) / speed));
  const path = Array.from({length: 480}, (_, step) => {
    const pass = Math.floor(step / sweep), along = step % sweep;
    return [low + pass * 4 % speed + (pass % 2 ? sweep - 1 - along : along) * speed,
      Math.round(height / 2 + Math.min(200, height / 4) * Math.sin(step * Math.PI / 120))];
  });
  if (new Set(path.map(([x]) => x)).size !== path.length) throw new Error('Drag path repeats an x');
  return path;
}

// marker: column of the recorded frame whose changes count as new guest content (5 = F, 6 = P).
const SCENARIOS = {
  typing: {plan: () => typed('typing', typingKeys, 700, 1500)},
  burst: {plan: () => typed('burst', 40, 110, 220)},
  scroll: {scene: F(2), marker: 5, plan: () => typed('scroll', 15, 440, 520, 8000)},
  drag: {scene: F(3), marker: 6, path: dragPath},
  video: {scene: F(4), marker: 5, plan: () => ({events: [], duration: 8000})},
  idle: {settle: 2000, plan: () => ({events: [], duration: 5000})},
};
for (const name of selected) if (!(name in SCENARIOS) || name === 'idle') usage(`Unknown scenario: ${name}`);
selected.push('idle');  // always last: the static scene, long enough after the last motion

const quantile = (sorted, q) => {
  const position = (sorted.length - 1) * q, low = Math.floor(position);
  return sorted[low] + (sorted[Math.min(sorted.length - 1, low + 1)] - sorted[low]) * (position - low);
};
const spread = values => {
  if (!values.length) return {count: 0};
  const sorted = [...values].sort((a, b) => a - b);
  return {count: sorted.length, mean: sorted.reduce((sum, value) => sum + value, 0) / sorted.length,
    p50: quantile(sorted, 0.5), p95: quantile(sorted, 0.95), p99: quantile(sorted, 0.99), max: sorted.at(-1)};
};

// Time spent at each refresh interval QEMU logged, between two host times.
function refreshSummary(timeline, from, to) {
  const time = {};
  let current = null, cursor = from, changes = 0;
  const spend = until => {if (current !== null) time[current] = (time[current] ?? 0) + until - cursor;cursor = until;};
  for (const {at, interval} of timeline) {
    if (at >= to) break;
    if (at > from) {spend(at);changes++;}
    current = interval;
  }
  spend(to);
  const total = Object.values(time).reduce((sum, value) => sum + value, 0);
  if (!total) return null;
  return {changes, maxMs: Math.max(...Object.keys(time).map(Number)), ms: Object.fromEntries(Object.entries(time).map(([interval, spent]) => [interval, Math.round(spent)])),
    meanMs: Object.entries(time).reduce((sum, [interval, spent]) => sum + interval * spent, 0) / total, shareAt30: (time[30] ?? 0) / total};
}

async function runScenario(name, {vm, server, viewer, chromium}) {
  const spec = SCENARIOS[name], {page} = viewer;
  const play = (events, duration) => page.evaluate(([events, duration]) => window.glissBench.play(events, duration), [events, duration]);
  // F5, then the canvas itself must show the cleared counters before anything is timed.
  await play(press(F(5)), 100);
  const settled = clock() + (spec.settle ?? 1000);
  let marks;
  for (const deadline = clock() + 4000; clock() < deadline; await sleep(50)) {
    marks = await page.evaluate(() => window.glissBench.read());
    if (marks.k === 0 && marks.p === 0 && marks.f >= 0) break;
  }
  if (marks.k !== 0 || marks.p !== 0 || marks.f < 0) throw new Error(`reset not shown by the markers: ${JSON.stringify(marks)}`);
  await sleep(Math.max(0, settled - clock()));
  if (spec.scene) await play(press(spec.scene), WARMUP_MS);  // the scene switch itself is not measured
  await page.evaluate(() => window.glissBench.take());
  const before = processTimes(), from = clock();
  const plan = spec.plan?.();
  const span = spec.path
    ? await page.evaluate(([path, step]) => window.glissBench.drag(path, step), [spec.path(), TICK_MS])
    : await play(plan.events, plan.duration);
  const after = processTimes(), to = clock();
  if (spec.path) await play([{at: 0, message: {type: 'pointer', x: width >> 1, y: height >> 1, buttons: 0}, tag: 'release'}], 0);
  await sleep(GRACE_MS);  // late frames may still answer the last probes
  const taken = await page.evaluate(() => window.glissBench.take());
  const cpu = {server: cpuBetween(before, after, [server.pid]), qemu: cpuBetween(before, after, [vm.pid]),
    qemuGuest: cpuBetween(before, after, [vm.pid], true), chromium: cpuBetween(before, after, chromium)};
  return {...taken, span: span.slice(0, 2), wire: span[2], host: [from, to], cpu};
}

function measure(name, run, {vm, viewer}) {
  const spec = SCENARIOS[name], [from, to] = run.span, seconds = (to - from) / 1000, problems = [];
  const painted = run.frames.filter(frame => frame[0] >= from && frame[0] <= to);
  const intervals = spread(painted.slice(1).map((frame, n) => frame[0] - painted[n][0]));
  const cpuTotal = run.cpu.server + run.cpu.qemu + run.cpu.chromium;
  const metrics = {wallS: seconds, frames: painted.length, fps: painted.length / seconds,
    intervalP50: intervals.p50, intervalP95: intervals.p95, intervalP99: intervals.p99, intervalMax: intervals.max,
    over50: painted.slice(1).filter((frame, n) => frame[0] - painted[n][0] > 50).length,
    decodeP50: spread(painted.map(frame => frame[3])).p50,
    wireBytes: run.wire.bytes, mbit: run.wire.bytes * 8 / seconds / 1e6,
    frameBytes: painted.reduce((sum, frame) => sum + frame[2], 0),
    serverCpuMs: run.cpu.server, qemuCpuMs: run.cpu.qemu, qemuGuestCpuMs: run.cpu.qemuGuest, chromiumCpuMs: run.cpu.chromium,
    serverCpuPercent: run.cpu.server / seconds / 10, qemuCpuPercent: run.cpu.qemu / seconds / 10, chromiumCpuPercent: run.cpu.chromium / seconds / 10,
    cpuMsPerFrame: painted.length ? cpuTotal / painted.length : undefined,
    serverCpuMsPerFrame: painted.length ? run.cpu.server / painted.length : undefined,
    unreadable: run.frames.filter(frame => frame[4] < 0 || frame[5] < 0 || frame[6] < 0).length, clientErrors: run.errors};
  if (run.errors) problems.push(`${run.errors} client decode/paint errors`);
  if (run.sends.some(send => !send.sent)) problems.push('input sent on a closed socket');

  // Probe keys: first painted frame at or after the send whose K has reached the probe's ordinal.
  const probes = run.sends.filter(send => send.type === 'key' && typeof send.tag === 'number'), latencies = [];
  let index = 0;
  for (const probe of probes) {
    while (index < run.frames.length && !(run.frames[index][0] >= probe.at && run.frames[index][4] >= probe.tag)) index++;
    if (index === run.frames.length) break;
    latencies.push(run.frames[index][0] - probe.at);
  }
  if (probes.length) {
    const keys = spread(latencies), last = run.frames.findLast(frame => frame[4] >= 0)?.[4];
    Object.assign(metrics, {keySent: probes.length, keyObserved: latencies.length, keyMissed: probes.length - latencies.length,
      keyP50: keys.p50, keyP95: keys.p95, keyMean: keys.mean, keyMax: keys.max, keyLatencies: latencies});
    if (!latencies.length) problems.push('no probe key reached the K marker');
    else if (metrics.keyMissed) problems.push(`${metrics.keyMissed} of ${probes.length} probe keys missed`);
    if (last !== probes.length) problems.push(`guest counted ${last} of ${probes.length} probe keys`);
  }

  if (spec.marker) {
    let unique = 0, last = -1;
    for (const frame of painted) if (frame[spec.marker] >= 0 && frame[spec.marker] !== last) {unique++;last = frame[spec.marker];}
    metrics.uniqueFps = unique / seconds;
    if (unique < 2) problems.push(`${spec.marker === 5 ? 'F' : 'P'} marker did not advance`);
  }

  // Frame age: paint time minus the guest's nominal time for the animation tick on
  // screen. T lines date the guest clock; the minimum cancels serial and pipe delay.
  if (spec.marker === 5) {
    const dated = vm.ticks.filter(sample => sample.at >= run.host[0] - WARMUP_MS - 500 && sample.at <= run.host[1] + 500);
    const origin = Math.min(...dated.map(sample => sample.at - sample.tick * TICK_MS - sample.lateUs / 1000)), ages = [];
    for (const frame of painted) {
      if (frame[5] < 0 || !dated.length) continue;
      const at = frame[0] - viewer.clock.offsetMs, near = Math.round((at - origin) / TICK_MS);
      ages.push(at - origin - (frame[5] + 65536 * Math.round((near - frame[5]) / 65536)) * TICK_MS);
    }
    const age = spread(ages);
    Object.assign(metrics, {ageP50: age.p50, ageP95: age.p95, ageMax: age.max, guestClockSamples: dated.length});
    if (!ages.length) problems.push('no dated animation frame');
  }

  // Pointer: P names one position of the path (no x repeats). A frame showing position
  // k answers every unanswered send up to k, which the window has passed.
  if (spec.path) {
    const moves = run.sends.filter(send => send.type === 'pointer' && typeof send.tag === 'number');
    const byX = new Map(moves.map((move, n) => [move.x, n])), delays = [];
    let answered = -1;
    for (const frame of run.frames) {
      const shown = byX.get(frame[6]);
      if (shown === undefined || shown <= answered || frame[0] < moves[shown].at) continue;
      for (let n = answered + 1; n <= shown; n++) delays.push(frame[0] - moves[n].at);
      answered = shown;
    }
    const pointer = spread(delays);
    Object.assign(metrics, {pointerSent: moves.length, pointerObserved: delays.length, pointerMissed: moves.length - delays.length,
      pointerP50: pointer.p50, pointerP95: pointer.p95, pointerMean: pointer.mean, pointerMax: pointer.max, pointerLatencies: delays});
    if (!delays.length) problems.push('no pointer position reached the P marker');
    else if (metrics.pointerMissed) problems.push(`${metrics.pointerMissed} of ${moves.length} pointer positions missed`);
  }

  if (options['trace-refresh']) {
    const refresh = refreshSummary(vm.refresh, run.host[0], run.host[1]);
    Object.assign(metrics, {refreshChanges: refresh?.changes, refreshMeanMs: refresh?.meanMs, refreshMaxMs: refresh?.maxMs,
      refreshShareAt30: refresh?.shareAt30, refreshMs: refresh?.ms});
    if (!refresh) problems.push('QEMU logged no refresh interval');
  }
  return {metrics, problems};
}

// ---------------------------------------------------------------- run

const live = [];  // stop functions of everything started, newest last
let cleaning, interrupted = false;
const cleanup = () => cleaning ??= (async () => {
  for (const stop of live.reverse()) await Promise.race([Promise.resolve().then(stop).catch(() => {}), sleep(5000)]);
})();
const started = stop => {live.push(stop);};
const spawned = new Set();  // last resort if this process dies before cleanup() ran
process.on('exit', () => {for (const child of spawned) child.kill();});
const finished = async stop => {live.splice(live.indexOf(stop), 1);await stop();};
for (const [signal, code] of [['SIGINT', 130], ['SIGTERM', 143], ['SIGHUP', 129]]) {
  process.on(signal, () => {interrupted = true;cleanup().then(() => process.exit(code));});
}

const results = {
  command: process.argv.slice(1), startedAt: new Date().toISOString(), size: options.size, rounds, scenarios: selected,
  server: ['--max-clients', '1', '--codec', 'h264', '--h264-bitrate', '2000000', '--fps', '30', '--in-flight-frames', '8'],
  binaries, host: {kernel: release(), cpus: cpus().length, loadBefore: loadavg(), qemu: execFileSync('qemu-system-x86_64', ['--version'], {encoding: 'utf8'}).split('\n')[0]},
  runs: [], summary: {}, failures: [], warnings: [],
};

async function main() {
  const directory = mkdtempSync(join(tmpdir(), 'gliss-distribox-bench-'));
  started(() => rmSync(directory, {recursive: true, force: true}));
  const vm = await startDisplayVm({directory, width, height, traceRefresh: options['trace-refresh']});
  started(vm.stop);spawned.add(vm);
  results.guest = {mode: vm.mode, kernel: vm.kernel};
  const others = childPids(), browser = await launchBrowser({handleSIGINT: false, handleSIGTERM: false});
  started(() => browser.close());
  const chromium = childPids().filter(pid => !others.includes(pid));
  console.error(`guest ${vm.mode} on ${vm.vnc}; ${binaries.length} binaries x ${rounds} rounds x ${selected.join(',')}`);

  // One fresh server and page, as Distribox starts them per viewer.
  const session = async (binary, body) => {
    const server = await startGliss({binary: binary.path, vnc: vm.vnc});
    started(server.stop);spawned.add(server);
    const viewer = await openViewer(browser, server), closePage = () => viewer.page.close();
    started(closePage);
    await body({vm, server, viewer, chromium});
    await finished(closePage);
    await finished(server.stop);spawned.delete(server);
    if (vm.exited()) throw new Error(`QEMU exited: ${vm.log.slice(-2000)}`);
    await sleep(500);  // QEMU notices the VNC client leaving before the next one connects
  };
  // A discarded session first, so the first measured run does not pay for a cold browser.
  await session(binaries[0], () => sleep(2000));

  for (let round = 0; round < rounds; round++) {
    for (const binary of round % 2 ? [...binaries].reverse() : binaries) {
      await session(binary, async context => {
        const {viewer} = context, record = {round, label: binary.label, clock: viewer.clock, scenarios: {}};
        const where = `round ${round + 1} ${binary.label}`;
        results.runs.push(record);
        for (const name of selected) {
          let entry;
          try {
            const run = await runScenario(name, context);
            entry = {...run, ...measure(name, run, context)};
          } catch (error) {
            if (interrupted) throw error;
            entry = {metrics: {}, problems: [error.message.split('\n')[0]]};
          }
          record.scenarios[name] = entry;
          for (const problem of entry.problems) results.failures.push(`${where} ${name}: ${problem}`);
          const m = entry.metrics, show = (label, value, unit = '') => value === undefined ? '' : ` ${label} ${value.toFixed(1)}${unit}`;
          console.error(`${where} ${name}:${show('key p50', m.keyP50, ' ms')}${show('ptr p50', m.pointerP50, ' ms')}` +
            `${show('fps', m.fps)}${show('age p50', m.ageP50, ' ms')}${entry.problems.length ? ` FAILED: ${entry.problems.join('; ')}` : ''}`);
        }
        // The guest reports a scene when it ends, so one more reset flushes the last report.
        await viewer.page.evaluate(events => window.glissBench.play(events, 300), press(F(5))).catch(() => {});
        for (const [name, entry] of Object.entries(record.scenarios)) {
          if (!entry.host) continue;
          entry.guest = vm.scenes.find(scene => scene.at > entry.host[1]);
          entry.ticks = vm.ticks.filter(sample => sample.at >= entry.host[0] - WARMUP_MS - 500 && sample.at <= entry.host[1] + 500);
          entry.refresh = vm.refresh.filter(change => change.at >= entry.host[0] && change.at <= entry.host[1]);
          Object.assign(entry.metrics, {guestSkipped: entry.guest?.skipped, guestLateMaxUs: entry.guest?.late_max_us, guestFlushMaxUs: entry.guest?.flush_max_us});
          // Not failures, but anything here weakens the numbers next to it.
          if (entry.metrics.unreadable) results.warnings.push(`${where} ${name}: ${entry.metrics.unreadable} painted frames with an unreadable marker`);
          if (entry.guest?.skipped) results.warnings.push(`${where} ${name}: guest skipped ${entry.guest.skipped} animation ticks`);
          if (!entry.guest) results.warnings.push(`${where} ${name}: no guest report`);
        }
        record.clockAfter = await pageClockOffset(viewer.page).catch(() => null);
        record.pageErrors = viewer.errors;
        for (const error of viewer.errors) results.failures.push(`${where}: page error: ${error}`);
      });
    }
  }
  results.refreshTimeline = vm.refresh;
}

// Median across rounds of every numeric metric, per binary and scenario.
function summarize() {
  for (const {label} of binaries) {
    results.summary[label] = {};
    for (const name of selected) {
      const runs = results.runs.filter(run => run.label === label && run.scenarios[name]).map(run => run.scenarios[name].metrics), median = {};
      for (const metric of new Set(runs.flatMap(Object.keys))) {
        const values = runs.map(run => run[metric]).filter(value => typeof value === 'number' && Number.isFinite(value));
        if (values.length) median[metric] = spread(values).p50;
      }
      results.summary[label][name] = {rounds: runs.length, ...median, perRound: runs.map(({keyLatencies, pointerLatencies, refreshMs, ...rest}) => rest)};
    }
  }
}

function table(header, rows) {
  const widths = header.map((title, column) => Math.max(title.length, ...rows.map(row => row[column].length)));
  for (const row of [header, ...rows]) console.log(row.map((cell, column) => column ? cell.padStart(widths[column]) : cell.padEnd(widths[column])).join('  ').trimEnd());
}

function print() {
  const number = (value, digits = 1) => typeof value === 'number' ? value.toFixed(digits) : '-';
  const several = (digits, ...values) => values.every(value => value === undefined) ? '-' : values.map(value => number(value, digits)).join('/');
  const seen = (observed, sent) => sent === undefined ? '-' : `${number(observed, 0)}/${number(sent, 0)}`;
  for (const {label, path} of binaries) {
    const summary = results.summary[label];
    console.log(`\n== ${label} (${path}), medians of ${rounds} round${rounds > 1 ? 's' : ''}, ${options.size}`);
    table(['scenario', 'key ms p50/p95/mean', 'seen/sent', 'ptr ms p50/p95', 'seen/sent', 'paint fps', 'guest fps', 'interval ms p50/p95/p99/max', '>50ms', 'age ms p50/p95'],
      selected.map(name => {
        const m = summary[name];
        return [name, several(1, m.keyP50, m.keyP95, m.keyMean), seen(m.keyObserved, m.keySent), several(1, m.pointerP50, m.pointerP95),
          seen(m.pointerObserved, m.pointerSent), number(m.fps), number(m.uniqueFps), several(1, m.intervalP50, m.intervalP95, m.intervalP99, m.intervalMax),
          number(m.over50, 0), several(1, m.ageP50, m.ageP95)];
      }));
    console.log('');
    table(['scenario', 'wall s', 'frames', 'Mbit/s', 'server CPU ms', 'QEMU CPU ms (vCPU)', 'Chromium CPU ms', 'CPU ms/frame all', 'server'],
      selected.map(name => {
        const m = summary[name];
        return [name, number(m.wallS), number(m.frames, 0), number(m.mbit, 2), number(m.serverCpuMs, 0),
          `${number(m.qemuCpuMs, 0)} (${number(m.qemuGuestCpuMs, 0)})`, number(m.chromiumCpuMs, 0), number(m.cpuMsPerFrame, 2), number(m.serverCpuMsPerFrame, 2)];
      }));
    if (options['trace-refresh']) {
      console.log('\nQEMU display refresh interval (console_refresh), time at each logged interval summed over rounds');
      table(['scenario', 'changes', 'mean ms', 'at 30 ms', 'max ms', 'ms spent per interval'], selected.map(name => {
        const m = summary[name], spent = {};
        for (const run of results.runs.filter(run => run.label === label)) for (const [interval, ms] of Object.entries(run.scenarios[name]?.metrics.refreshMs ?? {})) spent[interval] = (spent[interval] ?? 0) + ms;
        return [name, number(m.refreshChanges, 0), number(m.refreshMeanMs), m.refreshShareAt30 === undefined ? '-' : `${number(m.refreshShareAt30 * 100, 0)}%`, number(m.refreshMaxMs, 0),
          Object.entries(spent).sort((a, b) => a[0] - b[0]).map(([interval, ms]) => `${interval}:${ms}`).join(' ')];
      }));
    }
    console.log('\nPer round');
    table(['scenario', 'key p50', 'key p95', 'ptr p50', 'paint fps', 'guest fps', 'age p50', 'int p95', 'Mbit/s', 'server CPU', 'QEMU CPU', 'guest late ms', 'skipped'],
      selected.map(name => {
        const runs = summary[name].perRound, each = (metric, digits = 1, scale = 1) => runs.map(run => typeof run[metric] === 'number' ? (run[metric] * scale).toFixed(digits) : '-').join(' ');
        return [name, each('keyP50'), each('keyP95'), each('pointerP50'), each('fps'), each('uniqueFps'), each('ageP50'), each('intervalP95'), each('mbit', 2),
          each('serverCpuMs', 0), each('qemuCpuMs', 0), each('guestLateMaxUs', 1, 0.001), each('guestSkipped', 0)];
      }));
  }
  if (results.warnings.length) console.log(`\nWarnings\n${results.warnings.map(warning => `  ${warning}`).join('\n')}`);
  if (results.failures.length) console.log(`\nFAILED\n${results.failures.map(failure => `  ${failure}`).join('\n')}`);
}

let code = 0;
try {
  await main();
} catch (error) {
  if (!interrupted) {console.error(error);results.failures.push(`aborted: ${error.message.split('\n')[0]}`);}
} finally {
  await cleanup();
}
if (!interrupted) {
  results.host.loadAfter = loadavg();
  summarize();
  if (options.output) writeFileSync(resolve(options.output), JSON.stringify(results));
  print();
  if (options.output) console.log(`\nraw samples: ${resolve(options.output)}`);
  code = results.failures.length ? 1 : 0;
  process.exit(code);
}
