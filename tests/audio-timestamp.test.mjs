import test from 'node:test';
import assert from 'node:assert/strict';
import {createServer} from 'node:http';
import {launchBrowser} from './helpers.mjs';

test('audio: real Opus output timestamps track 320 packet timestamps within one microsecond', {timeout: 15000}, async t => {
  const server = createServer((_request, response) => response.end('<!doctype html><title>Owned audio fixture</title>'));
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise(resolve => server.close(resolve)));
  const browser = await launchBrowser();t.after(() => browser.close());
  const page = await browser.newPage();
  await page.goto(`http://127.0.0.1:${server.address().port}`);
  const result = await page.evaluate(async () => {
    const chunks = [], errors = [], timestamps = [];
    const encoder = new AudioEncoder({output(chunk) {
      const bytes = new Uint8Array(chunk.byteLength);chunk.copyTo(bytes);chunks.push(bytes);
    }, error(error) {errors.push(error.name);}});
    encoder.configure({codec: 'opus', sampleRate: 48000, numberOfChannels: 2, bitrate: 96000});
    const data = new AudioData({format: 'f32-planar', sampleRate: 48000, numberOfChannels: 2,
      numberOfFrames: 960, timestamp: 0, data: new Float32Array(1920)});
    encoder.encode(data);data.close();await encoder.flush();encoder.close();
    const decoder = new AudioDecoder({output(audio) {timestamps.push(audio.timestamp);audio.close();},
      error(error) {errors.push(error.name);}});
    decoder.configure({codec: 'opus', sampleRate: 48000, numberOfChannels: 2});
    for (let i = 0; i < 320; i++) decoder.decode(new EncodedAudioChunk({type: 'key', timestamp: i * 20000,
      duration: 20000, data: chunks[0]}));
    await decoder.flush();decoder.close();
    return {errors, timestamps, rounded: timestamps.map((actual, i) => ({actual, expected: i * 20000, delta: actual - i * 20000})).filter(row => row.delta !== 0)};
  });
  assert.deepEqual(result.errors, []);assert.equal(result.timestamps.length, 320);
  result.timestamps.forEach((timestamp, i) => assert.ok(Math.abs(timestamp - i * 20000) <= 1));
  t.diagnostic(JSON.stringify({outputs: result.timestamps.length, rounded: result.rounded}));
  await page.close();
});
