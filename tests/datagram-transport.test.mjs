import assert from 'node:assert/strict';
import test from 'node:test';
import { DatagramAssembler } from '../web/datagram-transport.js';

function frame(sequence, payload = 2400) {
  const data = new Uint8Array(payload + 24), view = new DataView(data.buffer);
  data.set([71, 76, 86, 49]); view.setUint32(4, sequence, true);
  view.setUint16(12, 1280, true); view.setUint16(14, 800, true);
  view.setUint16(18, 1, true); view.setUint32(20, payload, true);
  for (let n = 24; n < data.length; n++) data[n] = (n + sequence) & 255;
  return data;
}
function fragments(data, sequence, stride = 1000) {
  const count = Math.ceil(data.length / stride);
  return Array.from({ length: count }, (_, index) => {
    const payload = data.subarray(index * stride, (index + 1) * stride);
    const fragment = new Uint8Array(20 + payload.length), view = new DataView(fragment.buffer);
    fragment.set([71, 76, 68, 49]); view.setUint32(4, sequence, true); view.setUint32(8, data.length, true);
    view.setUint16(12, index, true); view.setUint16(14, count, true); view.setUint16(16, stride, true);
    fragment.set(payload, 20); return fragment;
  });
}

test('reordered and duplicated fragments produce exactly one complete frame', () => {
  const assembler = new DatagramAssembler(), expected = frame(12), chunks = fragments(expected, 12);
  assert.equal(assembler.push(chunks[2]), null); assert.equal(assembler.push(chunks[2]), null);
  assert.equal(assembler.push(chunks[0]), null);
  assert.deepEqual(new Uint8Array(assembler.push(chunks[1])), expected);
  assert.equal(assembler.push(chunks[0]), null);
  assert.deepEqual({ complete: assembler.stats.complete, duplicates: assembler.stats.duplicates, stale: assembler.stats.stale }, { complete: 1, duplicates: 1, stale: 1 });
});

test('missing video is discarded while a later independent frame progresses', () => {
  let current = 0; const assembler = new DatagramAssembler({ now: () => current });
  const lost = fragments(frame(40), 40), next = fragments(frame(41), 41);
  assert.equal(assembler.push(lost[0]), null); current = 300; assembler.expire();
  assert.equal(assembler.pending.size, 0); assert.equal(assembler.stats.incomplete, 1);
  for (const fragment of next.slice(0, -1)) assert.equal(assembler.push(fragment), null);
  assert.deepEqual(new Uint8Array(assembler.push(next.at(-1))), frame(41));
  for (const fragment of lost.slice(1)) assert.equal(assembler.push(fragment), null);
  assert.equal(assembler.stats.complete, 1);
});

test('only two incomplete frames are retained and delayed old frames cannot evict live data', () => {
  const assembler = new DatagramAssembler();
  for (const sequence of [20, 21, 22]) assembler.push(fragments(frame(sequence), sequence)[0]);
  assert.deepEqual([...assembler.pending.keys()], [21, 22]);
  assembler.push(fragments(frame(19), 19)[0]);
  assert.deepEqual([...assembler.pending.keys()], [21, 22]);
  assert.equal(assembler.stats.incomplete, 1); assert.equal(assembler.stats.stale, 1);
});

test('sequence wrap permits new frames and rejects pre-wrap late media', () => {
  const assembler = new DatagramAssembler();
  for (const sequence of [0xfffffffe, 0xffffffff, 0, 1]) {
    const expected = frame(sequence, 1), [fragment] = fragments(expected, sequence);
    assert.deepEqual(new Uint8Array(assembler.push(fragment)), expected);
  }
  assert.equal(assembler.push(fragments(frame(0xfffffffe, 1), 0xfffffffe)[0]), null);
  assert.equal(assembler.stats.complete, 4); assert.equal(assembler.stats.stale, 1);
});

test('malformed length, dependent media and conflicting fragment headers never emit a partial frame', () => {
  const assembler = new DatagramAssembler();
  const invalid = fragments(frame(10), 10)[0]; new DataView(invalid.buffer).setUint32(8, 0x7fffffff, true);
  assert.equal(assembler.push(invalid), null); assert.equal(assembler.pending.size, 0);
  const dependent = frame(11, 1); new DataView(dependent.buffer).setUint16(18, 0, true);
  assert.equal(assembler.push(fragments(dependent, 11)[0]), null);
  const conflict = fragments(frame(12), 12); assembler.push(conflict[0]);
  const other = fragments(frame(12, 2500), 12)[1]; assert.equal(assembler.push(other), null);
  assert.equal(assembler.pending.size, 0); assert.equal(assembler.stats.malformed, 3);
  assert.equal(assembler.stats.complete, 0);
});

