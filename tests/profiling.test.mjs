import test from 'node:test';
import assert from 'node:assert/strict';
import {StreamProfiler} from '../web/profiling.js';

function fixture(options = {}, more = {}) {
  let now = 1000, next = 0;
  const timers = new Map(), intervals = new Map(), probes = [];
  const dependencies = {codec: 'h264', now: () => now,
    setTimeout: fn => {const id = ++next;timers.set(id, fn);return id;}, clearTimeout: id => timers.delete(id),
    setInterval: fn => {const id = ++next;intervals.set(id, fn);return id;}, clearInterval: id => intervals.delete(id),
    sendProbe: id => probes.push(id), ...more};
  const profiler = new StreamProfiler(options, dependencies);
  return {profiler, probes, timers, intervals, at: value => {now = value;},
    tick: value => {now = value;for (const fn of [...intervals.values()]) fn();},
    alarm: () => {const [id, fn] = timers.entries().next().value;timers.delete(id);fn();}};
}
const frame = {received: 1010, started: 1013, decoded: 1017, paintStarted: 1025, presented: 1027, bytes: 120};
const stats = (count, total, elapsed, user, system) => ({type: 'stream_stats', generation: 1,
  counters: {encoded_frames: count, encode_ms: {count, total_ms: total, max_ms: 9}, encode_cpu_ms: {count, total_ms: total / 2, max_ms: 5}},
  process_cpu: {elapsed_ms: elapsed, user_ms: user, system_ms: system}});

test('profiling validates configuration before installing timers', () => {
  for (const options of [null, [], {durationMs: 0}, {durationMs: 60001}, {durationMs: '15000'},
    {durationMs: NaN}, {probeIntervalMs: 1}, {probeIntervalMs: Infinity}, {url: 'secret'}])
    assert.throws(() => new StreamProfiler(options, {setTimeout() {assert.fail('Invalid options installed a timer');}}));
});

test('profiling separates queue, decoder, rAF and Canvas elapsed stages', () => {
  const h = fixture();h.at(1028);h.profiler.received(120, 1010);h.profiler.frame(frame);
  const report = h.profiler.stop();
  assert.deepEqual(report.frames, [{t_ms: 27, bytes: 120, queue_ms: 3, decode_ms: 4, raf_wait_ms: 8, canvas_submit_ms: 2, receive_to_canvas_ms: 17}]);
  assert.equal(report.summary.painted_frames, 1);assert.equal(report.summary.received_media_bytes, 120);
  assert.equal(report.summary.receive_to_canvas_ms.p95, 17);
  assert.equal(report.capabilities.browser_cpu, false);assert.equal(report.capabilities.one_way_network_latency, false);
  assert.equal(report.capabilities.physical_presentation, false);
});

test('profiling preserves missing native CPU as null and computes independent counter windows', () => {
  const h = fixture();h.at(1100);h.profiler.control(stats(10, 50, 1000, 30, 15));
  assert.equal(h.profiler.snapshot().summary.process_cpu_percent_one_core, null);
  h.at(10100);h.profiler.control(stats(14, 80, 10000, 70, 20));
  const report = h.profiler.stop();
  assert.equal(report.summary.process_cpu_percent_one_core, 0.5);
  assert.equal(report.summary.process_cpu_sample_span_ms, 9000);
  assert.equal(report.summary.native.encode_ms.mean_ms, 7.5);
  assert.equal(report.summary.native.encode_cpu_ms.mean_ms, 3.75);
  assert.equal(report.summary.native.encode_ms.lifetime_max_ms, 9);
  assert.equal(report.summary.native.source_age_ms, null);
});

test('profiling rejects counter resets and nonpositive CPU windows', () => {
  const h = fixture();h.at(1100);h.profiler.control(stats(10, 50, 1000, 30, 15));
  h.at(2100);h.profiler.control(stats(1, 1, 1000, 1, 0));
  const report = h.profiler.stop();
  assert.equal(report.summary.process_cpu_percent_one_core, null);
  assert.equal(report.summary.native.encode_ms, null);
});

test('profiling exports only explicit numeric fields, never arbitrary event data', () => {
  const h = fixture({}, {codec: 'DO_NOT_EXPORT'});h.at(1100);
  const event = {...stats(10, 50, 1000, 30, 15), url: 'DO_NOT_EXPORT', cookie: 'DO_NOT_EXPORT',
    vm_id: 'DO_NOT_EXPORT', clipboard: 'DO_NOT_EXPORT', pixels: ['DO_NOT_EXPORT']};
  event.counters.untrusted = 'DO_NOT_EXPORT';event.process_cpu.command = 'DO_NOT_EXPORT';
  h.profiler.control(event);h.profiler.frame({...frame, text: 'DO_NOT_EXPORT'});
  const report = h.profiler.stop(), json = JSON.stringify(report);
  assert.equal(report.codec, null);assert.ok(!json.includes('DO_NOT_EXPORT'));
  assert.equal(Object.keys(report.server[0].process_cpu).length, 3);
});

