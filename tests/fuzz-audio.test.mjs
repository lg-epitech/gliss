// Fuzzes the browser's readers of audio bytes from the server: the ADPCM decoder and the GLA1
// packet handling of the player. No browser is needed: the player gets an audio context that
// records what it is asked to allocate and schedule.
import test from 'node:test';
import assert from 'node:assert/strict';
import {decodeAdpcm} from '../web/audio-adpcm.js';
import {AudioPlayer, isAudioPacket} from '../web/audio-player.js';
import {fingerprint, fuzz, mutate, onMain, unhex, vectors} from './fuzz-support.mjs';

// The limits of docs/audio-wire.md.
const MAX_FRAMES = 5760, MAX_OPUS_BYTES = 4096, MAX_SCHEDULED_SECONDS = 0.2, MAX_SOURCES = 12, MAX_PENDING_DECODES = 8;
const ADPCM_ERRORS = ['Invalid ADPCM block length.', 'Invalid ADPCM state.'];

// Whether a block is one the format allows: 1 to 5760 frames, two four-byte channel headers with
// a step index of at most 88 and a zero byte, then one byte for each frame after the first.
function adpcmAllowed(bytes, frames) {
  return bytes instanceof Uint8Array && Number.isInteger(frames) && frames >= 1 && frames <= MAX_FRAMES &&
    bytes.length === frames + 7 && bytes[2] <= 88 && bytes[6] <= 88 && bytes[3] === 0 && bytes[7] === 0;
}

function adpcmBlock(rng, frames) {
  const bytes = rng.bytes(frames + 7);
  bytes[2] = rng.below(89);bytes[6] = rng.below(89);bytes[3] = 0;bytes[7] = 0;
  return bytes;
}

// Decodes and checks everything that can be said without a second decoder: what is refused, how
// it is refused, and that what is returned is within the frame limit and the range of a sample.
function checkedAdpcm(bytes, frames) {
  let decoded;
  try { decoded = decodeAdpcm(bytes, frames); } catch (error) {
    assert.equal(error.constructor, Error, `${error}`);
    assert.ok(ADPCM_ERRORS.includes(error.message), error.message);
    assert.ok(!adpcmAllowed(bytes, frames), 'an allowed block was refused');
    return null;
  }
  assert.ok(adpcmAllowed(bytes, frames), 'a block the format forbids was decoded');
  assert.equal(decoded.length, 2);
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  decoded.forEach((channel, index) => {
    assert.ok(channel instanceof Float32Array);
    assert.equal(channel.length, frames);
    assert.equal(channel[0], view.getInt16(index * 4, true) / 32768);
    assert.ok(channel.every(sample => sample >= -1 && sample < 1));
  });
  return decoded;
}

// A GLA1 packet as the server writes it for `codec`, with `frames` frames.
function audioPacket(rng, codec, sequence, frames) {
  const payload = codec === 0 ? rng.bytes(frames * 4) : codec === 2 ? adpcmBlock(rng, frames)
    : rng.bytes(rng.pick([1, 2, 60, 300, MAX_OPUS_BYTES, MAX_OPUS_BYTES + 1, 0]));
  const bytes = new Uint8Array(20 + payload.length), view = new DataView(bytes.buffer);
  bytes.set([71, 76, 65, 49, codec, 2, 0, 0]);
  view.setUint32(8, 48000, true);view.setUint32(12, sequence, true);view.setUint32(16, frames, true);
  bytes.set(payload, 20);
  return bytes;
}

// Whether a player that expects `codec` may play the packet, read from docs/audio-wire.md.
function packetAllowed(bytes, codec) {
  if (bytes.length < 20) return false;
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength), frames = view.getUint32(16, true);
  const payload = bytes.length - 20;
  if (bytes[4] !== codec || bytes[5] !== 2 || bytes[6] !== 0 || bytes[7] !== 0 || view.getUint32(8, true) !== 48000 ||
      frames < 1 || frames > MAX_FRAMES) return false;
  if (codec === 0) return payload === frames * 4;
  if (codec === 1) return payload >= 1 && payload <= MAX_OPUS_BYTES;
  return adpcmAllowed(bytes.subarray(20), frames);
}

