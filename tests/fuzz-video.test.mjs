// Fuzzes the browser's GLV1 packet parser, and holds it to the verdicts of the Rust parser on the
// packets in a vector file that the Rust tests write.
import test from 'node:test';
import assert from 'node:assert/strict';
import {parseVideoPacket} from '../web/video-decoder.js';
import {fuzz, mutate, onMain, unhex, vectors} from './fuzz-support.mjs';

const MAX_PAYLOAD = 16 * 1024 * 1024;
// Every way the parser refuses a packet.
const REFUSALS = ['Invalid GLV1 packet size', 'Unknown video wire version', 'Invalid video frame header',
  'Invalid Annex B access unit', 'Invalid H.264 NAL header', 'GLV1 requires Baseline H.264 SPS', 'Invalid video IDR/parameter sets'];

// The NAL units of an Annex B stream. A start code is a one after two zeros; a third zero before
// it is part of the start code, and further zeros are the tail of the unit before.
function nalUnits(payload) {
  const starts = [];
  for (let at = 2; at < payload.length; at++) if (payload[at] === 1 && payload[at - 1] === 0 && payload[at - 2] === 0) starts.push(at);
  return starts.map((at, index) => {
    const next = starts[index + 1];
    return payload.subarray(at + 1, next === undefined ? payload.length : next - (payload[next - 3] === 0 ? 3 : 2));
  });
}

// The packet's fields if the table and rules of crates/gliss-video/README.md allow it. This is
// what the Rust parser checks: framing, and that a keyframe is an IDR slice with parameter sets.
function framed(bytes) {
  if (bytes.length < 24 || bytes.length > 24 + MAX_PAYLOAD || bytes[0] !== 71 || bytes[1] !== 76 || bytes[2] !== 86 || bytes[3] !== 49) return null;
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const width = view.getUint16(12, true), height = view.getUint16(14, true), flags = view.getUint16(18, true);
  const payload = bytes.subarray(24);
  if (width < 16 || height < 16 || width % 2 || height % 2 || Math.max(width, height) > 3840 || Math.min(width, height) > 2160 ||
      view.getUint16(16, true) !== 0 || flags > 1 || view.getUint32(20, true) !== payload.length) return null;
  const begins = payload[0] === 0 && payload[1] === 0 &&
    ((payload[2] === 1 && payload.length > 3) || (payload[2] === 0 && payload[3] === 1 && payload.length > 4));
  // The type of a unit is in the byte after its start code, whatever that byte is part of.
  const types = [];
  for (let at = 2; at < payload.length - 1; at++) if (payload[at] === 1 && payload[at - 1] === 0 && payload[at - 2] === 0) types.push(payload[at + 1] & 31);
  const keyframe = flags === 1, has = type => types.includes(type);
  if (!begins || has(5) !== keyframe || !(has(1) || has(5)) || (keyframe && !(has(7) && has(8)))) return null;
  return {sequence: view.getUint32(4, true), inputId: view.getUint32(8, true), width, height, keyframe};
}

// What the browser refuses beyond the framing, because it hands the bytes to a native decoder and
// builds the codec string from them: a unit with no bytes, a start code with nothing behind it, a
// header with the forbidden bit set, a parameter set that is cut short or not Baseline profile,
// and more units than a picture has.
function browserRefuses(payload) {
  const units = nalUnits(payload);
  return units.length > 4096 || units.some(unit => !unit.length || unit[0] & 128 || ((unit[0] & 31) === 7 && (unit.length < 4 || unit[1] !== 66)));
}

// The codec string of a stream: the three bytes after the header of its last parameter set.
function codecOf(payload) {
  const set = nalUnits(payload).findLast(unit => (unit[0] & 31) === 7);
  return set ? `avc1.${Array.from(set.subarray(1, 4), byte => byte.toString(16).padStart(2, '0')).join('')}` : null;
}

