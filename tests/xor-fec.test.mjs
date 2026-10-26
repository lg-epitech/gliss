import test from "node:test";
import assert from "node:assert/strict";
import { PredictiveDatagramSocket } from "../web/predictive-datagram.js";
import { PredictiveReceiver } from "../web/predictive-state.js";
import { FecPredictiveReceiver } from "../web/fec-predictive-state.js";
import {
  LIMITS,
  MAGIC,
  XorFecReceiver,
  parseDatagram,
  parityForGroup,
} from "../web/xor-fec.js";

function au(sequence, keyframe = true) {
  const bytes = new Uint8Array(724),
    view = new DataView(bytes.buffer);
  bytes.set([71, 76, 86, 49]);
  view.setUint32(4, sequence, true);
  view.setUint16(12, 320, true);
  view.setUint16(14, 192, true);
  view.setUint16(18, Number(keyframe), true);
  view.setUint32(20, 700, true);
  bytes.fill(0x55, 24);
  if (keyframe)
    bytes.set(
      [0, 0, 0, 1, 0x67, 66, 0, 30, 0, 0, 0, 1, 0x68, 0x55, 0, 0, 0, 1, 0x65],
      24,
    );
  else bytes.set([0, 0, 0, 1, 0x41], 24);
  return bytes.buffer;
}
function fragment(buffer, stride = 256) {
  const source = new Uint8Array(buffer),
    sequence = new DataView(buffer).getUint32(4, true),
    count = Math.ceil(source.length / stride);
  return Array.from({ length: count }, (_, index) => {
    const body = source.subarray(index * stride, (index + 1) * stride),
      bytes = new Uint8Array(20 + body.length),
      view = new DataView(bytes.buffer);
    view.setUint32(0, MAGIC.data, true);
    view.setUint32(4, sequence, true);
    view.setUint32(8, source.length, true);
    view.setUint16(12, index, true);
    view.setUint16(14, count, true);
    view.setUint16(16, stride, true);
    bytes.set(body, 20);
    return bytes;
  });
}
function syntheticFragments(
  sequence,
  count,
  { stride = 256, last = stride } = {},
) {
  const total = (count - 1) * stride + last;
  return Array.from({ length: count }, (_, index) => {
    const length = index === count - 1 ? last : stride,
      bytes = new Uint8Array(20 + length),
      view = new DataView(bytes.buffer);
    view.setUint32(0, MAGIC.data, true);
    view.setUint32(4, sequence, true);
    view.setUint32(8, total, true);
    view.setUint16(12, index, true);
    view.setUint16(14, count, true);
    view.setUint16(16, stride, true);
    for (let at = 20; at < bytes.length; at++)
      bytes[at] = (sequence + index * 37 + at * 19) & 255;
    return bytes;
  });
}
function parity(parts) {
  return parityForGroup(parts);
}
function feed(receiver, buffer, now = 0, drop = []) {
  const parts = fragment(buffer);
  for (let index = 0; index < parts.length; index++)
    if (!drop.includes(index)) receiver.receive(parts[index], now);
  return parts;
}
function complete(receiver, now = 0) {
  const next = receiver.next(now);
  assert.ok(next);
  receiver.complete(next.token, true, now);
  return next;
}

test("canonical GLP1 geometry covers full and every final group cardinality", () => {
  for (let final = 1; final <= 8; final++)
    for (const last of [1, 255, 256]) {
      const data = syntheticFragments(100 + final, 8 + final, { last });
      for (let first = 0; first < data.length; first += 8) {
        const packet = parseDatagram(parity(data.slice(first, first + 8)));
        assert.equal(packet.kind, "parity");
        assert.equal(packet.index, first);
        assert.equal(
          packet.body.byteLength,
          first + 8 < data.length ? 256 : final === 1 ? last : 256,
        );
      }
    }
  const data = syntheticFragments(200, 9),
    bad = parity(data.slice(0, 8)).slice();
  bad[18] = 7;
  assert.throws(() => parseDatagram(bad));
  assert.throws(() => parity(data.slice(0, 7)));
});