// An audio context and decoder that keep count of what the player asks of them.
function installAudio(opus) {
  const seen = {buffers: [], copies: [], starts: 0, decodes: [], clock: 1};
  globalThis.AudioContext = class {
    state = 'running';destination = {};
    get currentTime() { return seen.clock; }
    resume() { return Promise.resolve(); }
    close() { this.state = 'closed';return Promise.resolve(); }
    createBuffer(channels, frames, rate) {
      seen.buffers.push({channels, frames, rate});
      return {copyToChannel(samples, channel) { seen.copies.push({channel, samples}); }};
    }
    createBufferSource() { return {connect() {}, disconnect() {}, stop() {}, start() { seen.starts++; }}; }
  };
  globalThis.AudioDecoder = opus ? class {
    static async isConfigSupported() { return {supported: true}; }
    state = 'unconfigured';decodeQueueSize = 0;
    configure() { this.state = 'configured'; }
    decode(chunk) { seen.decodes.push(chunk);this.decodeQueueSize = seen.queue; }
    close() { this.state = 'closed'; }
  } : undefined;
  globalThis.EncodedAudioChunk = opus ? class { constructor(init) { Object.assign(this, init); } } : undefined;
  return seen;
}

fuzz(import.meta.url, {
  'ADPCM decoder': {iterations: 2000, run(rng) {
    // One frame more than the limit is a block like any other, and refused.
    const frames = rng.pick([1, 2, 3, MAX_FRAMES, MAX_FRAMES + 1, rng.between(1, 64), rng.between(1, MAX_FRAMES)]);
    let bytes = adpcmBlock(rng, frames), count = frames;
    // The decoder is handed a view into the packet, not a buffer of its own.
    if (rng.chance(2)) { const whole = new Uint8Array(bytes.length + 9);whole.set(bytes, 5);bytes = whole.subarray(5, 5 + bytes.length); }
    switch (rng.below(8)) {
      case 0: bytes = mutate(rng, bytes, [2, 3, 6, 7]); break;
      case 1: bytes[rng.pick([2, 6])] = rng.pick([88, 89, 255]); break;
      case 2: bytes[rng.pick([3, 7])] = rng.pick([1, 255]); break;
      // A frame count that is not the block's, or not a count at all.
      case 3: count = rng.pick([0, -1, frames + 1, frames - 1, MAX_FRAMES + 1, 2 ** 32, 1.5, NaN, Infinity, `${frames}`, null, undefined, BigInt(frames)]); break;
      // Bytes that are not a byte array.
      case 4: bytes = rng.pick([bytes.buffer, Array.from(bytes), new DataView(bytes.buffer), null, 'GLA1', new Uint16Array(bytes.length)]); break;
      default:
    }
    checkedAdpcm(bytes, count);
  }},

  'audio packets': {iterations: 300, async run(rng) {
    const codec = rng.pick([0, 2, 1]);
    const seen = installAudio(codec === 1);
    let state = null;
    const player = new AudioPlayer(event => { state = event; });
    player.status({available: true, enabled: false, codecs: [['pcm'], ['opus', 'pcm'], ['ima-adpcm', 'pcm']][codec]});
    let sequence = rng.next();
    // A third of the runs are a clean stream of short packets that arrive faster than they play:
    // only such a stream fills the player's queues to their limits.
    const clean = rng.chance(3);
    for (let step = clean ? rng.between(14, 30) : rng.between(1, 12); step > 0; step--) {
      if (!player.enabled) assert.equal(await player.enable(), ['pcm', 'opus', 'ima-adpcm'][codec]);
      seen.clock += clean ? 0 : rng.pick([0, 0, 0.001, 0.02, 0.5]);
      seen.queue = clean ? 0 : rng.pick([0, 0, 0, 7, 8, 30]);
      // Mostly the next packet of the stream; sometimes a gap, another codec or damaged bytes.
      sequence = (sequence + (clean ? 1 : rng.pick([1, 1, 1, 1, 2, 0]))) >>> 0;
      const frames = clean ? rng.between(1, 64) : rng.pick([960, 960, 1920, 1, MAX_FRAMES, rng.between(1, 64)]);
      let bytes = audioPacket(rng, !clean && rng.chance(8) ? rng.below(4) : codec, sequence, frames);
      if (clean) {
        // As it is.
      } else if (rng.chance(3)) {
        for (let changes = 1 + rng.below(3); changes > 0; changes--) bytes = mutate(rng, bytes, [0, 4, 5, 6, 8, 12, 16, 20, 22, 26]);
      } else if (rng.chance(6)) {
        const view = new DataView(bytes.buffer);
        view.setUint32(16, rng.pick([0, MAX_FRAMES + 1, 2 ** 32 - 1, frames + 1]), true);
      }
      const data = bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
      // The client hands the player only what starts as an audio packet.
      const audio = bytes.length >= 4 && bytes[0] === 71 && bytes[1] === 76 && bytes[2] === 65 && bytes[3] === 49;
      assert.equal(isAudioPacket(data), audio);
      for (const other of [bytes, 'GLA1', null, undefined, 7, {byteLength: 20}]) assert.equal(isAudioPacket(other), false);
      if (!audio) continue;
      const before = {buffers: seen.buffers.length, copies: seen.copies.length, decodes: seen.decodes.length, scheduled: player.scheduled};
      player.push(data);
      if (packetAllowed(bytes, codec)) {
        assert.equal(player.enabled, true, 'a packet the format allows stopped playback');
        if (codec === 1) {
          assert.equal(seen.decodes.length, before.decodes + 1);
          assert.equal(seen.decodes.at(-1).data.length, bytes.length - 20);
        } else {
          assert.equal(player.scheduled, before.scheduled + 1);
          const count = new DataView(data).getUint32(16, true);
          assert.deepEqual(seen.buffers.at(-1), {channels: 2, frames: count, rate: 48000});
          assert.equal(seen.copies.length, before.copies + 2);
          for (const copy of seen.copies.slice(-2)) assert.equal(copy.samples.length, count);
        }
      } else {
        // A packet the format forbids stops playback before anything is made for it.
        assert.equal(player.enabled, false, 'a packet the format forbids was played');
        assert.equal(player.context, null);
        assert.equal(state.enabled, false);
        assert.deepEqual([seen.buffers.length, seen.copies.length, seen.decodes.length], [before.buffers, before.copies, before.decodes]);
      }
      // The bounds on what the player holds, whatever the packets say.
      assert.ok(seen.buffers.every(buffer => buffer.frames <= MAX_FRAMES));
      assert.ok(player.sources.size <= MAX_SOURCES);
      assert.ok(player.pendingDecodes.size <= MAX_PENDING_DECODES);
      assert.ok(player.snapshot().queuedSeconds <= MAX_SCHEDULED_SECONDS + 1e-9);
    }
    player.dispose();
  }},
});

if (onMain) test('fuzz: the ADPCM decoder gives the samples the Rust decoder model gives', () => {
  let decoded = 0;
  for (const line of vectors('adpcm-vectors.txt')) {
    const [frames, block, expected] = line.split(' ');
    const samples = checkedAdpcm(unhex(block), Number(frames));
    if (expected === 'refused') { assert.equal(samples, null, `decoded a block Rust refuses: ${line}`);continue; }
    assert.ok(samples, `refused a block Rust decodes: ${line}`);
    // Left and right in turn, as 16-bit numbers: what the Rust side fingerprinted.
    const bytes = new Uint8Array(Number(frames) * 4), view = new DataView(bytes.buffer);
    for (let frame = 0; frame < Number(frames); frame++) {
      view.setInt16(frame * 4, samples[0][frame] * 32768, true);view.setInt16(frame * 4 + 2, samples[1][frame] * 32768, true);
    }
    assert.equal(fingerprint(bytes), expected, `other samples than Rust for ${line.slice(0, 80)}`);
    decoded++;
  }
  assert.ok(decoded >= 40, `only ${decoded} blocks were compared`);
});
