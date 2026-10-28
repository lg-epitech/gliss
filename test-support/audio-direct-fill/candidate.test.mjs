import test from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {AudioPlayer as Baseline} from '../../web/audio-player.js';
import {AudioPlayer as Candidate} from './audio-player.js';

function packet(codec, frames, sequence = 0) {
  const raw = new ArrayBuffer(20 + (codec === 'pcm' ? frames * 4 : 1)), view = new DataView(raw);
  new Uint8Array(raw, 0, 4).set([71, 76, 65, 49]);
  view.setUint8(4, codec === 'pcm' ? 0 : 1);view.setUint8(5, 2);
  view.setUint32(8, 48000, true);view.setUint32(12, sequence, true);view.setUint32(16, frames, true);
  if (codec === 'pcm') for (let i = 0; i < frames * 2; i++) view.setInt16(20 + i * 2, (i * 733 + 32768) % 65536 - 32768, true);
  return raw;
}

function harness(Player, codec) {
  const buffers = [], sources = [], outputs = [];
  const context = {state: 'running', currentTime: 1, destination: {},
    createBuffer(channels, frames) {
      const data = Array.from({length: channels}, () => new Float32Array(frames));buffers.push(data);
      return {getChannelData: channel => data[channel], copyToChannel: (samples, channel) => data[channel].set(samples)};
    },
    createBufferSource() {
      const node = {connect() {}, disconnect() {}, stop() {this.stopped = true;}, start(at) {this.at = at;}};
      sources.push(node);return node;
    },close() {this.state = 'closed';return Promise.resolve();}};
  const player = new Player();player.context = context;player.codec = codec;player.enabled = true;
  if (codec === 'opus') {
    const saved = Object.getOwnPropertyDescriptor(globalThis, 'AudioDecoder');
    globalThis.AudioDecoder = class {
      state = 'unconfigured';decodeQueueSize = 0;
      constructor(callbacks) {this.callbacks = callbacks;}
      configure() {this.state = 'configured';}
      decode(chunk) {this.chunk = chunk;}
      close() {this.state = 'closed';}
    };
    player.createDecoder();
    if (saved) Object.defineProperty(globalThis, 'AudioDecoder', saved);else delete globalThis.AudioDecoder;
  }
  return {player, buffers, sources, context, outputs, output() {
    const decoder = player.decoder, frames = 960;
    const audio = {sampleRate: 48000, numberOfChannels: 2, numberOfFrames: frames,
      timestamp: decoder.chunk.timestamp, closed: false, copies: 0,
      copyTo(destination, {planeIndex, format}) {
        assert.equal(format, 'f32-planar');this.copies++;
        for (let i = 0; i < frames; i++) destination[i] = ((i * 733 + planeIndex * 51) % 65536 - 32768) / 32768;
      },close() {this.closed = true;}};
    decoder.callbacks.output(audio);outputs.push(audio);return audio;
  }};
}

for (const frames of [1, 960, 5760]) test(`direct fill: every PCM sample matches the original at ${frames} frames`, () => {
  const baseline = harness(Baseline, 'pcm'), candidate = harness(Candidate, 'pcm');
  const raw = packet('pcm', frames);baseline.player.push(raw);candidate.player.push(raw);
  assert.equal(candidate.player.scheduled, 1);assert.deepEqual(candidate.buffers, baseline.buffers);
  assert.deepEqual(candidate.sources.map(s => s.at), baseline.sources.map(s => s.at));
  baseline.player.dispose();candidate.player.dispose();
});

