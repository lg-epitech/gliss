import test from 'node:test';
import assert from 'node:assert/strict';
import {classifyMarker, MARKER_FRAMES} from '../test-support/qemu-audio-marker/detector.js';

function tones(left, right, phase = 0, noise = 0) {
  let state = 17;
  return [left, right].map((frequency, channel) => Float32Array.from({length: MARKER_FRAMES}, (_, n) => {
    state = (Math.imul(state, 1664525) + 1013904223) >>> 0;
    return (channel ? 0.24 : 0.36) * Math.sin(2 * Math.PI * frequency * n / 48000 + phase)
      + noise * (state / 0xffffffff - 0.5);
  }));
}

test('audio markers identify all 48 stereo codes across phase and bounded noise', () => {
  for (let code = 1; code <= 48; code++) for (const phase of [0, 0.3, 1.8, 3.5])
    assert.equal(classifyMarker(tones(1000 + (code - 1) * 100, 11000 - (code - 1) * 100, phase, 0.05)).code, code);
});

test('audio markers reject the baseline, silence, channel mismatch and ambiguous mixtures', () => {
  assert.equal(classifyMarker(tones(440, 880)).code, null);
  assert.equal(classifyMarker([new Float32Array(MARKER_FRAMES), new Float32Array(MARKER_FRAMES)]).code, null);
  assert.equal(classifyMarker(tones(1000, 10900)).code, null);
  const one = tones(1000, 11000), two = tones(1100, 10900);
  const mixed = one.map((row, channel) => row.map((value, i) => (value + two[channel][i]) / 2));
  assert.equal(classifyMarker(mixed).code, null);
  one[0][0] = NaN;assert.equal(classifyMarker(one).code, null);
  assert.throws(() => classifyMarker(tones(1000, 11000), 44100));
});