// Parses a packet and checks the result against the reference. Returns the parsed packet or null.
function checked(bytes) {
  const buffer = bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
  const fields = framed(bytes), expected = fields && !browserRefuses(bytes.subarray(24)) ? fields : null;
  let parsed;
  try { parsed = parseVideoPacket(buffer); } catch (error) {
    assert.equal(error.constructor, Error, `${error}`);
    assert.ok(REFUSALS.includes(error.message), error.message);
    assert.equal(expected, null, `refused with "${error.message}": ${Array.from(bytes)}`);
    return null;
  }
  assert.ok(expected, `accepted a packet the rules forbid: ${Array.from(bytes)}`);
  const {sequence, inputId, width, height, keyframe} = parsed;
  assert.deepEqual({sequence, inputId, width, height, keyframe}, expected);
  assert.equal(parsed.codec, codecOf(bytes.subarray(24)));
  assert.equal(parsed.bytes, bytes.length);
  assert.deepEqual(Array.from(parsed.payload), Array.from(bytes.subarray(24)));
  return parsed;
}

// A NAL unit of the given type behind a start code of either length.
function nal(rng, type) {
  const unit = rng.chance(2) ? [0, 0, 1] : [0, 0, 0, 1];
  unit.push(type | (rng.below(4) << 5));
  if (type === 7) unit.push(66, 0xc0, 0x1e);
  for (let count = rng.below(12); count > 0; count--) {
    // Zeros come in runs, with the byte that keeps them from reading as a start code.
    const kind = rng.below(8);
    if (kind === 0) unit.push(0, 0, 3); else unit.push(kind === 1 ? 0 : rng.between(2, 255));
  }
  unit.push(0x80);
  return unit;
}

// A valid packet: a keyframe with its parameter sets or a delta frame, and units a decoder skips.
function packet(rng) {
  const keyframe = rng.chance(2), types = keyframe ? [7, 8, 5] : [1];
  for (let count = rng.below(3); count > 0; count--) types.splice(rng.below(types.length + 1), 0, rng.pick([6, 9, 12, 1, 0, 31]));
  if (keyframe && rng.chance(4)) types.push(5);
  const payload = types.flatMap(type => nal(rng, type));
  const [width, height] = rng.pick([[3840, 2160], [2160, 3840], [16, 16], [2 * rng.between(8, 1920), 2 * rng.between(8, 1080)]]);
  const bytes = new Uint8Array(24 + payload.length), view = new DataView(bytes.buffer);
  bytes.set([71, 76, 86, 49]);
  view.setUint32(4, rng.next(), true);view.setUint32(8, rng.next(), true);
  view.setUint16(12, width, true);view.setUint16(14, height, true);
  view.setUint16(18, Number(keyframe), true);view.setUint32(20, payload.length, true);
  bytes.set(payload, 24);
  return bytes;
}

// One change to a packet: to its bytes, to its size or to the units of its payload.
function changed(rng, bytes) {
  if (bytes.length <= 24 || rng.chance(2)) return mutate(rng, bytes, [0, 4, 8, 12, 14, 16, 18, 20, 24, 27, 28]);
  const result = Array.from(bytes), view = () => new DataView(Uint8Array.from(result).buffer);
  if (rng.chance(6)) {
    const [width, height] = rng.pick([[16, 16], [14, 16], [16, 14], [17, 16], [16, 17], [3840, 2160], [2160, 3840], [3842, 2160], [3840, 2162], [2162, 2162], [0, 0]]);
    const sized = view();sized.setUint16(12, width, true);sized.setUint16(14, height, true);
    return new Uint8Array(sized.buffer);
  }
  if (rng.chance(8)) {
    // Flags and the reserved field next to them: only bit zero of the flags may be set.
    const flagged = view();flagged.setUint16(rng.pick([16, 18, 18]), rng.pick([1, 2, 3, 0x100, 0x8001]), true);
    return new Uint8Array(flagged.buffer);
  }
  const type = rng.pick([1, 5, 7, 8]);
  const unit = [
    [0, 0, 1], [0, 0, 0, 1], nal(rng, type), nal(rng, type), nal(rng, type),
    // The forbidden bit, another profile, and a parameter set cut short.
    [0, 0, 1, 0x80 | type, 0x80], [0, 0, 1, 0x67, rng.pick([77, 100, 66]), 0x80], [0, 0, 1, 0x67, 66],
  ][rng.below(8)];
  result.splice(rng.between(24, result.length), 0, ...unit);
  const framedAgain = view();
  // Most of the time the header still tells the truth about the length.
  if (!rng.chance(4)) framedAgain.setUint32(20, result.length - 24, true);
  if (rng.chance(4)) framedAgain.setUint16(18, framedAgain.getUint16(18, true) ^ 1, true);
  return new Uint8Array(framedAgain.buffer);
}

