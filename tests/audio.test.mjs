import test from 'node:test';
import assert from 'node:assert/strict';
import {createServer} from 'node:http';
import {createHash} from 'node:crypto';
import {readFile} from 'node:fs/promises';
import {launchBrowser} from './helpers.mjs';

// A wire peer, not an AudioContext mock: tests exercise real browser playback,
// WebSocket binary delivery, autoplay, decoding and teardown without a VM.
async function audioPeer(t) {
  const sockets = new Set(), controls = [];
  let peer;
  const frame = (socket, payload, opcode) => {
    const body = typeof payload === 'string' ? Buffer.from(payload) : Buffer.from(payload);
    const header = Buffer.alloc(body.length < 126 ? 2 : 4);
    header[0] = 0x80 | opcode;
    if (body.length < 126) header[1] = body.length;
    else { header[1] = 126; header.writeUInt16BE(body.length, 2); }
    socket.write(Buffer.concat([header, body]));
  };
  const send = value => frame(peer, typeof value === 'object' && !Buffer.isBuffer(value) ? JSON.stringify(value) : value,
    Buffer.isBuffer(value) ? 2 : 1);
  const server = createServer(async (request, response) => {
    if (request.url === '/') {
      response.setHeader('Content-Type', 'text/html');
      response.end(`<canvas id="screen" tabindex="0"></canvas><button id="sound">Sound</button>
        <script type="module">
          import {GlissClient} from '/client.js';
          window.events=[];
          window.client=new GlissClient(document.querySelector('canvas'),()=>{},event=>window.events.push(event),{url:'ws://'+location.host+'/ws'});
          document.querySelector('#sound').onclick=()=>{window.soundRequest=window.client.setAudioEnabled(!window.client.audio.enabled);};
          window.client.connect();
        </script>`);
      return;
    }
    const name = request.url.slice(1);
    if (!/^[a-z-]+\.js$/.test(name)) { response.writeHead(404).end(); return; }
    try {
      const source = await readFile(new URL('../web/' + name, import.meta.url));
      response.setHeader('Content-Type', 'text/javascript');response.end(source);
    } catch { response.writeHead(404).end(); }
  });
  server.on('upgrade', (request, socket) => {
    sockets.add(socket);socket.on('close', () => sockets.delete(socket));socket.on('error', () => {});
    const accept = createHash('sha1').update(request.headers['sec-websocket-key'] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64');
    socket.write('HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ' + accept + '\r\n\r\n');
    peer = socket;
    let buffer = Buffer.alloc(0);
    socket.on('data', chunk => {
      buffer = Buffer.concat([buffer, chunk]);
      while (buffer.length >= 2) {
        const opcode = buffer[0] & 15, size = buffer[1] & 127;
        if (size === 127) { socket.destroy(); return; }
        const offset = size === 126 ? 4 : 2;
        if (buffer.length < offset) return;
        const length = size === 126 ? buffer.readUInt16BE(2) : size;
        if (buffer.length < offset + 4 + length) return;
        const mask = buffer.subarray(offset, offset + 4), body = Buffer.from(buffer.subarray(offset + 4, offset + 4 + length));
        buffer = buffer.subarray(offset + 4 + length);
        for (let index = 0; index < body.length; index++) body[index] ^= mask[index % 4];
        if (opcode === 8) { socket.end(); return; }
        if (opcode !== 1) continue;
        const control = JSON.parse(body.toString());controls.push(control);
        if (control.type === 'audio') send({type: 'audio_status', available: true, enabled: control.enabled, codec: control.codec});
      }
    });
    send({type: 'hello', codec: 'batch'});
    send({type: 'audio_status', available: true, enabled: false});
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(async () => {for (const socket of sockets) socket.destroy();await new Promise(resolve => server.close(resolve));});
  return {url: `http://127.0.0.1:${server.address().port}`, controls, send,
    close: () => {frame(peer, Buffer.from([3, 232]), 8);peer.end();}};
}

function pcmPacket(sequence, frames = 960) {
  const packet = Buffer.alloc(20 + frames * 4);
  packet.write('GLA1');packet[5] = 2;packet.writeUInt32LE(48000, 8);
  packet.writeUInt32LE(sequence, 12);packet.writeUInt32LE(frames, 16);
  for (let sample = 0; sample < frames; sample++) {
    packet.writeInt16LE(Math.round(Math.sin(sample * 2 * Math.PI * 440 / 48000) * 16000), 20 + sample * 4);
    packet.writeInt16LE(Math.round(Math.cos(sample * 2 * Math.PI * 660 / 48000) * 8000), 22 + sample * 4);
  }
  return packet;
}

async function openClient(t, peer, {pcm = true} = {}) {
  const browser = await launchBrowser();t.after(() => browser.close());
  const page = await browser.newPage();
  const errors = [];page.on('pageerror', error => errors.push(error.message));
  if (pcm) await page.addInitScript(() => {window.AudioDecoder = undefined;});
  await page.goto(peer.url);
  await page.waitForFunction(() => window.client.audio.available);
  t.after(() => assert.deepEqual(errors, []));
  return page;
}

test('audio: gesture enables real stereo PCM playback; mute releases audio and stops subscription', async t => {
  const peer = await audioPeer(t), page = await openClient(t, peer);
  assert.equal(peer.controls.some(control => control.type === 'audio'), false);
  peer.send(pcmPacket(0));
  assert.equal(await page.evaluate(() => window.client.audio.snapshot().scheduled), 0);
  await page.click('#sound');await page.evaluate(() => window.soundRequest);
  await page.waitForFunction(() => window.client.audio.enabled);
  assert.equal(peer.controls.find(control => control.type === 'audio')?.codec, 'pcm');
  await page.evaluate(() => {
    const context = window.client.audio.context;
    const analyser = context.createAnalyser(), mute = context.createGain();mute.gain.value = 0;
    analyser.fftSize = 2048;analyser.connect(mute).connect(context.destination);window.analyser = analyser;
    const create = context.createBufferSource.bind(context);
    context.createBufferSource = () => {
      const source = create(), connect = source.connect.bind(source);
      source.connect = destination => {connect(analyser);return connect(destination);};
      return source;
    };
  });
  for (let sequence = 0; sequence < 10; sequence++) {
    peer.send(pcmPacket(sequence));
    await new Promise(resolve => setTimeout(resolve, 20));
  }
  const result = await page.evaluate(() => {
    const samples = new Float32Array(window.analyser.fftSize);window.analyser.getFloatTimeDomainData(samples);
    return {...window.client.audio.snapshot(), rms: Math.sqrt(samples.reduce((sum, value) => sum + value * value, 0) / samples.length)};
  });
  assert.equal(result.codec, 'pcm');assert.ok(result.scheduled >= 10);assert.ok(result.rms > 0.05);
  assert.ok(result.queuedSeconds <= 0.2);assert.ok(result.sources <= 12);
  await page.click('#sound');await page.evaluate(() => window.soundRequest);
  const stopped = await page.evaluate(() => window.client.audio.snapshot());
  assert.equal(stopped.enabled, false);assert.equal(stopped.contextState, 'closed');assert.equal(stopped.sources, 0);
  assert.equal(peer.controls.filter(control => control.type === 'audio').at(-1)?.enabled, false);
  peer.send(pcmPacket(20));
  assert.equal(await page.evaluate(() => window.client.audio.snapshot().scheduled), result.scheduled);
});

test('audio: network bursts remain bounded and bypass a stalled video decode', async t => {
  const peer = await audioPeer(t), page = await openClient(t, peer);
  await page.click('#sound');await page.evaluate(() => window.soundRequest);
  await page.evaluate(() => {
    window.resumeVideo = null;
    window.client.decoder.decode = () => new Promise(resolve => {window.resumeVideo = () => resolve({width: 1, height: 1, pixels: new Uint8ClampedArray([20, 40, 60, 255]), sequence: 1, inputId: 0, bytes: 4});});
  });
  peer.send(Buffer.from('video'));
  await page.waitForFunction(() => window.resumeVideo !== null);
  for (let sequence = 0; sequence < 100; sequence++) peer.send(pcmPacket(sequence));
  await page.waitForFunction(() => window.client.audio.received === 100);
  const bounded = await page.evaluate(() => window.client.audio.snapshot());
  assert.ok(bounded.sources <= 12);assert.ok(bounded.queuedSeconds <= 0.2);assert.ok(bounded.dropped > 0);
  assert.equal(await page.evaluate(() => window.client.metrics.frames), 0);
  await page.evaluate(() => window.resumeVideo());
  await page.waitForFunction(() => window.client.metrics.frames === 1);
  assert.equal(await page.evaluate(() => window.client.metrics.errors), 0);
  assert.deepEqual(await page.evaluate(() => Array.from(window.client.ctx.getImageData(0, 0, 1, 1).data)), [20, 40, 60, 255]);
  peer.close();await page.waitForFunction(() => window.events.some(event => event.type === 'close'));
  assert.equal(await page.evaluate(() => window.client.audio.snapshot().contextState), 'closed');
  assert.equal(await page.evaluate(() => window.client.audio.sources.size), 0);
});

test('audio: malformed audio stops sound without requesting display recovery; reconnect starts muted', async t => {
  const peer = await audioPeer(t), page = await openClient(t, peer);
  await page.click('#sound');await page.evaluate(() => window.soundRequest);
  const malformed = pcmPacket(0);malformed.writeUInt32LE(1000000, 16);peer.send(malformed);
  await page.waitForFunction(() => window.events.some(event => event.type === 'audio_state' && event.message?.includes('Invalid audio')));
  assert.equal(await page.evaluate(() => window.client.audio.enabled), false);
  assert.equal(await page.evaluate(() => window.client.metrics.errors), 0);
  assert.equal(peer.controls.some(control => control.type === 'resync'), false);
  await page.click('#sound');await page.evaluate(() => window.soundRequest);
  const oldContext = await page.evaluate(async () => {
    window.oldAudioContext = window.client.audio.context;
    await window.client.connect();return window.oldAudioContext.state;
  });
  assert.equal(oldContext, 'closed');
  await page.waitForFunction(() => window.client.audio.available);
  assert.equal(await page.evaluate(() => window.client.audio.enabled), false);
  assert.equal(await page.evaluate(() => window.client.audio.context), null);
});

test('audio: native Opus decoding plays browser-encoded 48kHz stereo packets', async t => {
  const peer = await audioPeer(t), page = await openClient(t, peer, {pcm: false});
  const supported = await page.evaluate(async () => !!window.AudioEncoder && !!window.AudioDecoder &&
    (await AudioEncoder.isConfigSupported({codec: 'opus', sampleRate: 48000, numberOfChannels: 2, bitrate: 96000})).supported &&
    (await AudioDecoder.isConfigSupported({codec: 'opus', sampleRate: 48000, numberOfChannels: 2})).supported);
  assert.equal(supported, true, 'Test Chromium must provide native Opus encoding and decoding');
  await page.click('#sound');await page.evaluate(() => window.soundRequest);
  assert.equal(await page.evaluate(() => window.client.audio.codec), 'opus');
  const chunks = await page.evaluate(async () => {
    const chunks = [], encoder = new AudioEncoder({output: chunk => {
      const bytes = new Uint8Array(chunk.byteLength);chunk.copyTo(bytes);chunks.push(Array.from(bytes));
    }, error: error => {throw error;}});
    encoder.configure({codec: 'opus', sampleRate: 48000, numberOfChannels: 2, bitrate: 96000});
    for (let index = 0; index < 8; index++) {
      const samples = new Float32Array(1920);
      for (let frame = 0; frame < 960; frame++) {samples[frame] = Math.sin((frame + index * 960) * 2 * Math.PI * 440 / 48000) * 0.4;samples[960 + frame] = samples[frame] * 0.5;}
      const audio = new AudioData({format: 'f32-planar', sampleRate: 48000, numberOfFrames: 960, numberOfChannels: 2, timestamp: index * 20000, data: samples});
      encoder.encode(audio);audio.close();
    }
    await encoder.flush();encoder.close();return chunks;
  });
  let sequence = 0;
  for (const chunk of chunks) {
    const header = pcmPacket(sequence++).subarray(0, 20);header[4] = 1;
    peer.send(Buffer.concat([header, Buffer.from(chunk)]));
    await new Promise(resolve => setTimeout(resolve, 20));
  }
  await page.waitForFunction(() => window.client.audio.scheduled >= 8);
  const result = await page.evaluate(() => window.client.audio.snapshot());
  assert.equal(result.codec, 'opus');assert.equal(result.enabled, true);assert.equal(result.decodeQueue, 0);
  assert.ok(result.queuedSeconds <= 0.2);assert.ok(result.sources <= 12);
});

test('audio: autoplay denial releases context and a later click can enable sound', async t => {
  const peer = await audioPeer(t), browser = await launchBrowser();t.after(() => browser.close());
  const page = await browser.newPage();await page.addInitScript(() => {window.AudioDecoder = undefined;});await page.goto(peer.url);
  // Playwright evaluation marks execution as a user gesture; CDP lets this test
  // exercise the browser's actual no-activation policy.
  const cdp = await page.context().newCDPSession(page);
  await cdp.send('Runtime.evaluate', {expression: 'new Promise(resolve=>{const timer=setInterval(()=>{if(window.client?.audio.available){clearInterval(timer);resolve();}},10);})', awaitPromise: true, userGesture: false});
  await cdp.send('Runtime.evaluate', {expression: 'window.client.setAudioEnabled(true)', awaitPromise: true, userGesture: false});
  await cdp.detach();
  const blocked = await page.evaluate(() => ({state: window.client.audio.snapshot(), event: window.events.filter(event => event.type === 'audio_state').at(-1)}));
  assert.equal(blocked.state.enabled, false);assert.equal(blocked.state.contextState, 'closed');assert.equal(blocked.event.blocked, true);
  assert.equal(peer.controls.some(control => control.type === 'audio' && control.enabled), false);
  await page.click('#sound');await page.evaluate(() => window.soundRequest);
  assert.equal(await page.evaluate(() => window.client.audio.enabled), true);
  peer.send(pcmPacket(0));await page.waitForFunction(() => window.client.audio.scheduled === 1);
});

test('audio: mute cancels pending activation and delayed mute acknowledgement cannot cancel a new activation', async t => {
  const peer = await audioPeer(t), page = await openClient(t, peer, {pcm: false});
  await page.evaluate(() => {
    window.originalAudioSupport = AudioDecoder.isConfigSupported;
    AudioDecoder.isConfigSupported = () => new Promise(resolve => {window.finishAudioSupport = () => resolve({supported: true});});
  });
  await page.click('#sound');await page.waitForFunction(() => !!window.finishAudioSupport);
  await page.evaluate(() => {void window.client.setAudioEnabled(false);window.finishAudioSupport();});
  await page.evaluate(() => window.soundRequest);
  assert.equal(await page.evaluate(() => window.client.audio.snapshot().contextState), 'closed');
  assert.equal(peer.controls.some(control => control.type === 'audio' && control.enabled), false);
  await page.evaluate(() => {AudioDecoder.isConfigSupported = window.originalAudioSupport;});
  await page.click('#sound');await page.evaluate(() => window.soundRequest);
  peer.send({type: 'audio_status', available: true, enabled: false});
  await page.waitForFunction(() => window.events.filter(event => event.type === 'audio_status' && !event.enabled).length >= 3);
  assert.equal(await page.evaluate(() => window.client.audio.enabled), true);
});
