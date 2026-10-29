// Loaded only by an explicit GlissClient.startProfiling() call.
const MAX_DURATION = 60000, MAX_FRAMES = 1024, MAX_SERVER = 60, MAX_BYTES = 262144;
const TIMINGS = ['encode_ms', 'encode_cpu_ms', 'encode_worker_wait_ms', 'source_age_ms', 'write_ms', 'enqueue_to_ack_ms', 'byte_capacity_wait_ms'];
const COUNTERS = ['encoded_frames', 'written_frames', 'acknowledged_frames', 'observed_capture_replacements', 'resyncs'];
const CODECS = ['h264', 'batch', 'global', 'global-cached', 'sparse', 'deflate', 'raw', 'rle', 'hybrid', 'hybrid-grid', 'hybrid-continuity'];
const number = value => typeof value === 'number' && Number.isFinite(value) && value >= 0 && value <= 1e15;
const integer = value => number(value) && Number.isSafeInteger(value);
const round = value => Math.round(value * 1000) / 1000;
const object = value => value !== null && typeof value === 'object' && !Array.isArray(value);
let nonce = 0;
function distribution(values) {
  if (!values.length) return null;
  const sorted = [...values].sort((a, b) => a - b);
  return {count: sorted.length, mean: round(sorted.reduce((a, b) => a + b, 0) / sorted.length),
    p50: sorted[Math.ceil(sorted.length * 0.5) - 1], p95: sorted[Math.ceil(sorted.length * 0.95) - 1], max: sorted.at(-1)};
}
function freeze(value) {
  if (value && typeof value === 'object') {for (const child of Object.values(value)) freeze(child);Object.freeze(value);}
  return value;
}
function timing(value) {
  if (!object(value) || !integer(value.count) || !number(value.total_ms) || !number(value.max_ms)) return null;
  return {count: value.count, total_ms: round(value.total_ms), max_ms: round(value.max_ms)};
}
function serverRow(message, at) {
  if (!object(message) || !integer(message.generation) || !object(message.counters)) return null;
  const row = {t_ms: round(at), generation: message.generation, counters: {}};
  for (const name of COUNTERS) if (integer(message.counters[name])) row.counters[name] = message.counters[name];
  for (const name of TIMINGS) {const value = timing(message.counters[name]);if (value) row.counters[name] = value;}
  for (const name of ['reserved_frames', 'reserved_bytes', 'waiting_encoded_bytes', 'writer_jobs'])
    if (integer(message[name])) row[name] = message[name];
  const cpu = message.process_cpu;
  if (object(cpu) && [cpu.elapsed_ms, cpu.user_ms, cpu.system_ms].every(number))
    row.process_cpu = {elapsed_ms: round(cpu.elapsed_ms), user_ms: round(cpu.user_ms), system_ms: round(cpu.system_ms)};
  return row;
}

