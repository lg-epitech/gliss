import assert from 'node:assert/strict';
import test from 'node:test';

import {TilePainter} from '../web/tile-painter.js';

const TILE = 32;

function color(tile, version, x, y) {
  return [(tile * 17 + version * 47 + x) % 251,
    (tile * 29 + version * 83 + y) % 251,
    (tile + version * 107 + x + y) % 251, 255];
}

function patch(width, height, tile, sequence, {epoch = 1, arrival = 0} = {}) {
  const columns = Math.ceil(width / TILE);
  const x = tile % columns * TILE, y = Math.floor(tile / columns) * TILE;
  const w = Math.min(TILE, width - x), h = Math.min(TILE, height - y);
  const pixels = new Uint8ClampedArray(w * h * 4);
  for (let row = 0; row < h; row++) for (let column = 0; column < w; column++) {
    pixels.set(color(tile, sequence, column, row), (row * w + column) * 4);
  }
  return {epoch, sequence, frameWidth: width, frameHeight: height, tile,
    x, y, width: w, height: h, pixels, arrival, decoded: arrival};
}

function expectedPixels(width, height, versions) {
  const pixels = new Uint8ClampedArray(width * height * 4);
  const columns = Math.ceil(width / TILE);
  for (let y = 0; y < height; y++) for (let x = 0; x < width; x++) {
    const tile = Math.floor(y / TILE) * columns + Math.floor(x / TILE);
    const version = versions[tile];
    pixels.set(version == null ? [0, 0, 0, 255] : color(tile, version, x % TILE, y % TILE),
      (y * width + x) * 4);
  }
  return pixels;
}

function differentPixels(actual, expected) {
  assert.equal(actual.length, expected.length, 'visible canvas dimensions');
  let different = 0;
  for (let offset = 0; offset < actual.length; offset += 4) {
    if (actual[offset] !== expected[offset] || actual[offset + 1] !== expected[offset + 1] ||
      actual[offset + 2] !== expected[offset + 2] || actual[offset + 3] !== expected[offset + 3]) different++;
  }
  return different;
}

// This models Canvas pixel writes and the browser's presentation boundary, not
// the painter's queue. All rAF callbacks already scheduled for a display tick
// finish before the visible canvas is sampled. No per-putImageData sampling.
function browser(t) {
  const originalImageData = Object.getOwnPropertyDescriptor(globalThis, 'ImageData');
  Object.defineProperty(globalThis, 'ImageData', {configurable: true, value: class {
    constructor(data, width, height) {
      if (typeof data === 'number') {
        height = width; width = data; data = new Uint8ClampedArray(width * height * 4);
      }
      assert.ok(data instanceof Uint8ClampedArray);
      assert.equal(data.length, width * height * 4);
      this.data = data; this.width = width; this.height = height;
    }
  }});
  t.after(() => {
    if (originalImageData) Object.defineProperty(globalThis, 'ImageData', originalImageData);
    else delete globalThis.ImageData;
  });

  let width = 0, height = 0, pixels = new Uint8ClampedArray(0), clock = 0, frameId = 0;
  const callbacks = new Map(), presentations = [], paints = [];
  function clear() {
    pixels = new Uint8ClampedArray(width * height * 4);
    // The shipped painter requests an opaque CPU canvas.
    for (let offset = 3; offset < pixels.length; offset += 4) pixels[offset] = 255;
  }
  const context = {putImageData(image, dx, dy,
    dirtyX = 0, dirtyY = 0, dirtyWidth = image.width, dirtyHeight = image.height) {
    assert.ok(image.data instanceof Uint8ClampedArray);
    assert.ok([dx, dy, dirtyX, dirtyY, dirtyWidth, dirtyHeight].every(Number.isInteger));
    const left = Math.max(0, dirtyX, -dx), top = Math.max(0, dirtyY, -dy);
    const right = Math.min(image.width, dirtyX + dirtyWidth, width - dx);
    const bottom = Math.min(image.height, dirtyY + dirtyHeight, height - dy);
    for (let y = top; y < bottom; y++) {
      pixels.set(image.data.subarray((y * image.width + left) * 4, (y * image.width + right) * 4),
        ((y + dy) * width + left + dx) * 4);
    }
  }};
  const canvas = {
    get width() { return width; }, set width(value) { width = value; clear(); },
    get height() { return height; }, set height(value) { height = value; clear(); },
    getContext(kind, options) {
      assert.equal(kind, '2d'); assert.equal(options?.alpha, false); return context;
    },
  };
  const painter = new TilePainter(canvas, {now: () => clock,
    requestFrame(callback) { callbacks.set(++frameId, callback); return frameId; },
    cancelFrame(id) { callbacks.delete(id); },
    onPaint(value) { paints.push(value); },
    onError(error) { throw error; },
  });
  t.after(() => painter.close());

  function present() {
    assert.ok(callbacks.size > 0, 'a browser display tick was requested');
    const tick = [...callbacks];
    for (const [id, callback] of tick) {
      if (!callbacks.delete(id)) continue;
      callback(clock);
    }
    const visible = pixels.slice();
    presentations.push(visible);
    return visible;
  }
  function drain() {
    let ticks = 0;
    while (callbacks.size) {
      assert.ok(++ticks <= 32, 'pending CPU work must settle without waiting for network input');
      present();
    }
  }
  function queue(patches) {
    // Real worker transfers are limited to 64 independent tiles per message.
    for (let offset = 0; offset < patches.length; offset += 64) {
      painter.acceptBatch(patches.slice(offset, offset + 64));
    }
  }
  return {painter, presentations, paints, queue, present, drain,
    advance(milliseconds) { clock += milliseconds; },
    resetObservations() { presentations.length = 0; paints.length = 0; },
  };
}