test("every single IDR and P fragment erasure reconstructs exact GOP order", () => {
  const chain = [au(1), au(2, false), au(3, false), au(4, false)];
  for (let frame = 0; frame < chain.length; frame++)
    for (let missing = 0; missing < 3; missing++) {
      const receiver = new FecPredictiveReceiver();
      for (let at = 0; at < chain.length; at++) {
        const parts = feed(
          receiver,
          chain[at],
          0,
          at === frame ? [missing] : [],
        );
        receiver.receive(parity(parts), 0);
        const selected = complete(receiver);
        assert.equal(selected.sequence, at + 1);
        assert.deepEqual(
          new Uint8Array(selected.buffer),
          new Uint8Array(chain[at]),
        );
      }
      assert.equal(receiver.stats.recoveredFragments, 1);
      assert.equal(receiver.acceptedBytes, 4 * 724);
      receiver.close();
    }
});

test("loss from a receive call retires both stages before its forward output", () => {
  const receiver = new FecPredictiveReceiver();
  for (const sequence of [1, 2, 3])
    receiver.receive(fragment(au(sequence))[0], 0);
  const snapshot = receiver.snapshot();
  assert.equal(receiver.stats.forwardedDataDatagrams, 2);
  assert.equal(snapshot.assembly.partialFrames, 0);
  assert.equal(snapshot.fec.gauges.partial_access_units, 0);
  assert.equal(snapshot.reference.knownLoss, 1);
});

function dependencyBoundary() {
  const receiver = new FecPredictiveReceiver();
  feed(receiver, au(1));
  complete(receiver);
  const missing = feed(receiver, au(2, false), 1, [0]);
  feed(receiver, au(3, false), 1);
  assert.equal(receiver.next(1), null);
  return { receiver, missing };
}
test("repair received at the nominal 8 ms fence before pump remains selectable", () => {
  const { receiver, missing } = dependencyBoundary();
  assert.equal(receiver.receive(parity(missing), 9), true);
  assert.equal(receiver.next(9).sequence, 2);
});
test("pump at the nominal 8 ms fence retires a later repair", () => {
  const { receiver, missing } = dependencyBoundary();
  assert.equal(receiver.next(9), null);
  assert.equal(receiver.receive(parity(missing), 9), false);
  const snapshot = receiver.snapshot();
  assert.equal(snapshot.reference.state, "waiting-for-idr");
  assert.equal(snapshot.assembly.partialFrames, 0);
  assert.equal(snapshot.fec.gauges.partial_access_units, 0);
});

test("250 ms first-arrival expiry retires assembly and late history cannot reopen it", () => {
  const receiver = new FecPredictiveReceiver(),
    parts = fragment(au(1));
  receiver.receive(parity(parts), 0);
  receiver.receive(parts[0], 100);
  assert.equal(receiver.snapshot().assembly.partialFrames, 1);
  assert.equal(receiver.next(250), null);
  for (const bytes of parts) receiver.receive(bytes, 251);
  const snapshot = receiver.snapshot();
  assert.equal(snapshot.reference.offered, 0);
  assert.equal(snapshot.assembly.partialFrames, 0);
  assert.equal(snapshot.fec.gauges.partial_access_units, 0);
});

test("one input can forward original plus repair while accepted bytes count one AU", () => {
  const receiver = new FecPredictiveReceiver(),
    parts = fragment(au(7)),
    p = parity(parts);
  receiver.receive(p, 0);
  receiver.receive(parts[0], 0);
  const before = receiver.acceptedBytes;
  assert.equal(receiver.receive(parts[1], 0), true);
  assert.equal(receiver.acceptedBytes - before, 724);
  assert.equal(receiver.stats.acceptedAccessUnitBytes, 724);
  assert.equal(receiver.stats.forwardedDataDatagrams, 3);
  assert.equal(receiver.stats.assemblyArrayAllocatedBytes, 724 + 3);
  assert.equal(receiver.stats.gateArrayAllocatedBytes, 724);
  assert.equal(
    receiver.stats.receivedDataBytes,
    parts[0].byteLength + parts[1].byteLength,
  );
  assert.equal(receiver.stats.receivedParityBytes, p.byteLength);
  assert.equal(receiver.snapshot().assembly.partialFrames, 0);
  assert.equal(receiver.snapshot().fec.gauges.partial_access_units, 0);
});

test("generation floor retires late history in u32 wrap order", () => {
  const receiver = new FecPredictiveReceiver();
  receiver.receive(fragment(au(0xffffffff))[0], 0);
  receiver.next(250);
  feed(receiver, au(0), 251);
  assert.equal(receiver.next(251).sequence, 0);
  receiver.receive(fragment(au(0xffffffff))[1], 252);
  assert.equal(receiver.stats.retiredArrivals, 1);
});

