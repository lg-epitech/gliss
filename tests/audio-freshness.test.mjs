import test from 'node:test';
import assert from 'node:assert/strict';
import {AudioPlayer} from '../web/audio-player.js';

function packet(sequence) {
  const data = new ArrayBuffer(21), header = new DataView(data);
  new Uint8Array(data, 0, 4).set([71, 76, 65, 49]);
  header.setUint8(4, 1);header.setUint8(5, 2);
  header.setUint32(8, 48000, true);header.setUint32(12, sequence, true);
  header.setUint32(16, 960, true);
  return data;
}

function harness(t) {
  let now = 0;
  const decoders = [], starts = [];
  const originalDecoder = Object.getOwnPropertyDescriptor(globalThis, 'AudioDecoder');
  const originalChunk = Object.getOwnPropertyDescriptor(globalThis, 'EncodedAudioChunk');
  t.mock.method(performance, 'now', () => now);
  globalThis.AudioDecoder = class {
    constructor(callbacks) {this.callbacks = callbacks;this.state = 'unconfigured';this.decodeQueueSize = 0;this.chunks = [];decoders.push(this);}
    configure() {this.state = 'configured';}
    decode(chunk) {this.chunks.push(chunk);}
    close() {this.state = 'closed';}
  };
  globalThis.EncodedAudioChunk = class {constructor(init) {Object.assign(this, init);}};
  t.after(() => {
    if (originalDecoder) Object.defineProperty(globalThis, 'AudioDecoder', originalDecoder);else delete globalThis.AudioDecoder;
    if (originalChunk) Object.defineProperty(globalThis, 'EncodedAudioChunk', originalChunk);else delete globalThis.EncodedAudioChunk;
  });
  const context = {
    state: 'running', currentTime: 1, destination: {},
    createBuffer: () => ({copyToChannel() {}}),
    createBufferSource: () => ({connect() {}, disconnect() {}, stop() {}, start(at) {starts.push(at);}}),
    close() {this.state = 'closed';return Promise.resolve();},
  };
  const player = new AudioPlayer();
  player.context = context;player.enabled = true;player.codec = 'opus';player.createDecoder();
  t.after(() => player.dispose());
  return {player, decoders, starts,
    advance(ms) {now += ms;context.currentTime += ms / 1000;},
    output(decoder = decoders.at(-1), chunk = decoder.chunks.at(-1)) {
      const audio = {sampleRate: 48000, numberOfChannels: 2, numberOfFrames: 960,
        timestamp: chunk.timestamp, closed: false, copied: 0,
        copyTo(samples) {this.copied++;samples.fill(0.25);},close() {this.closed = true;}};
      decoder.callbacks.output(audio);
      return audio;
    },
  };
}

test('audio: an Opus output delayed 250 ms is closed without copying or scheduling stale sound', t => {
  const h = harness(t);h.player.push(packet(0));h.advance(250);
  const audio = h.output();
  assert.equal(audio.closed, true);
  assert.equal(h.player.scheduled, 0);
  assert.equal(audio.copied, 0);
  assert.equal(h.starts.length, 0);
  assert.equal(h.player.dropped, 1);
});

test('audio: an 80 ms decoder delay retains the existing startup jitter buffer', t => {
  const h = harness(t);h.player.push(packet(0));h.advance(80);
  const audio = h.output();
  assert.equal(audio.closed, true);assert.equal(audio.copied, 2);
  assert.equal(h.player.scheduled, 1);assert.equal(h.player.dropped, 0);
  assert.ok(Math.abs(h.starts[0] - 1.14) < 1e-9);
});

test('audio: pending output remains bounded even when decodeQueueSize reports zero', t => {
  const h = harness(t);
  for (let sequence = 0; sequence < 9; sequence++) {h.player.push(packet(sequence));h.advance(20);}
  assert.equal(h.decoders.length, 2);
  assert.equal(h.decoders[0].state, 'closed');
  const old = h.output(h.decoders[0], h.decoders[0].chunks[0]);
  assert.equal(old.closed, true);assert.equal(old.copied, 0);assert.equal(h.starts.length, 0);
  const fresh = h.output();
  assert.equal(fresh.closed, true);assert.equal(h.player.scheduled, 1);
});

test('audio: mute discards decoder work and late output without creating a new source', t => {
  const h = harness(t);h.player.push(packet(0));h.player.stop();
  const audio = h.output();
  assert.equal(audio.closed, true);assert.equal(audio.copied, 0);
  assert.equal(h.player.snapshot().sources, 0);assert.equal(h.starts.length, 0);
});
