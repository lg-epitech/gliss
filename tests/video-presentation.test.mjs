import test from 'node:test';
import assert from 'node:assert/strict';
import {RgbaVideoPresentation} from '../web/video-presentation.js';

function frame(width = 32, height = 32, overrides = {}) {
  const calls = [], videoFrame = {format: 'I420', visibleRect: {x: 0, y: 0, width, height},
    async copyTo(pixels, options) {calls.push({pixels, options});pixels.fill(123);},
    close() {calls.push('close');}, ...overrides};
  return {value: {width, height, sequence: 7, inputId: 4, bytes: 100, videoFrame}, calls};
}
test('RGBA presentation reuses one tight buffer, preserves frame metadata and closes converted frames', async () => {
  const presenter = new RgbaVideoPresentation(), first = frame(), second = frame();
  const a = await presenter.prepare(first.value), b = await presenter.prepare(second.value);
  assert.equal(a.pixels, b.pixels);assert.equal(a.pixels.length, 32 * 32 * 4);
  assert.ok(a.pixels.every(value => value === 123));assert.equal(a.videoFrame, undefined);
  assert.deepEqual([a.sequence, a.inputId, a.bytes], [7, 4, 100]);
  assert.deepEqual(a.damage, {x: 0, y: 0, width: 32, height: 32});
  assert.deepEqual(first.calls[0].options, {format: 'RGBA', layout: [{offset: 0, stride: 128}]});
  assert.equal(first.calls[1], 'close');assert.equal(second.calls[1], 'close');
  const resized = frame(64, 48), c = await presenter.prepare(resized.value);
  assert.notEqual(c.pixels, a.pixels);assert.equal(c.pixels.length, 64 * 48 * 4);
});
test('unavailable formats, cropped/display-scaled frames and invalid sizes retain direct rendering', async () => {
  for (const overrides of [{format: null}, {format: 'NV12'}, {copyTo: undefined}, {visibleRect: {width: 30, height: 32}}]) {
    const presenter = new RgbaVideoPresentation(), row = frame(32, 32, overrides);
    assert.equal(await presenter.prepare(row.value), row.value);assert.deepEqual(row.calls, []);
    assert.equal(presenter.pixels, null);
  }
  const presenter = new RgbaVideoPresentation();
  for (const [width, height] of [[8, 8], [3840, 3000], [NaN, 32]]) {
    const row = frame(width, height);assert.equal(await presenter.prepare(row.value), row.value);
    assert.deepEqual(row.calls, []);assert.equal(presenter.pixels, null);
  }
  const rgba = {width: 32, height: 32, pixels: new Uint8ClampedArray(4096)};
  assert.equal(await presenter.prepare(rgba), rgba);
});
test('copy rejection falls back once per connection without closing the caller-owned frame', async () => {
  const presenter = new RgbaVideoPresentation(), row = frame(32, 32, {async copyTo() {throw new Error('unsupported');}});
  assert.equal(await presenter.prepare(row.value), row.value);assert.deepEqual(row.calls, []);
  const next = frame();assert.equal(await presenter.prepare(next.value), next.value);assert.deepEqual(next.calls, []);
  assert.equal(presenter.pixels, null);presenter.reset();
  assert.equal((await presenter.prepare(next.value)).videoFrame, undefined);
});
test('reset during an async copy releases reuse without replacing its destination', async () => {
  let done, destination;
  const presenter = new RgbaVideoPresentation(), row = frame(32, 32, {copyTo(pixels) {
    destination = pixels;return new Promise(resolve => {done = resolve;});
  }});
  const pending = presenter.prepare(row.value);presenter.reset();
  assert.equal(presenter.pixels, null);destination.fill(45);done();
  const prepared = await pending;
  assert.equal(prepared.pixels, destination);assert.equal(prepared.pixels[0], 45);
  assert.equal(presenter.pixels, null);assert.deepEqual(row.calls, ['close']);
});