test('direct fill: Opus output copies directly into both destination planes with exact samples', t => {
  const saved = Object.getOwnPropertyDescriptor(globalThis, 'EncodedAudioChunk');
  globalThis.EncodedAudioChunk = class {constructor(value) {Object.assign(this, value);}};
  t.after(() => {if (saved) Object.defineProperty(globalThis, 'EncodedAudioChunk', saved);else delete globalThis.EncodedAudioChunk;});
  const baseline = harness(Baseline, 'opus'), candidate = harness(Candidate, 'opus');
  for (const h of [baseline, candidate]) {h.player.push(packet('opus', 960));assert.equal(h.output().closed, true);}
  assert.deepEqual(candidate.buffers, baseline.buffers);assert.equal(candidate.player.scheduled, 1);
  assert.equal(candidate.outputs[0].copies, 2);
  baseline.player.dispose();candidate.player.dispose();
});

test('direct fill: expired outputs and mute release without allocating playback buffers', t => {
  let now = 0;t.mock.method(performance, 'now', () => now);
  const saved = Object.getOwnPropertyDescriptor(globalThis, 'EncodedAudioChunk');
  globalThis.EncodedAudioChunk = class {constructor(value) {Object.assign(this, value);}};
  t.after(() => {if (saved) Object.defineProperty(globalThis, 'EncodedAudioChunk', saved);else delete globalThis.EncodedAudioChunk;});
  const h = harness(Candidate, 'opus');h.player.push(packet('opus', 960));now = 250;
  const output = h.output();assert.equal(output.closed, true);assert.equal(output.copies, 0);
  assert.equal(h.buffers.length, 0);assert.equal(h.player.dropped, 1);
  h.player.stop();h.player.push(packet('opus', 960, 1));assert.equal(h.buffers.length, 0);
});

test('direct fill: sequence gaps and the200ms queue cap retain only bounded new work', () => {
  const h = harness(Candidate, 'pcm');
  for (let i = 0; i < 40; i++) h.player.push(packet('pcm', 960, i));
  assert.ok(h.player.snapshot().queuedSeconds <= .2);assert.ok(h.player.sources.size <= 12);
  h.player.push(packet('pcm', 960, 100));assert.equal(h.player.sources.size, 1);
  assert.ok(h.sources.some(s => s.stopped));h.player.dispose();assert.equal(h.player.sources.size, 0);
});

test('direct fill: candidate changes only destination filling and its relative import', async () => {
  const baseline = await readFile(new URL('../../web/audio-player.js', import.meta.url), 'utf8');
  const candidate = await readFile(new URL('./audio-player.js', import.meta.url), 'utf8');
  const changes = [
    ["from './audio-adpcm.js'", "from '../../web/audio-adpcm.js'"],
    ["this.schedule(audio.numberOfFrames, channel => {\n            const samples = new Float32Array(audio.numberOfFrames);\n            audio.copyTo(samples, {planeIndex: channel, format: 'f32-planar'});\n            return samples;\n          });", "this.schedule(audio.numberOfFrames, (samples, channel) => {\n            audio.copyTo(samples, {planeIndex: channel, format: 'f32-planar'});\n          }, true);"],
    ["this.schedule(frames, channel => {\n          const samples = new Float32Array(frames);\n          for (let frame = 0; frame < frames; frame++) samples[frame] = header.getInt16(HEADER_BYTES + (frame * CHANNELS + channel) * 2, true) / 32768;\n          return samples;\n        });", "this.schedule(frames, (samples, channel) => {\n          for (let frame = 0; frame < frames; frame++) samples[frame] = header.getInt16(HEADER_BYTES + (frame * CHANNELS + channel) * 2, true) / 32768;\n        }, true);"],
    ['schedule(frames, samples) {', 'schedule(frames, samples, fill = false) {'],
    ['for (let channel = 0; channel < CHANNELS; channel++) buffer.copyToChannel(samples(channel), channel);', 'for (let channel = 0; channel < CHANNELS; channel++) {\n      if (fill) samples(buffer.getChannelData(channel), channel);\n      else buffer.copyToChannel(samples(channel), channel);\n    }'],
  ];
  let derived = baseline;
  for (const [from, to] of changes) {assert.equal(derived.split(from).length, 2);derived = derived.replace(from, to);}
  assert.equal(candidate, derived);
});
