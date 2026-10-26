// WebSocket-shaped adapter for the existing Gliss client. Only media uses
// unreliable QUIC DATAGRAM. Input, clipboard, resize and hello remain reliable.
// GLD1 contains independently decodable GLV1 IDRs; no missing frame is retried.
const HEADER = 20, MAX_PACKET = 16 * 1024 * 1024 + 24;
const MAX_CONTROL = 1024 * 1024 + 1024;
const ASSEMBLY_TTL_MS = 250;
const OPEN = 1, CLOSED = 3;
const newer = (a, b) => ((a - b) | 0) > 0;

export class DatagramAssembler {
  constructor({ now = () => performance.now() } = {}) {
    this.now = now; this.pending = new Map(); this.last = null;
    this.stats = { fragments: 0, complete: 0, incomplete: 0, stale: 0, malformed: 0, duplicates: 0 };
  }
  expire() {
    for (const [sequence, frame] of this.pending) if (this.now() - frame.started >= ASSEMBLY_TTL_MS) {
      this.pending.delete(sequence); this.stats.incomplete++;
    }
  }
  push(bytes) {
    this.expire(); this.stats.fragments++;
    if (!(bytes instanceof Uint8Array) || bytes.byteLength < HEADER) { this.stats.malformed++; return null; }
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    const sequence = view.getUint32(4, true), total = view.getUint32(8, true);
    const index = view.getUint16(12, true), count = view.getUint16(14, true), stride = view.getUint16(16, true);
    if (view.getUint32(0, true) !== 0x31444c47 || view.getUint16(18, true) !== 0 ||
        total < 24 || total > MAX_PACKET || stride < 256 || stride > 1000 ||
        count !== Math.ceil(total / stride) || index >= count ||
        bytes.byteLength !== HEADER + Math.min(stride, total - index * stride)) {
      this.stats.malformed++; return null;
    }
    if (this.last !== null && !newer(sequence, this.last)) { this.stats.stale++; return null; }
    let frame = this.pending.get(sequence);
    if (!frame) {
      if (this.pending.size >= 2) {
        const oldest = [...this.pending.keys()].reduce((a, b) => newer(a, b) ? b : a);
        if (!newer(sequence, oldest)) { this.stats.stale++; return null; }
        this.pending.delete(oldest); this.stats.incomplete++;
      }
      frame = { total, count, stride, started: this.now(), data: new Uint8Array(total), seen: new Uint8Array(count), received: 0 };
      this.pending.set(sequence, frame);
    }
    if (frame.total !== total || frame.count !== count || frame.stride !== stride) {
      this.pending.delete(sequence); this.stats.malformed++; return null;
    }
    if (frame.seen[index]) { this.stats.duplicates++; return null; }
    frame.data.set(bytes.subarray(HEADER), index * stride); frame.seen[index] = 1; frame.received++;
    if (frame.received !== frame.count) return null;
    this.pending.delete(sequence);
    const packet = new DataView(frame.data.buffer);
    // Atomically validate inner framing before delivering any bytes downstream.
    if (packet.getUint32(0, true) !== 0x31564c47 || packet.getUint32(4, true) !== sequence ||
        packet.getUint16(18, true) !== 1 || packet.getUint32(20, true) !== total - 24) {
      this.stats.malformed++; return null;
    }
    this.last = sequence; this.stats.complete++;
    for (const [older] of this.pending) if (!newer(older, sequence)) {
      this.pending.delete(older); this.stats.incomplete++;
    }
    return frame.data.buffer;
  }
  close() { this.pending.clear(); }
}

export async function* controlMessages(readable) {
  const reader = readable.getReader(); let buffered = new Uint8Array(0);
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) return;
      if (!(value instanceof Uint8Array) || buffered.length + value.length > 2 * MAX_CONTROL + 8) throw new Error('Control receive bound exceeded');
      const next = new Uint8Array(buffered.length + value.length); next.set(buffered); next.set(value, buffered.length); buffered = next;
      while (buffered.length >= 4) {
        const length = new DataView(buffered.buffer, buffered.byteOffset, buffered.byteLength).getUint32(0, true);
        if (length < 1 || length > MAX_CONTROL) throw new Error('Invalid control length');
        if (buffered.length < length + 4) break;
        const message = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(buffered.subarray(4, length + 4)));
        buffered = buffered.slice(length + 4); yield message;
      }
    }
  } finally { reader.releaseLock(); }
}

