import test from 'node:test';
import assert from 'node:assert/strict';
import {decodeMotionStamp} from '../test-support/qemu-audio-motion/stamp.js';

function image(frame, shift = 0) {
  const bytes = new Uint8ClampedArray(372 * 20 * 4);
  const check = (frame & 255) ^ ((frame >>> 8) & 255) ^ ((frame >>> 16) & 255) ^ 0x5a;
  const value = (frame | (check << 24)) >>> 0;
  for (let y = 0; y < 20; y++) for (let x = 0; x < 372; x++) {
    let color = [24,32,43];
    if (x < 20) color = [240,16,240];
    if (x >= 352) color = [16,240,240];
    if (x >= 24 && x < 344) color = Array(3).fill((value >>> Math.floor((x - 24) / 10)) & 1 ? 232 : 24);
    bytes.set([...color.map(n => n + shift),255], (y * 372 + x) * 4);
  }
  return bytes;
}

test('motion marker: complete source IDs survive small color errors', () => {
  for (const frame of [0,1,255,256,65535,0x123456,0xffffff]) for (const shift of [-10,0,10])
    assert.equal(decodeMotionStamp(image(frame, shift)), frame);
});

test('motion marker: invalid sentinel, uncertain cells, checksum and size are rejected', () => {
  assert.equal(decodeMotionStamp(new Uint8ClampedArray(3)), null);
  for (const [center, color] of [[10,[0,0,0]], [362,[0,0,0]], [29,[128,128,128]], [29,[232,232,232]]]) {
    const bytes = image(0);
    for (let dy = -1; dy <= 1; dy++) for (let dx = -1; dx <= 1; dx++)
      bytes.set(color, ((10 + dy) * 372 + center + dx) * 4);
    assert.equal(decodeMotionStamp(bytes), null);
  }
});