test("exact token, close and fresh epoch remain isolated", () => {
  const old = new FecPredictiveReceiver();
  feed(old, au(9));
  const selected = old.next(0);
  assert.throws(() => old.complete({ ...selected.token }, true, 0));
  old.receive(fragment(au(10, false))[0], 1);
  old.close();
  assert.equal(old.snapshot().memory.reachableTypedArrayBytes, 0);
  assert.equal(old.snapshot().reference.activeSequence, null);
  assert.throws(() => old.receive(fragment(au(9))[0], 2));
  const fresh = new FecPredictiveReceiver();
  feed(fresh, au(9));
  assert.equal(fresh.next(0).sequence, 9);
});

function mediaReader(bytes) {
  let delivered = false;
  return {
    async read() {
      if (delivered) return { done: true };
      delivered = true;
      return { done: false, value: bytes };
    },
    releaseLock() {},
  };
}
function mediaSequence(values) {
  let index = 0;
  return {
    async read() {
      return index < values.length
        ? { done: false, value: values[index++] }
        : { done: true };
    },
    releaseLock() {},
  };
}
test("socket media bytes use the admitted AU delta while wire scopes stay separate", async () => {
  const parts = fragment(au(12)),
    p = parity(parts),
    receiver = new FecPredictiveReceiver();
  const socket = Object.create(PredictiveDatagramSocket.prototype),
    transportStats = {
      receivedDatagramBytes: 0,
      mediaBytes: 0,
      dataDatagrams: 0,
      dataDatagramBytes: 0,
      parityDatagrams: 0,
      parityDatagramBytes: 0,
      forwardedDataDatagrams: 0,
      forwardedDataDatagramBytes: 0,
      reconstructedFragments: 0,
    };
  Object.assign(socket, {
    closedOnce: false,
    fecEnabled: true,
    receiver,
    transport: {
      datagrams: {
        readable: { getReader: () => mediaSequence([p, parts[0], parts[1]]) },
      },
    },
    transportStats,
    _pump: () => {},
  });
  await socket._readMedia();
  assert.equal(transportStats.mediaBytes, 724);
  assert.equal(
    transportStats.receivedDatagramBytes,
    p.byteLength + parts[0].byteLength + parts[1].byteLength,
  );
  assert.equal(
    transportStats.dataDatagramBytes,
    parts[0].byteLength + parts[1].byteLength,
  );
  assert.equal(transportStats.parityDatagramBytes, p.byteLength);
  assert.equal(transportStats.reconstructedFragments, 1);
});
test("off-profile receiver behavior stays direct and rejects canonical or malformed GLP1 on the socket", async () => {
  const plain = new PredictiveReceiver(),
    parts = fragment(au(1));
  for (const bytes of parts) plain.receive(bytes, 0);
  assert.equal(plain.acceptedBytes, 724);
  assert.equal(plain.next(0).sequence, 1);
  const canonical = parity(parts),
    malformed = canonical.slice();
  malformed[18] = 2;
  for (const bytes of [canonical, malformed]) {
    const socket = Object.create(PredictiveDatagramSocket.prototype),
      receiver = new PredictiveReceiver();
    Object.assign(socket, {
      closedOnce: false,
      fecEnabled: false,
      receiver,
      transport: {
        datagrams: { readable: { getReader: () => mediaReader(bytes) } },
      },
      transportStats: { receivedDatagramBytes: 0, mediaBytes: 0 },
      _pump: () => {},
    });
    await assert.rejects(
      () => socket._readMedia(),
      /GLP1 requires negotiated predictive FEC/,
    );
  }
});

test("XOR receiver caps reachable arrays and clears them on close", () => {
  const stride = 1000,
    total = LIMITS.maxAccessUnitBytes,
    count = Math.ceil(total / stride),
    last = total - (count - 1) * stride;
  const receiver = new XorFecReceiver();
  receiver.receive(syntheticFragments(50, count, { stride, last })[0], 0);
  assert.ok(receiver.knownBytes() <= LIMITS.maxKnownRetainedTypedArrayBytes);
  assert.ok(
    receiver.snapshot().counters.typed_array_bytes_allocated_total >=
      receiver.knownBytes(),
  );
  receiver.close();
  assert.equal(receiver.knownBytes(), 0);
  assert.throws(() => receiver.expire(1));
});
