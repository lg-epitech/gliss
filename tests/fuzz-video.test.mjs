// Cases that fuzzing the browser's GLV1 packet parser found, kept as tests.
import test from 'node:test';
import assert from 'node:assert/strict';
import {parseVideoPacket} from '../web/video-decoder.js';

// A start code right behind another was read as a unit of type 0 with the second start code as
// its body, so the unit after it was never looked at: here an IDR slice in a packet that says it
// is a delta frame, which the Rust parser refuses.
test('fuzz: found: a start code behind a start code does not hide the unit after it', () => {
  const payload = [0, 0, 1, 0x41, 0x80, 0, 0, 1, 0, 0, 1, 0x65, 0x80];
  const bytes = new Uint8Array(24 + payload.length), view = new DataView(bytes.buffer);
  bytes.set([71, 76, 86, 49]);bytes.set(payload, 24);
  view.setUint16(12, 16, true);view.setUint16(14, 16, true);view.setUint32(20, payload.length, true);
  assert.throws(() => parseVideoPacket(bytes.buffer), {message: 'Invalid Annex B access unit'});
});