export class StreamProfiler {
  #now; #clearTimeout; #clearInterval; #setTimeout; #sample; #send; #onStop;
  #start; #end; #deadline; #poll; #observer; #active = true; #final = null;
  #frames = []; #server = []; #rtts = []; #pings = new Map(); #prefix; #nextProbe; #probeInterval;
  #sent = 0; #received = 0; #painted = 0; #mediaBytes = 0; #mediaPackets = 0; #budget = 8192;
  #omitted = {frames: 0, server: 0, invalid: 0}; #codec;
  #longTasks = {supported: false, count: 0, total_ms: 0, max_ms: 0};
  #audio = {samples: 0, queued_ms_max: 0, pending_decodes_max: 0, dropped_start: null, dropped_end: null};
  #bufferedMax = 0;
  constructor(options = {}, adapters = {}) {
    if (!object(options) || Object.keys(options).some(key => !['durationMs', 'probeIntervalMs'].includes(key))) throw new Error('Invalid profiling options');
    const duration = options.durationMs ?? 15000, probes = options.probeIntervalMs ?? null;
    if (!Number.isInteger(duration) || duration < 1000 || duration > MAX_DURATION ||
        (probes !== null && (!Number.isInteger(probes) || probes < 1000 || probes > 10000))) throw new Error('Invalid profiling duration or probe interval');
    this.#now = adapters.now ?? (() => performance.now());
    this.#setTimeout = adapters.setTimeout ?? globalThis.setTimeout.bind(globalThis);
    this.#clearTimeout = adapters.clearTimeout ?? globalThis.clearTimeout.bind(globalThis);
    this.#clearInterval = adapters.clearInterval ?? globalThis.clearInterval.bind(globalThis);
    this.#sample = adapters.sample;this.#send = adapters.sendProbe;this.#onStop = adapters.onStop;
    this.#codec = CODECS.includes(adapters.codec) ? adapters.codec : null;
    this.#start = this.#now();if (!number(this.#start)) throw new Error('Profiling clock unavailable');
    this.#end = this.#start + duration;this.#probeInterval = probes;this.#nextProbe = this.#start + (probes ?? MAX_DURATION);
    this.#prefix = 'gliss-profile-' + (++nonce) + '-' + (globalThis.crypto?.randomUUID?.() ?? Math.random().toString(36).slice(2)) + '-';
    const Observer = adapters.PerformanceObserver ?? globalThis.PerformanceObserver;
    if (Observer) try {
      this.#observer = new Observer(list => this.#observeTasks(list.getEntries()));
      this.#observer.observe({type: 'longtask', buffered: false});this.#longTasks.supported = true;
    } catch {this.#observer?.disconnect();this.#observer = null;}
    this.#arm();
    this.#poll = (adapters.setInterval ?? globalThis.setInterval.bind(globalThis))(() => this.#tick(), 250);
  }
  #arm() {
    this.#deadline = this.#setTimeout(() => {
      if (!this.#active) return;
      if (this.#now() < this.#end) this.#arm();else this.stop('deadline');
    }, Math.max(1, this.#end - this.#now()));
  }
  #live(at = this.#now()) {
    if (!this.#active) return false;
    const current = this.#now();
    if (!number(current) || current >= this.#end) {this.stop(number(current) ? 'deadline' : 'clock');return false;}
    return number(at) && at >= this.#start && at < this.#end;
  }
  #observeTasks(entries) {
    if (!this.#active) return;
    for (const entry of entries) if (number(entry.startTime) && number(entry.duration) && entry.startTime >= this.#start && entry.startTime < this.#end) {
      const duration = Math.min(entry.duration, this.#end - entry.startTime);
      this.#longTasks.count++;this.#longTasks.total_ms += duration;this.#longTasks.max_ms = Math.max(this.#longTasks.max_ms, duration);
    }
  }
  #tick() {
    const at = this.#now();if (!this.#live(at)) return;
    try {
      const sample = this.#sample?.();
      if (object(sample?.audio)) {
        const audio = sample.audio;this.#audio.samples++;
        if (number(audio.queuedSeconds)) this.#audio.queued_ms_max = Math.max(this.#audio.queued_ms_max, audio.queuedSeconds * 1000);
        if (integer(audio.pendingDecodes)) this.#audio.pending_decodes_max = Math.max(this.#audio.pending_decodes_max, audio.pendingDecodes);
        if (integer(audio.dropped)) {this.#audio.dropped_start ??= audio.dropped;this.#audio.dropped_end = audio.dropped;}
      }
      if (number(sample?.bufferedBytes)) this.#bufferedMax = Math.max(this.#bufferedMax, sample.bufferedBytes);
    } catch {this.#omitted.invalid++;}
    if (this.#probeInterval !== null && at >= this.#nextProbe && this.#sent < 60) {
      this.#nextProbe = at + this.#probeInterval;
      if (this.#pings.size >= 4 || !this.#send) return;
      const id = this.#prefix + this.#sent;this.#pings.set(id, at);
      try {this.#send(id);this.#sent++;} catch {this.#pings.delete(id);this.#omitted.invalid++;}
    }
  }
  #store(rows, row, limit, kind) {
    const size = JSON.stringify(row).length + 1;
    if (rows.length >= limit || this.#budget + size > MAX_BYTES) {this.#omitted[kind]++;return;}
    rows.push(row);this.#budget += size;
  }
  received(bytes, at) {
    if (!this.#live(at)) return;
    if (!integer(bytes) || bytes > 134217728) {this.#omitted.invalid++;return;}
    this.#mediaPackets++;this.#mediaBytes += bytes;
  }
  frame(value) {
    if (!this.#live()) return;
    if (!object(value)) {this.#omitted.invalid++;return;}
    const {received, started, decoded, paintStarted, presented, bytes} = value;
    if (![received, started, decoded, paintStarted, presented].every(number) || received < this.#start || presented >= this.#end ||
        !(received <= started && started <= decoded && decoded <= paintStarted && paintStarted <= presented) || !integer(bytes) || bytes > 134217728) {
      this.#omitted.invalid++;return;
    }
    this.#painted++;
    this.#store(this.#frames, {t_ms: round(presented - this.#start), bytes, queue_ms: round(started - received),
      decode_ms: round(decoded - started), raf_wait_ms: round(paintStarted - decoded), canvas_submit_ms: round(presented - paintStarted),
      receive_to_canvas_ms: round(presented - received)}, MAX_FRAMES, 'frames');
  }
  control(message, at = this.#now()) {
    if (!this.#live(at) || !object(message)) return;
    if (message.type === 'pong' && this.#pings.has(message.id)) {
      this.#rtts.push(round(at - this.#pings.get(message.id)));this.#pings.delete(message.id);this.#received++;
    } else if (message.type === 'stream_stats') {
      const row = serverRow(message, at - this.#start);
      if (row) this.#store(this.#server, row, MAX_SERVER, 'server');else this.#omitted.invalid++;
    }
  }
  snapshot() {
    if (this.#final) return this.#final;
    if (!this.#live()) return this.#final;
    return this.#report('recording', null);
  }
  #report(state, reason) {
    const frames = this.#frames.map(row => ({...row})), server = this.#server.map(row => structuredClone(row));
    const cpuRows = server.filter(row => row.process_cpu), firstCpu = cpuRows[0]?.process_cpu, lastCpu = cpuRows.at(-1)?.process_cpu;
    const span = firstCpu && lastCpu ? lastCpu.elapsed_ms - firstCpu.elapsed_ms : 0;
    const cpu = span > 0 && lastCpu.user_ms >= firstCpu.user_ms && lastCpu.system_ms >= firstCpu.system_ms
      ? round((lastCpu.user_ms - firstCpu.user_ms + lastCpu.system_ms - firstCpu.system_ms) * 100 / span) : null;
    const native = {};
    for (const key of TIMINGS) {
      const first = server[0]?.counters[key], last = server.at(-1)?.counters[key];
      const count = first && last ? last.count - first.count : 0, total = first && last ? last.total_ms - first.total_ms : -1;
      native[key] = count > 0 && total >= 0 ? {count, total_ms: round(total), mean_ms: round(total / count), lifetime_max_ms: last.max_ms} : null;
    }
    const summary = {painted_frames: this.#painted, received_media_packets: this.#mediaPackets, received_media_bytes: this.#mediaBytes,
      frame_sample_scope: this.#omitted.frames ? 'retained prefix; not a whole-window distribution' : 'all observed completed frames',
      process_cpu_percent_one_core: cpu, process_cpu_sample_span_ms: span > 0 ? round(span) : null,
      process_cpu_scope: 'Native process including all sessions and threads; not encoder-only CPU', native,
      probes_sent: this.#sent, probes_received: this.#received, probes_unanswered_at_stop: this.#sent - this.#received,
      control_round_trip_ms: distribution(this.#rtts)};
    for (const key of ['queue_ms', 'decode_ms', 'raf_wait_ms', 'canvas_submit_ms', 'receive_to_canvas_ms']) summary[key] = distribution(frames.map(row => row[key]));
    const now = this.#now();
    return {schema: 'gliss-profile-v1', state, stop_reason: reason, duration_ms: round(number(now) ? Math.max(0, Math.min(now, this.#end) - this.#start) : 0), codec: this.#codec,
      limits: {duration_ms: MAX_DURATION, frame_rows: MAX_FRAMES, server_rows: MAX_SERVER, export_bytes: MAX_BYTES},
      capabilities: {native_stats: server.length > 0, native_process_cpu: cpu !== null, native_encode_thread_cpu: native.encode_cpu_ms !== null,
        long_tasks: this.#longTasks.supported, browser_cpu: false, guest_render_time: false, one_way_network_latency: false, physical_presentation: false},
      summary, frames, server, omitted: {...this.#omitted},
      page_long_tasks: {...this.#longTasks, total_ms: round(this.#longTasks.total_ms), max_ms: round(this.#longTasks.max_ms)},
      sampled_audio: {...this.#audio, queued_ms_max: round(this.#audio.queued_ms_max)}, sampled_socket_buffered_bytes_max: this.#bufferedMax,
      interpretation: 'Browser timings are local elapsed times through Canvas submission. Native times use a separate clock. Control RTT includes both directions, relay and event-loop time. Native timing fields overlap; do not add them or subtract clocks. Audio and buffer peaks are sampled at 250 ms. Unanswered probes are not packet-loss counts.'};
  }
  stop(reason = 'manual') {
    if (this.#final) return this.#final;
    if (!this.#active) return null;
    const normalized = ['manual', 'deadline', 'disconnect', 'clock'].includes(reason) ? reason : 'manual';
    this.#observeTasks(this.#observer?.takeRecords?.() ?? []);
    this.#active = false;this.#clearTimeout(this.#deadline);this.#clearInterval(this.#poll);this.#observer?.disconnect();this.#pings.clear();
    this.#final = freeze(this.#report('stopped', normalized));
    try {this.#onStop?.();} catch { /* Profiling cleanup cannot break the desktop. */ }
    return this.#final;
  }
  toJSON() {return this.snapshot();}
}