test('socket facade dispatches real message events to property and registered listeners exactly once', async () => {
  const { DatagramSocket } = await import('../web/datagram-transport.js');
  class OfflineSocket extends DatagramSocket { async _connect() {} }
  const socket = new OfflineSocket();
  try {
    const received = [], removed = () => assert.fail('removed listener ran');
    socket.addEventListener('message', event => received.push(['listener', event.data, event.target === socket]));
    socket.addEventListener('message', removed); socket.removeEventListener('message', removed);
    let once = 0; socket.addEventListener('message', () => once++, { once: true });
    socket.onmessage = function(event) { received.push(['property', event.data, this === socket]); };
    socket._emit('message', new MessageEvent('message', { data: 'hello' }));
    socket._emit('message', new MessageEvent('message', { data: 'stats' }));
    assert.deepEqual(received, [
      ['listener', 'hello', true], ['property', 'hello', true],
      ['listener', 'stats', true], ['property', 'stats', true],
    ]);
    assert.equal(once, 1);
    let closed = 0, propertyClosed = 0;
    socket.addEventListener('close', () => closed++); socket.onclose = () => propertyClosed++;
    socket.close(); socket.close();
    assert.equal(closed, 1); assert.equal(propertyClosed, 1);
  } finally { socket.close(); }
});

test('disconnect while the reliable stream opens cannot reopen the socket', async () => {
  const { DatagramSocket } = await import('../web/datagram-transport.js');
  const oldFetch = globalThis.fetch, oldWebTransport = globalThis.WebTransport;
  let resolveStream, streamStarted;
  const started = new Promise(resolve => { streamStarted = resolve; });
  const opening = new Promise(resolve => { resolveStream = resolve; });
  let transportClosed = 0, opened = 0;
  globalThis.fetch = async () => ({ ok: true, json: async () => ({
    transport: 'webtransport-datagram', url: 'https://127.0.0.1:4443/display?ticket=test', certificate_sha256: Array(32).fill(0),
  }) });
  globalThis.WebTransport = class {
    constructor() { this.ready = Promise.resolve(); this.closed = new Promise(() => {}); this.datagrams = {}; }
    createBidirectionalStream() { streamStarted(); return opening; }
    close() { transportClosed++; }
  };
  const socket = new DatagramSocket(); socket.onopen = () => opened++;
  try {
    await started; socket.close();
    resolveStream({ writable: { getWriter() { assert.fail('closed socket acquired a writer'); } } });
    await new Promise(resolve => queueMicrotask(resolve));
    await new Promise(resolve => queueMicrotask(resolve));
    assert.equal(socket.readyState, 3); assert.equal(opened, 0); assert.ok(transportClosed >= 1);
  } finally {
    socket.close(); globalThis.fetch = oldFetch;
    if (oldWebTransport === undefined) delete globalThis.WebTransport; else globalThis.WebTransport = oldWebTransport;
  }
});

test('decoder recovery releases the local slot and reliably requests current source state', async () => {
  const { DatagramSocket } = await import('../web/datagram-transport.js');
  class OfflineSocket extends DatagramSocket { async _connect() {} }
  const socket = new OfflineSocket(), written = [], delivered = [];
  try {
    socket.readyState = 1; socket.hello = true; socket.delivering = true;
    socket.pendingFrame = frame(120, 1).buffer;
    socket.writer = { async write(value) { written.push(value); } };
    socket.onmessage = event => delivered.push(event.data);
    socket.send(JSON.stringify({ type: 'resync' }));
    await socket.writes;
    assert.equal(delivered.length, 1); assert.equal(socket.pendingFrame, null);
    assert.equal(written.length, 1);
    const bytes = written[0], length = new DataView(bytes.buffer).getUint32(0, true);
    assert.equal(length, bytes.length - 4);
    assert.deepEqual(JSON.parse(new TextDecoder().decode(bytes.subarray(4))), { type: 'resync' });
    socket.send(JSON.stringify({ type: 'ack', sequence: 120 }));
    await socket.writes;
    assert.equal(written.length, 1, 'presentation ACK never travels to the server or gates encoding');
  } finally { socket.close(); }
});
