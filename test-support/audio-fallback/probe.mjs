// Component probe; never connects to a production server or a VM.
import assert from 'node:assert/strict';
import {createServer} from 'node:http';
import {readFile, writeFile} from 'node:fs/promises';
import {createHash} from 'node:crypto';
import {launchBrowser} from '../../tests/helpers.mjs';

const [input, output] = process.argv.slice(2);
if (!input || !output) throw new Error('Usage: probe.mjs INPUT.json NEW-OUTPUT.json');
const bytes = await readFile(input);
assert.ok(bytes.length < 64 * 1024 * 1024);
const data = JSON.parse(bytes);
assert.ok(['gliss-ogg-audio-component-v2', 'gliss-ogg-audio-component-v3', 'gliss-ogg-audio-component-v4'].includes(data.schema));
const expectedArms = data.schema.endsWith('v4') ? 3 : data.schema.endsWith('v3') ? 9 : 27;
assert.equal(data.arms.length, expectedArms);
const sockets = new Set();
const adpcmSource = await readFile(new URL('../../web/audio-adpcm.js', import.meta.url));
const server = createServer((req, res) => {
  res.setHeader('Content-Security-Policy', "default-src 'self'; script-src 'self'; connect-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'");
  if (req.url === '/audio-adpcm.js') {res.setHeader('Content-Type', 'text/javascript');res.end(adpcmSource);}
  else if (req.url === '/data.json') {res.setHeader('Content-Type', 'application/json');res.end(bytes);}
  else if (req.url === '/') {res.setHeader('Content-Type', 'text/html');res.end('<!doctype html><title>Opus component</title>');}
  else res.writeHead(404).end();
});
server.on('connection', socket => {sockets.add(socket);socket.on('close', () => sockets.delete(socket));});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
let browser;
const result = {schema: data.schema.replace('-component-', '-browser-component-'), input_sha256: createHash('sha256').update(bytes).digest('hex'),
  native_csp: true, software_browser: true, arms: [], error: null};
try {
  browser = await launchBrowser();
  result.browser_version = browser.version();
  const page = await browser.newPage();
  await page.addInitScript(() => {globalThis.AudioDecoder = undefined;globalThis.EncodedAudioChunk = undefined;});
  await page.goto(`http://127.0.0.1:${server.address().port}/`);
  Object.assign(result, await page.evaluate(async () => {
    const data = await (await fetch('/data.json')).json();
    const {decodeAdpcm} = await import('/audio-adpcm.js');
    const context = new AudioContext({sampleRate: 48000, latencyHint: 'interactive'});
    function metrics(reference, decoded, frames) {
      if (reference.length !== decoded.length) throw new Error('Decoded sample count differs');
      let power = 0, error = 0, dot = 0, outPower = 0, max = 0, boundaryError = 0, boundaries = 0;
      for (let i = 0; i < decoded.length; i++) {
        if (!Number.isFinite(decoded[i])) throw new Error('Nonfinite output');
        const input = reference[i] / 32768, delta = decoded[i] - input;
        power += input ** 2;error += delta ** 2;dot += input * decoded[i];outPower += decoded[i] ** 2;
        max = Math.max(max, Math.abs(delta));
        if (i >= 2 && Math.floor(i / 2) % frames === 0) {
          boundaryError += Math.abs((decoded[i] - decoded[i - 2]) - (reference[i] - reference[i - 2]) / 32768);
          boundaries++;
        }
      }
      return {snr_db: 10 * Math.log10(power / Math.max(error, 1e-30)), rms: Math.sqrt(outPower / decoded.length),
        correlation: dot / Math.sqrt(power * outPower), max_error: max, boundary_delta_error_mean: boundaryError / boundaries};
    }
    const arms = [];
    try {
      for (const arm of data.arms) {
        let source = data.sources.find(s => s.name === arm.signal).pcm;
        if (arm.source_delay_frames) source = new Array(arm.source_delay_frames * 2).fill(0).concat(source.slice(0, -arm.source_delay_frames * 2));
        if (!['independent-ogg', 'rolling-ogg', 'ima-adpcm'].includes(arm.mode)) {
          arms.push({signal: arm.signal, mode: arm.mode, frames: arm.frames, encode_wall_ms: arm.encode_wall_ns / 1e6,
            payload_bytes: arm.chunks.reduce((n, v) => n + v.length, 0),
            quality: metrics(source, arm.native_reference, arm.frames)});
          continue;
        }
        const row = {signal: arm.signal, mode: arm.mode, frames: arm.frames, history_frames: arm.history_frames, files: arm.chunks.length,
          payload_bytes: arm.chunks.reduce((n, v) => n + v.length, 0), encode_wall_ms: arm.encode_wall_ns / 1e6,
          decode_wall_ms: [], decoded_frames: [], max_native_difference: 0, error: null};
        arms.push(row);
        try {
          const decoded = [];
          for (const chunk of arm.chunks) {
            const started = performance.now();
            let buffer;
            if (arm.mode === 'ima-adpcm') {
              const channels = decodeAdpcm(new Uint8Array(chunk), arm.frames);
              buffer = context.createBuffer(2, arm.frames, 48000);
              for (let c = 0; c < 2; c++) buffer.copyToChannel(channels[c], c);
            } else buffer = await context.decodeAudioData(new Uint8Array(chunk).buffer);
            row.decode_wall_ms.push(performance.now() - started);
            row.decoded_frames.push(buffer.length);
            if (buffer.sampleRate !== 48000 || buffer.numberOfChannels !== 2 || buffer.length !== arm.frames)
              throw new Error('Unexpected Web Audio decode duration or channels');
            const left = buffer.getChannelData(0), right = buffer.getChannelData(1);
            for (let i = 0; i < buffer.length; i++) decoded.push(left[i], right[i]);
          }
          row.quality = metrics(source, decoded, arm.frames);
          for (let i = 0; i < decoded.length; i++) row.max_native_difference = Math.max(row.max_native_difference, Math.abs(decoded[i] - arm.native_reference[i]));
        } catch (error) {row.error = error.message;}
      }
      return {webcodecs_disabled: globalThis.AudioDecoder === undefined && globalThis.EncodedAudioChunk === undefined,
        context_sample_rate: context.sampleRate, arms};
    } finally {await context.close();}
  }));
} catch (error) {result.error = error.message;}
finally {
  if (browser) await browser.close();
  for (const socket of sockets) socket.destroy();
  await new Promise(resolve => server.close(resolve));
}
await writeFile(output, JSON.stringify(result) + '\n', {flag: 'wx'});
console.log(JSON.stringify({...result, arms: result.arms.map(arm => ({...arm,
  decoded_frames: arm.decoded_frames ? [...new Set(arm.decoded_frames)] : undefined,
  decode_wall_ms: arm.decode_wall_ms ? {min: Math.min(...arm.decode_wall_ms), max: Math.max(...arm.decode_wall_ms),
    total: arm.decode_wall_ms.reduce((n, v) => n + v, 0)} : undefined}))}));
if (result.error || result.arms.length !== expectedArms || result.arms.some(arm => arm.error)) process.exitCode = 1;
