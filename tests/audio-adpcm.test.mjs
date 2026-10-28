import test from 'node:test';
import assert from 'node:assert/strict';
import {decodeAdpcm} from '../web/audio-adpcm.js';
import {AudioPlayer} from '../web/audio-player.js';

function packet(sequence, left = 8192, right = -8192) {
  const data = new ArrayBuffer(987), view = new DataView(data);
  new Uint8Array(data, 0, 4).set([71,76,65,49]);view.setUint8(4, 2);view.setUint8(5, 2);
  view.setUint32(8, 48000, true);view.setUint32(12, sequence, true);view.setUint32(16, 960, true);
  view.setInt16(20, left, true);view.setInt16(24, right, true);
  return data;
}

function contextHarness(t, support) {
  const copies = [], starts = [], saved = new Map();
  for (const name of ['AudioContext', 'AudioDecoder', 'EncodedAudioChunk']) saved.set(name, Object.getOwnPropertyDescriptor(globalThis, name));
  globalThis.AudioContext = class {
    state = 'running';currentTime = 1;destination = {};
    resume() {return Promise.resolve();}
    close() {this.state = 'closed';return Promise.resolve();}
    createBuffer() {return {copyToChannel(samples, channel) {copies.push({channel, samples});}};}
    createBufferSource() {return {connect() {}, disconnect() {}, stop() {}, start(at) {starts.push(at);}};}
  };
  globalThis.AudioDecoder = support === undefined ? undefined : class {
    static async isConfigSupported() {if (support === 'reject') throw new Error('unsupported');return {supported: support};}
    state = 'unconfigured';
    configure() {this.state = 'configured';}
    close() {this.state = 'closed';}
  };
  globalThis.EncodedAudioChunk = support === undefined ? undefined : class {};
  const player = new AudioPlayer();
  t.after(() => {
    player.dispose();
    for (const [name, value] of saved) if (value) Object.defineProperty(globalThis, name, value);else delete globalThis[name];
  });
  return {player, copies, starts};
}

test('audio ADPCM: fixed nibbles preserve stereo order, subarray offsets and signed clipping', () => {
  const backing = new Uint8Array([99,99,0xfe,0x7f,0,0,1,0x80,0,0,0xf7,0xf7,99]);
  const [left, right] = decodeAdpcm(backing.subarray(2,12), 3);
  assert.deepEqual([...left], [32766/32768,32767/32768,32767/32768]);
  assert.deepEqual([...right], [-32767/32768,-1,-1]);
  const fixed = decodeAdpcm(new Uint8Array([0,0,0,0,0,0,0,0,0xf7,0xf7]), 3);
  assert.deepEqual([...fixed[0]], [0,11/32768,41/32768]);
  assert.deepEqual([...fixed[1]], [0,-11/32768,-41/32768]);
});

test('audio ADPCM: malformed state and lengths fail within the fixed sample cap', () => {
  for (const frames of [0,-1,1.5,5761,Infinity,NaN]) assert.throws(() => decodeAdpcm(new Uint8Array(8), frames));
  for (const size of [0,7,9]) assert.throws(() => decodeAdpcm(new Uint8Array(size), 1));
  for (const [offset, value] of [[2,89],[6,255],[3,1],[7,1]]) {
    const bytes = new Uint8Array(8);bytes[offset] = value;assert.throws(() => decodeAdpcm(bytes, 1));
  }
});

for (const support of [undefined, false, 'reject']) test(`audio ADPCM: advertised fallback is selected without usable WebCodecs (${support})`, async t => {
  const {player} = contextHarness(t, support);
  player.status({available: true, enabled: false, codecs: ['opus','ima-adpcm','pcm']});
  assert.equal(await player.enable(), 'ima-adpcm');
});

test('audio ADPCM: Opus stays preferred and an old peer retains PCM fallback', async t => {
  const {player} = contextHarness(t, true);
  player.status({available: true, codecs: ['opus','ima-adpcm','pcm']});
  assert.equal(await player.enable(), 'opus');
  globalThis.AudioDecoder = undefined;
  player.status({available: true, enabled: false});
  assert.equal(await player.enable(), 'pcm');
});

test('audio ADPCM: a sequence gap plays only the new independent block and mute releases it', async t => {
  const {player, copies} = contextHarness(t);
  player.status({available: true, codecs: ['opus','ima-adpcm','pcm']});
  await player.enable();player.push(packet(0));player.push(packet(40, 4096, -2048));
  assert.equal(player.scheduled, 2);assert.equal(player.sources.size, 1);assert.equal(player.dropped, 1);
  assert.equal(copies.at(-2).samples[0], 0.125);assert.equal(copies.at(-1).samples[0], -0.0625);
  assert.ok(player.snapshot().queuedSeconds <= 0.2);
  player.stop();player.push(packet(41));
  assert.equal(player.scheduled, 2);assert.equal(player.sources.size, 0);assert.equal(player.context, null);
});

test('audio ADPCM: invalid packet state stops playback before scheduling', async t => {
  const {player, starts} = contextHarness(t);
  player.status({available: true, codecs: ['opus','ima-adpcm','pcm']});await player.enable();
  const bytes = packet(0);new Uint8Array(bytes)[26] = 89;player.push(bytes);
  assert.equal(player.enabled, false);assert.equal(starts.length, 0);assert.equal(player.context, null);
});