fuzz(import.meta.url, {
  'GLV1 packet parser': {iterations: 1500, run(rng, iteration) {
    let bytes = packet(rng);
    assert.ok(checked(bytes), 'a generated packet was refused');
    for (let round = 0; round < 3; round++) {
      for (let changes = 1 + rng.below(3); changes > 0; changes--) bytes = changed(rng, bytes);
      checked(bytes);
    }
    // What is not a buffer of bytes is refused like a packet of the wrong size.
    for (const other of [bytes, bytes.buffer.slice(0, rng.below(24)), 'GLV1', null, undefined, 24, {byteLength: 64}]) {
      assert.throws(() => parseVideoPacket(other), {message: 'Invalid GLV1 packet size'});
    }
    if (iteration === 0) {
      // As many units as the parser walks, one more, and a payload one byte over the limit.
      for (const units of [4096, 4097]) {
        const many = new Uint8Array(24 + units * 4 + 1), view = new DataView(many.buffer);
        many.set([71, 76, 86, 49]);
        for (let unit = 0; unit < units - 1; unit++) many.set([0, 0, 1, 0x49], 24 + unit * 4);
        many.set([0, 0, 1, 0x41, 0x80], 24 + (units - 1) * 4);
        view.setUint16(12, 16, true);view.setUint16(14, 16, true);view.setUint32(20, many.length - 24, true);
        assert.ok(framed(many));
        assert.equal(checked(many) !== null, units === 4096);
      }
      const over = new ArrayBuffer(24 + MAX_PAYLOAD + 1);
      new Uint8Array(over).set([71, 76, 86, 49]);
      assert.throws(() => parseVideoPacket(over), {message: 'Invalid GLV1 packet size'});
    }
  }},
});

if (onMain) test('fuzz: the browser parser agrees with the Rust parser on the same packets', () => {
  const counts = {both: 0, neither: 0, browserOnly: 0};
  for (const name of ['glv1-vectors.txt', 'glv1-encoded-vectors.txt']) {
    // The second file exists only after a campaign of the Rust encoder fuzzer.
    let lines;
    try { lines = vectors(name); } catch (error) { if (name === 'glv1-vectors.txt') throw error;continue; }
    for (const line of lines) {
      const [packet, ...verdict] = line.split(' ');
      const bytes = unhex(packet), parsed = checked(bytes);
      const rust = verdict[0] === 'refused' ? null : verdict.map(Number);
      // The reference here says of the framing what the Rust parser says.
      assert.equal(framed(bytes) !== null, rust !== null, `the reference and Rust differ on ${packet}`);
      if (!parsed) {
        // The browser refuses nothing Rust accepts, but for the rules of its own.
        assert.ok(!rust || browserRefuses(bytes.subarray(24)), `refused a packet Rust accepts: ${packet}`);
        counts[rust ? 'browserOnly' : 'neither']++;
        continue;
      }
      assert.ok(rust, `accepted a packet Rust refuses: ${packet}`);
      assert.deepEqual([parsed.sequence, parsed.inputId, parsed.width, parsed.height, Number(parsed.keyframe)], rust);
      counts.both++;
    }
  }
  assert.ok(counts.both >= 20 && counts.neither >= 20 && counts.browserOnly >= 5, JSON.stringify(counts));
});

// Found by the fuzzer above. A start code right behind another was read as a unit of type 0 with
// the second start code as its body, so the unit after it was never looked at: here an IDR slice
// in a packet that says it is a delta frame, which the Rust parser refuses.
if (onMain) test('fuzz: found: a start code behind a start code does not hide the unit after it', () => {
  const payload = [0, 0, 1, 0x41, 0x80, 0, 0, 1, 0, 0, 1, 0x65, 0x80];
  const bytes = new Uint8Array(24 + payload.length), view = new DataView(bytes.buffer);
  bytes.set([71, 76, 86, 49]);bytes.set(payload, 24);
  view.setUint16(12, 16, true);view.setUint16(14, 16, true);view.setUint32(20, payload.length, true);
  assert.equal(framed(bytes), null);
  assert.throws(() => parseVideoPacket(bytes.buffer), {message: 'Invalid Annex B access unit'});
});
