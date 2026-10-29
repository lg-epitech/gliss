import test from 'node:test';
import assert from 'node:assert/strict';
import {startServer, launchBrowser, loginPage} from './helpers.mjs';

async function desktop(t) {
  const server = await startServer({codec: 'h264', workload: 'video', fps: 20});t.after(server.stop);
  const browser = await launchBrowser();t.after(() => browser.close());
  const page = await browser.newPage();t.after(() => page.close());
  const errors = [], imports = [];
  page.on('pageerror', error => errors.push(error.message));
  page.on('request', request => {if (new URL(request.url()).pathname === '/video-presentation.js') imports.push(true);});
  await loginPage(page, server);t.after(() => assert.deepEqual(errors, []));
  return {page, imports};
}
async function enable(page) {
  await page.evaluate(async () => {
    const {GlissClient} = await import('/client.js'), canvas = window.gliss.canvas;
    window.gliss.dispose();window.gliss = new GlissClient(canvas, () => {}, event => {
      if (!window.canvasProofs || event.type !== 'frame' || !event.frame.pixels || window.canvasProofs.length >= 20) return;
      const {width, height, pixels} = event.frame, actual = window.gliss.ctx.getImageData(0, 0, width, height).data;
      window.canvasProofs.push({width, mode: window.gliss.renderStrategy, equal: actual.every((value, i) => value === pixels[i])});
    }, {videoPresentation: 'rgba'});
    await window.gliss.connect();
  });
}
test('opt-in RGBA Canvas pixels equal direct VideoFrame rendering across native frames and resize', {timeout: 20000}, async t => {
  const {page, imports} = await desktop(t);assert.equal(imports.length, 0);
  await page.evaluate(async () => {
    const {RgbaVideoPresentation} = await import('/video-presentation.js'), prepare = RgbaVideoPresentation.prototype.prepare;
    window.presentationProofs = [];window.canvasProofs = [];
    RgbaVideoPresentation.prototype.prepare = async function (frame) {
      if (window.presentationProofs.length >= 20) return prepare.call(this, frame);
      const canvas = document.createElement('canvas');canvas.width = frame.width;canvas.height = frame.height;
      const ctx = canvas.getContext('2d', {alpha: false, willReadFrequently: true}), video = frame.videoFrame;
      ctx.drawImage(video, 0, 0);const expected = ctx.getImageData(0, 0, frame.width, frame.height).data;
      const result = await prepare.call(this, frame);
      window.presentationProofs.push({width: frame.width, height: frame.height, closed: video.format === null,
        equal: result.pixels?.length === expected.length && result.pixels.every((value, i) => value === expected[i])});
      return result;
    };
  });
  await enable(page);
  await page.waitForFunction(() => window.presentationProofs.length >= 4);
  await page.evaluate(() => {window.gliss.renderStrategy = 'damage';window.gliss.send({type: 'resize', width: 962, height: 602});});
  await page.waitForFunction(() => window.presentationProofs.filter(row => row.width === 962).length >= 8);
  const result = await page.evaluate(() => ({proofs: window.presentationProofs, canvas: window.canvasProofs, errors: window.gliss.metrics.errors,
    buffer: window.gliss.rgbaPresentation.pixels.length, width: window.gliss.canvas.width, height: window.gliss.canvas.height}));
  assert.equal(result.errors, 0);assert.ok(result.proofs.length >= 12);
  assert.ok(result.proofs.every(row => row.equal && row.closed));
  assert.ok(result.canvas.length >= 8);assert.ok(result.canvas.some(row => row.mode === 'damage'));
  assert.ok(result.canvas.every(row => row.equal));
  assert.deepEqual([result.width, result.height, result.buffer], [962, 602, 962 * 602 * 4]);
});
test('a rejected native RGBA copy falls back to direct Canvas once per connection', {timeout: 15000}, async t => {
  const {page} = await desktop(t);
  await page.evaluate(() => {
    window.copyAttempts = 0;
    VideoFrame.prototype.copyTo = function () {window.copyAttempts++;return Promise.reject(new DOMException('unsupported', 'NotSupportedError'));};
  });
  await enable(page);await page.waitForFunction(() => window.gliss.metrics.frames >= 8);
  assert.deepEqual(await page.evaluate(() => ({attempts: window.copyAttempts, errors: window.gliss.metrics.errors,
    disabled: window.gliss.rgbaPresentation.unsupported, released: window.gliss.rgbaPresentation.pixels === null})),
  {attempts: 1, errors: 0, disabled: true, released: true});
  await page.evaluate(() => window.gliss.connect());
  await page.waitForFunction(() => window.copyAttempts === 2);
});
test('disconnect during native copy closes the old frame and cannot paint after reconnect', {timeout: 15000}, async t => {
  const {page} = await desktop(t);
  await page.evaluate(() => {
    const copy = VideoFrame.prototype.copyTo;let once = true;
    VideoFrame.prototype.copyTo = function (...args) {
      const pending = copy.apply(this, args);
      if (!once) return pending;once = false;window.heldVideo = this;
      return pending.then(() => new Promise(resolve => {window.releaseCopy = resolve;}));
    };
  });
  await enable(page);await page.waitForFunction(() => window.releaseCopy);
  await page.evaluate(async () => {
    const client = window.gliss;window.oldCopyChain = client.chain;
    client.disconnect();await client.connect();
  });
  await page.waitForFunction(() => window.gliss.metrics.frames >= 3);
  const stopped = await page.evaluate(async () => {
    const client = window.gliss, before = client.metrics.frames, pixels = client.rgbaPresentation.pixels;
    window.releaseCopy();await window.oldCopyChain;
    return {paintedLate: client.metrics.frames !== before, closed: window.heldVideo.format === null,
      reusedNewBuffer: client.rgbaPresentation.pixels === pixels};
  });
  assert.deepEqual(stopped, {paintedLate: false, closed: true, reusedNewBuffer: true});
  assert.equal(await page.evaluate(() => window.gliss.metrics.errors), 0);
});