export class DatagramSocket extends EventTarget {
  constructor({ descriptor = null } = {}) {
    super();
    this.readyState = 0; this.binaryType = 'arraybuffer'; this.transport = null;
    this.assembler = new DatagramAssembler(); this.transportStats = this.assembler.stats;
    Object.assign(this.transportStats, { presented: 0, replaced: 0, mediaBytes: 0, reliableBytes: 0 });
    this.pendingFrame = null; this.delivering = false; this.hello = false;
    this.queuedControls = 0; this.queuedControlBytes = 0; this.writes = Promise.resolve();
    this.closedOnce = false;
    this.descriptor = descriptor;
    this.expiryTimer = setInterval(() => this.assembler.expire(), ASSEMBLY_TTL_MS);
    Promise.resolve().then(() => this._connect()).catch(error => this._fail(error));
  }
  async _connect() {
    if (typeof WebTransport !== 'function') throw new Error('This browser does not support WebTransport');
    let descriptor = this.descriptor;
    if (!descriptor) {
      const response = await fetch('/api/datagram-ticket', { method: 'POST', credentials: 'same-origin', cache: 'no-store' });
      if (!response.ok) throw new Error('The datagram session was rejected');
      descriptor = await response.json();
    }
    this.descriptor = descriptor;
    if (this.closedOnce) return;
    if (descriptor.transport !== 'webtransport-datagram' || !Array.isArray(descriptor.certificate_sha256) ||
        descriptor.certificate_sha256.length !== 32 || !descriptor.certificate_sha256.every(v => Number.isInteger(v) && v >= 0 && v < 256)) {
      throw new Error('Invalid datagram descriptor');
    }
    const url = new URL(descriptor.url);
    if (url.protocol !== 'https:' || !['127.0.0.1', '[::1]', 'localhost'].includes(url.hostname) ||
        url.pathname !== '/display' || url.username || url.password || url.hash) throw new Error('Invalid datagram endpoint');
    this.transport = new WebTransport(url.href, {
      serverCertificateHashes: [{ algorithm: 'sha-256', value: new Uint8Array(descriptor.certificate_sha256) }],
      requireUnreliable: true, congestionControl: 'low-latency', allowPooling: false,
    });
    this.transport.closed.then(() => this._closed(), () => this._closed());
    await this.transport.ready;
    if (this.closedOnce) { this.transport.close(); return; }
    this._configureDatagrams();
    const stream = await this.transport.createBidirectionalStream();
    if (this.closedOnce) { this.transport.close(); return; }
    this.writer = stream.writable.getWriter();
    this.readyState = OPEN;
    // QUIC streams are opened lazily. This first reliable message lets the
    // server accept_bi() observe the stream before it sends the hello.
    this.send(JSON.stringify({ type: 'ping', id: 0 }));
    this._emit('open', new Event('open'));
    this._readControl(stream.readable).catch(error => this._fail(error));
    this._readMedia().catch(error => this._fail(error));
  }
  _configureDatagrams() {
    this.transport.datagrams.incomingMaxAge = ASSEMBLY_TTL_MS;
    // Cover one admitted 1 MiB native frame even at the 256-byte minimum
    // fragment stride; a 64-entry queue would destroy larger clean-link IDRs.
    this.transport.datagrams.incomingHighWaterMark = 4096;
  }
  async _readControl(readable) {
    for await (const message of controlMessages(readable)) {
      if (this.closedOnce) return;
      if (message.type === 'hello') {
        if (this.hello || message.transport !== 'webtransport-datagram' || message.codec !== 'h264' ||
            !message.independent_frames || message.media_retransmission !== false) throw new Error('Invalid datagram negotiation');
        this.hello = true;
      } else if (!this.hello) throw new Error('Missing datagram hello');
      this._emit('message', new MessageEvent('message', { data: JSON.stringify(message) }));
      this._deliverLatest();
    }
    this.close();
  }
  async _readMedia() {
    const reader = this.transport.datagrams.readable.getReader();
    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done || this.closedOnce) return;
        const frame = this.assembler.push(value);
        if (!frame) continue;
        this.transportStats.mediaBytes += frame.byteLength;
        if (this.pendingFrame) this.transportStats.replaced++;
        this.pendingFrame = frame; this._deliverLatest();
      }
    } finally { reader.releaseLock(); }
  }
  _deliverLatest() {
    if (this.closedOnce || !this.hello || this.delivering || !this.pendingFrame) return;
    const data = this.pendingFrame; this.pendingFrame = null; this.delivering = true;
    this._emit('message', new MessageEvent('message', { data }));
  }
  send(text) {
    if (this.readyState !== OPEN) throw new Error('Datagram session is not open');
    const message = JSON.parse(text);
    if (message.type === 'ack') {
      // A browser presentation ACK only releases the local one-frame slot. It
      // never gates native encoding or triggers media retransmission.
      this.transportStats.presented++;
      this.delivering = false; this._deliverLatest(); return;
    }
    if (message.type === 'resync') {
      // Release the local decoder slot, then request a fresh encoding of the
      // current source state. A static desktop may not emit another capture.
      this.delivering = false; this._deliverLatest();
    }
    const bytes = new TextEncoder().encode(text);
    if (bytes.length > MAX_CONTROL || this.queuedControls >= 32 || this.queuedControlBytes + bytes.length > 2 * MAX_CONTROL) {
      this._fail(new Error('Control send bound exceeded')); return;
    }
    const framed = new Uint8Array(bytes.length + 4); new DataView(framed.buffer).setUint32(0, bytes.length, true); framed.set(bytes, 4);
    this.queuedControls++; this.queuedControlBytes += bytes.length;
    this.writes = this.writes.then(async () => {
      if (this.closedOnce) return;
      await this.writer.write(framed); this.transportStats.reliableBytes += framed.byteLength;
    }).catch(error => this._fail(error)).finally(() => { this.queuedControls--; this.queuedControlBytes -= bytes.length; });
  }
  _emit(type, event) {
    this.dispatchEvent(event);
    const handler = this[`on${type}`];
    if (typeof handler === 'function') {
      // DOM event callbacks report exceptions without breaking transport I/O.
      try { handler.call(this, event); } catch (error) { queueMicrotask(() => { throw error; }); }
    }
  }
  _fail(error) {
    if (this.closedOnce) return;
    const event = new Event('error'); Object.defineProperty(event, 'error', { value: error });
    this._emit('error', event); this.close();
  }
  _closed() {
    if (this.closedOnce) return;
    this.closedOnce = true; this.readyState = CLOSED;
    clearInterval(this.expiryTimer); this.pendingFrame = null; this.assembler.close();
    this._emit('close', new Event('close'));
  }
  close() {
    if (this.closedOnce) return;
    this.transport?.close({ closeCode: 0, reason: 'display closed' }); this._closed();
  }
}
