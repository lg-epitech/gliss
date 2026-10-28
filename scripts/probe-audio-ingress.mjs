// Owned-guest ingress reference. Does not run a browser, encoder or existing VM.
import assert from 'node:assert/strict';
import {readFile, writeFile, lstat, readlink, open} from 'node:fs/promises';
import {existsSync} from 'node:fs';
import {dirname, isAbsolute, join} from 'node:path';
import {getPriority} from 'node:os';
import {spawn} from 'node:child_process';
import {createHash} from 'node:crypto';
import {startAudioMotionVm} from '../test-support/qemu-audio-motion/fixture-budget.mjs';

assert.equal(process.argv.length, 4);assert.equal(process.argv[2], '--output');
const output = process.argv[3], directory = dirname(output), binary = process.env.GLISS_INGRESS_BINARY;
const hash = bytes => createHash('sha256').update(bytes).digest('hex');
assert.ok(binary && isAbsolute(binary) && isAbsolute(output));assert.equal(getPriority(), 19);
assert.equal((await readFile('/proc/self/status', 'utf8')).match(/^Cpus_allowed_list:\s*(.+)$/m)?.[1], '0');
assert.ok(!['/dev/dri', '/dev/nvidiactl', '/dev/kfd'].some(existsSync));
assert.match(process.env.GLISS_AUDIO_HOST_NETNS ?? '', /^net:\[[0-9]+\]$/);
assert.notEqual(await readlink('/proc/self/ns/net'), process.env.GLISS_AUDIO_HOST_NETNS);
const fresh = async path => {try {await lstat(path);throw new Error('Output already exists');} catch (e) {if (e.code !== 'ENOENT') throw e;}};
await fresh(output);
const planName = 'benchmarks/audio-ingress-reference/plan.json', planRaw = await readFile(planName), plan = JSON.parse(planRaw);
assert.deepEqual(plan.motion_order, [false, true, true, false]);assert.equal(plan.window_ms, 6000);
const result = {schema: 'gliss-audio-ingress-cohort-v1', plan_sha256: hash(planRaw),
  binary_sha256: hash(await readFile(binary)), sources: {}, positions: [], cleanup_complete: false};
for (const name of ['scripts/probe-audio-ingress.mjs', planName, 'crates/gliss-server/examples/audio-ingress-probe.rs',
  'crates/gliss-rfb/src/audio.rs', 'crates/gliss-rfb/src/lib.rs', 'crates/gliss-server/src/audio.rs',
  'test-support/qemu-audio-motion/fixture-budget.mjs', 'test-support/qemu-audio-motion/build.py',
  'test-support/qemu-audio-motion/init.c', 'test-support/qemu-audio-marker/build.py',
  'test-support/qemu-audio-marker/init.c', 'tests/helpers.mjs', 'Cargo.lock'])
  result.sources[name] = hash(await readFile(name));
for (const motion of plan.motion_order) {
  const slot = result.positions.length + 1, file = `native-${slot}.json`;await fresh(join(directory, file));
  result.positions.push({slot, motion, file, status: 'blocked'});
}
const callbacks = [];let vm;
async function persist() {
  const file = await open(output, 'w');
  try {await file.writeFile(JSON.stringify(result, null, 2) + '\n');await file.sync();} finally {await file.close();}
}
async function capture(endpoint, file) {
  const child = spawn(binary, [endpoint, file], {stdio: ['ignore', 'pipe', 'pipe']});
  let log = '', failure, timer;
  const terminal = new Promise(resolve => {child.once('exit', (code, signal) => resolve({code, signal}));child.once('error', error => {failure = error;resolve({code: null, signal: null});});});
  for (const stream of [child.stdout, child.stderr]) stream.on('data', bytes => {log = (log + bytes).slice(-8192);});
  try {
    timer = setTimeout(() => {if (child.exitCode === null && child.signalCode === null) child.kill('SIGTERM');}, 20000);
    const allowed = (await readFile(`/proc/${child.pid}/status`, 'utf8')).match(/^Cpus_allowed_list:\s*(.+)$/m)?.[1];assert.equal(allowed, '0');
    const exit = await terminal;if (failure) throw failure;
    assert.deepEqual(exit, {code: 0, signal: null});return {exit, log_sha256: hash(log), log_bytes: Buffer.byteLength(log)};
  } finally {
    clearTimeout(timer);
    if (child.exitCode === null && child.signalCode === null && child.pid) {
      child.kill('SIGKILL');await terminal;
    }
  }
}
try {
  await persist();
  vm = await startAudioMotionVm({after: callback => callbacks.push(callback)}, {cpu: 19, mixFrequency: 48000});
  assert.equal(getPriority(vm.child.pid), 19);result.guest_build = vm.buildIdentity;result.audio_device = vm.audioDevice;
  assert.equal((await readFile(`/proc/${vm.child.pid}/status`, 'utf8')).match(/^Cpus_allowed_list:\s*(.+)$/m)?.[1], '19');
  for (const position of result.positions) {
    await vm.setMotion(position.motion);
    position.native_exit = await capture(vm.vnc, join(directory, position.file));
    const raw = await readFile(join(directory, position.file)), report = JSON.parse(raw);
    position.report_sha256 = hash(raw);position.report_bytes = raw.length;
    position.measurement_valid = report.window_ms === 6000 && report.elapsed_ms >= 6000 && report.elapsed_ms <= 6100 &&
      report.events.length > 0 && report.events.length <= 2048 && report.raw_bytes <= 2097152 &&
      report.raw_lagged_messages === 0 && report.audio_disabled === true;
    position.status = 'measured';await persist();
    console.log(JSON.stringify({slot: position.slot, motion: position.motion, valid: position.measurement_valid,
      chunks: report.events.length, raw_bytes: report.raw_bytes,
      models: report.models.map(m => ({frames: m.packet_frames, packets: m.completed,
        max_assembly_age_ms: Math.max(...m.assembly_age_ms), discarded_bytes: m.discarded_bytes}))}));
    assert.ok(position.measurement_valid, 'Ingress reference invalid');
  }
} catch (error) {result.failure = error.name;process.exitCode = 1;}
finally {
  let clean = true;
  for (const close of callbacks.reverse()) try {await close();} catch {clean = false;}
  result.cleanup_complete = clean;
  if (!clean) process.exitCode = 2;
  await persist();
}
