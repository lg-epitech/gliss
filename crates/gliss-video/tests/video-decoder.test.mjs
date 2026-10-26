import assert from 'node:assert/strict';
import test from 'node:test';
import { parseVideoPacket, VideoDecoderAdapter } from '../../../web/video-decoder.js';

function packet(sequence = 1, { keyframe = true, width = 64, height = 48 } = {}) {
  const payload = keyframe
    ? [0,0,0,1,0x67,0x42,0xc0,0x1e,0,0,1,0x68,1,0,0,1,0x65,1]
    : [0,0,1,0x41,1];
  const buffer = new ArrayBuffer(24 + payload.length), view = new DataView(buffer);
  new Uint8Array(buffer).set([71,76,86,49]);
  view.setUint32(4, sequence, true); view.setUint32(8, 123, true);
  view.setUint16(12, width, true); view.setUint16(14, height, true);
  view.setUint16(18, Number(keyframe), true); view.setUint32(20, payload.length, true);
  new Uint8Array(buffer).set(payload, 24);
  return buffer;
}

class FakeFrame {
  constructor(timestamp, width = 64, height = 48) {
    this.timestamp = timestamp; this.displayWidth = width; this.displayHeight = height;
    this.closed = false;
  }
  close() { this.closed = true; }
}

class FakeDecoder {
  static supported = true;
  static instances = [];
  static async isConfigSupported(config) { return { supported: this.supported, config }; }
  constructor(callbacks) {
    this.callbacks = callbacks; this.state = 'unconfigured'; this.decodeQueueSize = 0;
    this.chunks = []; FakeDecoder.instances.push(this);
  }
  configure(config) { this.config = config; this.state = 'configured'; }
  decode(chunk) { this.chunks.push(chunk); }
  close() { this.state = 'closed'; }
  output(sequence, width = 64, height = 48) {
    const frame = new FakeFrame(sequence, width, height); this.callbacks.output(frame); return frame;
  }
}

globalThis.VideoDecoder = FakeDecoder;
globalThis.EncodedVideoChunk = class { constructor(config) { Object.assign(this, config); } };

async function submitted(decoder, bytes) {
  const promise = decoder.decode(bytes);
  await Promise.resolve(); await Promise.resolve();
  return { promise, native: decoder.decoder };
}

test('valid framing derives codec and rejects altered headers and IDR claims', () => {
  const bytes = packet(4), parsed = parseVideoPacket(bytes);
  assert.equal(parsed.codec, 'avc1.42c01e'); assert.equal(parsed.inputId, 123);
  for (const [offset, value] of [[0,0],[16,1],[18,2],[20,0],[18,0],[24,5],[29,77]]) {
    const bad = bytes.slice(0); new Uint8Array(bad)[offset] = value;
    assert.throws(() => parseVideoPacket(bad));
  }
  for (const length of [0,23,24,bytes.byteLength - 1]) assert.throws(() => parseVideoPacket(bytes.slice(0,length)));
  assert.throws(() => parseVideoPacket(packet(1, { width: 65 })));
});

test('promises resolve actual output by timestamp and caller owns frame close', async () => {
  const decoder = new VideoDecoderAdapter();
  const { promise, native } = await submitted(decoder, packet(9));
  assert.equal(native.config.hardwareAcceleration, 'prefer-software');
  assert.equal(native.config.optimizeForLatency, true); assert.equal(native.config.description, undefined);
  const videoFrame = native.output(9), result = await promise;
  assert.equal(result.sequence, 9); assert.equal(result.inputId, 123);
  assert.equal(result.videoFrame, videoFrame); assert.equal(videoFrame.closed, false);
  videoFrame.close(); decoder.close();
});

test('missing reference frames and pending queue overflow are rejected', async () => {
  const decoder = new VideoDecoderAdapter();
  await assert.rejects(decoder.decode(packet(1, { keyframe: false })), /IDR/);
  const first = await submitted(decoder, packet(1));
  const second = decoder.decode(packet(2, { keyframe: false }));
  await assert.rejects(decoder.decode(packet(3, { keyframe: false })), /queue/);
  first.native.output(1); first.native.output(2); await first.promise; await second;
  await assert.rejects(decoder.decode(packet(4, { keyframe: false })), /reference/);
  decoder.close();
});

test('resize waits for drain then reconfigures only from a keyframe', async () => {
  const decoder = new VideoDecoderAdapter();
  const first = await submitted(decoder, packet(1));
  await assert.rejects(decoder.decode(packet(2, { width: 96 })), /pending/);
  first.native.output(1); await first.promise;
  await assert.rejects(decoder.decode(packet(2, { keyframe: false, width: 96 })), /IDR/);
  const second = await submitted(decoder, packet(2, { width: 96 }));
  assert.equal(first.native.state, 'closed'); second.native.output(2, 96, 48);
  assert.equal((await second.promise).width, 96); decoder.close();
});

test('native error and wrong output dimensions reject pending work', async () => {
  for (const failure of ['error', 'size']) {
    const decoder = new VideoDecoderAdapter();
    const entry = await submitted(decoder, packet(1));
    const rejection = assert.rejects(entry.promise);
    if (failure === 'error') entry.native.callbacks.error(new Error('invalid bitstream'));
    else assert.equal(entry.native.output(1, 32, 48).closed, true);
    await rejection; assert.equal(entry.native.state, 'closed'); decoder.close();
  }
});

test('reset rejects pending work and closes late outputs', async () => {
  const decoder = new VideoDecoderAdapter();
  const entry = await submitted(decoder, packet(1));
  const rejection = assert.rejects(entry.promise, /reset/); decoder.reset(); await rejection;
  assert.equal(entry.native.output(1).closed, true); decoder.close();
});

test('no native output is bounded by a deadline', async () => {
  const decoder = new VideoDecoderAdapter({ timeoutMs: 10 });
  const entry = await submitted(decoder, packet(1));
  await assert.rejects(entry.promise, /timed out/); assert.equal(entry.native.state, 'closed'); decoder.close();
});

test('unsupported configuration gives a useful error without submitting chunks', async () => {
  FakeDecoder.supported = false;
  const decoder = new VideoDecoderAdapter();
  try { await assert.rejects(decoder.decode(packet(1)), /Unsupported/); }
  finally { FakeDecoder.supported = true; decoder.close(); }
});
