// Actual WebCodecs decode of native fixtures; mock wrapper tests live separately.
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { launchBrowser, CPU_BROWSER_ARGS } from '../../../tests/helpers.mjs';

const dir = process.argv[2];
if (!dir) throw new Error('usage: node crates/gliss-video/tests/browser.mjs <native-fixture-directory>');
const source = await readFile(new URL('../../../web/video-decoder.js', import.meta.url));
const server = createServer(async (request, response) => {
  try {
    if (request.url === '/decoder.js') {
      response.writeHead(200, { 'Content-Type': 'text/javascript' }); response.end(source);
    } else if (/^\/[0-3]\.(glv|rgba)$/.test(request.url ?? '')) {
      response.writeHead(200, { 'Content-Type': 'application/octet-stream' });
      response.end(await readFile(resolve(dir, request.url.slice(1))));
    } else { response.writeHead(200, { 'Content-Type': 'text/html' }); response.end('<canvas></canvas>'); }
  } catch (error) { response.writeHead(500); response.end(error.message); }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
let browser;
try {
  browser = await launchBrowser();
  const page = await browser.newPage();
  await page.goto(`http://127.0.0.1:${server.address().port}`);
  const result = await page.evaluate(async () => {
    const { VideoDecoderAdapter } = await import('/decoder.js');
    const decoder = new VideoDecoderAdapter();
    const canvas = document.querySelector('canvas'), context = canvas.getContext('2d', { willReadFrequently: true });
    const frames = [];
    try {
      for (let index = 0; index < 4; index++) {
        const [packet, source] = await Promise.all([
          fetch(`/${index}.glv`).then(response => response.arrayBuffer()),
          fetch(`/${index}.rgba`).then(response => response.arrayBuffer()),
        ]);
        const frame = await decoder.decode(packet);
        canvas.width = frame.width; canvas.height = frame.height;
        context.drawImage(frame.videoFrame, 0, 0); frame.videoFrame.close();
        const pixels = context.getImageData(0, 0, frame.width, frame.height).data;
        const expected = new Uint8Array(source);
        let error = 0;
        for (let p = 0; p < pixels.length; p++) if (p % 4 !== 3) error += Math.abs(pixels[p] - expected[p]);
        frames.push({ sequence: frame.sequence, inputId: frame.inputId, width: frame.width,
                      height: frame.height, keyframe: frame.keyframe,
                      meanRgbError: error / (frame.width * frame.height * 3) });
      }
      return { configuration: decoder.configuration, frames };
    } finally { decoder.close(); }
  });
  for (let index = 0; index < 4; index++) {
    const frame = result.frames[index];
    assert.equal(frame.sequence, 10 + index); assert.equal(frame.inputId, 100 + index);
    assert.equal(frame.keyframe, index !== 1);
    assert.ok(frame.meanRgbError < 15, `bad rendered image: ${frame.meanRgbError}`);
  }
  assert.deepEqual([result.frames[3].width, result.frames[3].height], [98,66]);
  const gpu = await browser.newPage(); await gpu.goto('chrome://gpu');
  await gpu.locator('li').filter({ hasText: 'Video Decode' }).first().waitFor({ timeout: 5000 });
  result.browser = await browser.version();
  result.browserArgs = CPU_BROWSER_ARGS;
  result.gpuVideoLines = (await gpu.locator('li').allTextContents()).filter(line => /Video Decode|Video Encode/i.test(line));
  assert.ok(result.gpuVideoLines.some(line => /Video Decode: Software only/.test(line)), 'browser hardware video decode must be disabled');
  console.log(JSON.stringify(result, null, 2));
} finally {
  await browser?.close();
  await new Promise(resolve => server.close(resolve));
}