test('profiling caps rows and export bytes and qualifies retained timing distributions', () => {
  const h = fixture({durationMs: 60000});h.at(1100);
  for (let i = 0; i < 1500; i++) h.profiler.frame(frame);
  for (let i = 0; i < 100; i++) h.profiler.control(stats(10, 50, 1000, 30, 15));
  const report = h.profiler.stop();
  assert.ok(report.frames.length <= 1024 && report.server.length <= 60);
  assert.ok(report.omitted.frames > 0 && report.omitted.server > 0);
  assert.equal(report.summary.painted_frames, 1500);
  assert.match(report.summary.frame_sample_scope, /retained prefix/);
  assert.ok(Buffer.byteLength(JSON.stringify(report)) <= 262144);
});

test('profiling stop is immutable and idempotent and clears every timer', () => {
  let stops = 0;const h = fixture({}, {onStop: () => stops++});h.at(1100);h.profiler.frame(frame);
  const report = h.profiler.stop();h.at(1200);h.profiler.frame(frame);h.profiler.control(stats(10, 50, 1000, 30, 15));
  assert.equal(h.profiler.stop(), report);assert.equal(h.profiler.snapshot(), report);assert.equal(stops, 1);
  assert.equal(h.timers.size, 0);assert.equal(h.intervals.size, 0);
  assert.throws(() => report.frames.push(frame), TypeError);
  assert.throws(() => {report.frames[0].bytes = 1000;}, TypeError);
});

test('profiling re-arms an early deadline and rejects late events despite an old supplied timestamp', () => {
  const h = fixture({durationMs: 1000});h.alarm();assert.equal(h.profiler.snapshot().state, 'recording');
  h.at(2000);h.profiler.received(120, 1100);
  const report = h.profiler.snapshot();assert.equal(report.state, 'stopped');assert.equal(report.stop_reason, 'deadline');
  assert.equal(report.duration_ms, 1000);assert.equal(report.summary.received_media_packets, 0);
});

test('profiling control probes are opt-in, bounded and correlated without exporting their IDs', () => {
  const off = fixture();off.tick(2100);assert.deepEqual(off.probes, []);off.profiler.stop();
  const h = fixture({durationMs: 60000, probeIntervalMs: 1000});h.tick(2000);
  const id = h.probes[0];h.at(2035);h.profiler.control({type: 'pong', id});
  h.profiler.control({type: 'pong', id});h.profiler.control({type: 'pong', id: 'not-ours'});
  for (let at = 3000; at < 60000; at += 1000) h.tick(at);
  const report = h.profiler.stop();
  assert.equal(report.summary.probes_sent, 5);assert.equal(report.summary.probes_received, 1);
  assert.equal(report.summary.control_round_trip_ms.p95, 35);
  assert.equal(report.summary.probes_unanswered_at_stop, 4);assert.ok(!JSON.stringify(report).includes(id));
});

test('profiling invalid frames and sampler failures do not break the client', () => {
  const h = fixture({}, {sample: () => {throw new Error('DO_NOT_EXPORT');}});h.tick(1200);
  for (const value of [null, {...frame, bytes: NaN}, {...frame, decoded: 900}, {...frame, presented: 16000}]) h.profiler.frame(value);
  assert.equal(h.profiler.stop().omitted.invalid, 5);
});

test('profiling reports page long-task wall time clipped to its absolute window', () => {
  let callback, disconnected = false;
  class Observer {constructor(fn) {callback = fn;}observe() {}takeRecords() {return [];}disconnect() {disconnected = true;}}
  const h = fixture({durationMs: 1000}, {PerformanceObserver: Observer});
  callback({getEntries: () => [{startTime: 900, duration: 200}, {startTime: 1950, duration: 200}]});
  h.at(2000);const report = h.profiler.stop();
  assert.equal(report.page_long_tasks.count, 1);assert.equal(report.page_long_tasks.total_ms, 50);assert.equal(disconnected, true);
});

test('profiling binds browser timer APIs to their global receiver', t => {
  let scheduled = 0, cleared = 0;
  for (const name of ['setTimeout', 'setInterval']) t.mock.method(globalThis, name, function () {
    assert.equal(this, globalThis);return ++scheduled;
  });
  for (const name of ['clearTimeout', 'clearInterval']) t.mock.method(globalThis, name, function () {
    assert.equal(this, globalThis);cleared++;
  });
  const profile = new StreamProfiler();profile.stop();
  assert.equal(scheduled, 2);assert.equal(cleared, 2);
});