test('a queued 1000-tile desktop update has no old/new mixed visible presentations', t => {
  const output = browser(t), width = 1280, height = 800, tiles = 1000;
  output.queue(Array.from({length: tiles}, (_, tile) => patch(width, height, tile, 1)));
  output.drain();
  const oldPixels = expectedPixels(width, height, Array(tiles).fill(1));
  assert.equal(differentPixels(output.presentations.at(-1), oldPixels), 0, 'initial CPU raster');
  output.resetObservations();

  // All current pixels are decoded and queued before the browser can present.
  // No network packet, missing tile or whole-frame completion signal is needed.
  output.queue(Array.from({length: tiles}, (_, tile) => patch(width, height, tile, 2)));
  const currentPixels = expectedPixels(width, height, Array(tiles).fill(2));
  output.drain();
  const mixed = output.presentations.filter(visible =>
    differentPixels(visible, oldPixels) > 0 && differentPixels(visible, currentPixels) > 0).length;
  const observed = {
    visible_presentations: output.presentations.length,
    old_new_mixed_presentations: mixed,
    first_presentation_wrong_pixels: differentPixels(output.presentations[0], currentPixels),
    final_presentation_wrong_pixels: differentPixels(output.presentations.at(-1), currentPixels),
  };
  t.diagnostic(JSON.stringify({workload: '1280x800, 1000 queued RGBA tiles, one current sequence', ...observed}));
  assert.deepEqual(observed, {visible_presentations: 1, old_new_mixed_presentations: 0,
    first_presentation_wrong_pixels: 0, final_presentation_wrong_pixels: 0});
});

test('missing tiles do not hold disjoint current updates and stale tiles cannot roll pixels back', t => {
  const output = browser(t), width = 96, height = 32;
  output.queue([0, 1, 2].map(tile => patch(width, height, tile, 1)));
  output.drain(); output.resetObservations();

  // Tile 1 never arrives. Tile 2 has a lower sequence than tile 0, but is
  // independently current. A global sequence/frame barrier would be incorrect.
  output.queue([patch(width, height, 0, 20), patch(width, height, 2, 12)]);
  output.queue([patch(width, height, 0, 21)]);
  output.queue([patch(width, height, 0, 20)]);
  const visible = output.present();
  assert.equal(differentPixels(visible, expectedPixels(width, height, [21, 1, 12])), 0);
  assert.equal(output.presentations.length, 1);
  assert.equal(output.painter.snapshot().queueCount, 0);
});

test('edge tiles preserve exact CPU pixels across sparse updates and geometry changes', t => {
  const output = browser(t);
  output.queue([patch(64, 32, 0, 1), patch(64, 32, 1, 1)]);
  output.drain(); output.resetObservations();

  // A new odd-sized epoch arrives partially. Present the available edge now;
  // old geometry must not leak through a persistent CPU backing buffer.
  output.queue([patch(67, 35, 5, 2, {epoch: 2})]);
  let visible = output.present();
  assert.equal(differentPixels(visible, expectedPixels(67, 35, [null, null, null, null, null, 2])), 0);
  assert.equal(output.painter.snapshot().completeEpochCoverage, false);

  output.queue([patch(67, 35, 0, 3, {epoch: 2}), patch(67, 35, 2, 3, {epoch: 2})]);
  visible = output.present();
  assert.equal(differentPixels(visible, expectedPixels(67, 35, [3, null, 3, null, null, 2])), 0);
});

test('expired queued pixels are discarded while fresh disjoint pixels still present', t => {
  const output = browser(t), width = 64, height = 32;
  output.queue([patch(width, height, 0, 1), patch(width, height, 1, 1)]);
  output.drain(); output.resetObservations();
  output.queue([patch(width, height, 0, 2)]);
  output.advance(250);
  output.queue([patch(width, height, 1, 3, {arrival: 250})]);
  const visible = output.present();
  assert.equal(differentPixels(visible, expectedPixels(width, height, [1, 3])), 0);
  assert.equal(output.painter.snapshot().queueCount, 0);
});
