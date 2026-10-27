// Opt-in cross-repository test. Auth/database and VM discovery are fixture
// boundaries; the React UI, FastAPI tunnel, Gliss process and QEMU audio are real.
import test from 'node:test';
import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {isAbsolute, join} from 'node:path';
import {freePort, launchBrowser} from '../../tests/helpers.mjs';
import {startAudioVm} from '../../test-support/qemu-audio/fixture.mjs';

async function startProcess(t, command, args, options, url) {
  const child = spawn(command, args, {...options, detached: true, stdio: ['ignore', 'pipe', 'pipe']});
  let output = '', failure;
  child.on('error', error => {failure = error;});
  for (const stream of [child.stdout, child.stderr]) stream.on('data', bytes => {output = (output + bytes).slice(-8192);});
  t.after(async () => {
    if (!child.pid) return;
    const signal = name => {try {process.kill(-child.pid, name);} catch (error) {if (error.code !== 'ESRCH') throw error;}};
    if (child.exitCode !== null || child.signalCode !== null) {signal('SIGKILL');return;}
    await new Promise(resolve => {
      const timer = setTimeout(() => signal('SIGKILL'), 4000);
      child.once('exit', () => {clearTimeout(timer);resolve();});signal('SIGTERM');
    });
  });
  const deadline = Date.now() + 30000;
  while (Date.now() < deadline) {
    if (failure || child.exitCode !== null) throw failure || new Error(output);
    try {if ((await fetch(url)).ok) return {output: () => output};} catch {}
    await new Promise(resolve => setTimeout(resolve, 30));
  }
  throw new Error(`Process did not become ready: ${output}`);
}

test('Distribox sound controls relay QEMU audio for local credentials and slave VM access', {timeout: 90000}, async t => {
  const root = process.env.DISTRIBOX_ROOT;
  assert.ok(root && isAbsolute(root), 'Set DISTRIBOX_ROOT to the audio-capable Distribox checkout');
  const vm = await startAudioVm(t);
  const apiPort = await freePort(), frontendPort = await freePort();
  const api = `http://127.0.0.1:${apiPort}`, frontend = `http://127.0.0.1:${frontendPort}`;
  const slaveId = '00000000-0000-4000-8000-000000000002';
  const env = {...process.env, PYTHONPATH: join(root, 'backend'),
    AWS_EC2_METADATA_DISABLED: 'true', AUDIO_TEST_PORT: String(vm.port), AUDIO_API_PORT: String(apiPort),
    AUDIO_FRONTEND: frontend, TMPDIR: vm.directory};
  const backendProcess = await startProcess(t, process.env.DISTRIBOX_PYTHON || 'python3', ['-c', `
import os
from types import SimpleNamespace
from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware
import uvicorn
from app.routes import tunnel
from app.services import slave_client

local_id = '00000000-0000-4000-8000-000000000001'
slave_id = '${slaveId}'
tunnel._find_vm_for_credential = lambda credential: local_id if credential == 'audio-fixture' else None
tunnel._resolve_vm_id_with_token = lambda vm_id, token: slave_id if vm_id == slave_id and token == 'audio-fixture-token' else None
tunnel.VmService._get_slave_for_vm = staticmethod(lambda vm_id: SimpleNamespace(hostname='127.0.0.1') if vm_id == slave_id else None)
tunnel.VNC_HOST = '127.0.0.1'
tunnel.get_vnc_port = lambda vm_id: int(os.environ['AUDIO_TEST_PORT'])
slave_client.slave_get_vnc_port = lambda slave, vm_id: int(os.environ['AUDIO_TEST_PORT'])
app = FastAPI()
app.add_middleware(CORSMiddleware, allow_origins=[os.environ['AUDIO_FRONTEND']], allow_methods=['*'], allow_headers=['*'])
app.include_router(tunnel.router)
uvicorn.run(app, host='127.0.0.1', port=int(os.environ['AUDIO_API_PORT']), log_level='warning')
`], {cwd: root, env}, `${api}/openapi.json`);
  const frontendProcess = await startProcess(t, process.execPath, [join(root, 'frontend/node_modules/@react-router/dev/bin.js'),
    'dev', '--host', '127.0.0.1', '--port', String(frontendPort), '--strictPort'], {
    cwd: join(root, 'frontend'), env: {...process.env, VITE_PORT: String(frontendPort), VITE_API_DOMAIN: api,
      VITE_TUNNEL_URL: api.replace('http', 'ws') + '/tunnel'},
  }, frontend);
  const browser = await launchBrowser();t.after(() => browser.close());
  for (const mode of ['credential', 'slave']) {
    const page = await browser.newPage(), errors = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.addInitScript(({pcm}) => {
      if (pcm) window.AudioDecoder = undefined;
      localStorage.setItem('auth_token', 'audio-fixture-token');
      window.audioProbe = {contexts: [], wireBytes: 0, frames: 0, videoPackets: 0, codec: null, tunnels: []};
      window.WebSocket = class extends WebSocket {
        constructor(...args) {
          super(...args);
          if (!String(args[0]).includes('/tunnel?')) return;
          window.audioProbe.tunnels.push(String(args[0]));
          this.addEventListener('message', event => {
            if (!(event.data instanceof ArrayBuffer) || event.data.byteLength < 4) return;
            const magic = new TextDecoder().decode(new Uint8Array(event.data, 0, 4));
            if (magic === 'GLV1') window.audioProbe.videoPackets++;
            if (magic !== 'GLA1') return;
            const header = new DataView(event.data), probe = window.audioProbe;
            probe.wireBytes += event.data.byteLength;probe.frames += header.getUint32(16, true);
            probe.codec = header.getUint8(4) === 1 ? 'opus' : 'pcm';
          });
        }
      };
      window.AudioContext = new Proxy(AudioContext, {construct(Target, args) {
        const context = new Target(...args), split = context.createChannelSplitter(2), mute = context.createGain();
        mute.gain.value = 0;mute.connect(context.destination);
        const analysers = [context.createAnalyser(), context.createAnalyser()];
        analysers.forEach((analyser, channel) => {analyser.fftSize = 4096;split.connect(analyser, channel);analyser.connect(mute);});
        const create = context.createBufferSource.bind(context);
        context.createBufferSource = () => {
          const source = create(), connect = source.connect.bind(source);
          source.connect = destination => {connect(split);return connect(destination);};return source;
        };
        window.audioProbe.contexts.push(context);
        window.audioProbe.signal = () => analysers.map(analyser => {
          const samples = new Float32Array(analyser.fftSize);analyser.getFloatTimeDomainData(samples);
          let energy = 0, crossings = 0;
          for (let n = 0; n < samples.length; n++) {energy += samples[n] ** 2;if (n && samples[n - 1] < 0 && samples[n] >= 0) crossings++;}
          return {rms: Math.sqrt(energy / samples.length), hz: crossings * context.sampleRate / samples.length};
        });
        return context;
      }});
    }, {pcm: mode === 'slave'});
    await page.goto(`${frontend}/client?credential=audio-fixture`);
    const enable = page.getByRole('button', {name: 'Enable sound', exact: true});
    await enable.waitFor({state: 'visible'}).catch(async error => {
      error.message += `\nUI: ${await page.locator('body').innerText()}\nPage errors: ${JSON.stringify(errors)}\nBackend: ${backendProcess.output()}\nFrontend: ${frontendProcess.output()}`;
      throw error;
    });
    if (mode === 'slave') {
      // Match dashboard navigation after hydration. Direct SSR loading of an
      // authenticated VM-id route has a separate, existing localStorage mismatch.
      await page.evaluate(id => {
        history.pushState(null, '', `/client?vm_id=${id}`);
        window.dispatchEvent(new PopStateEvent('popstate'));
      }, slaveId);
      await page.waitForFunction(() => window.audioProbe.tunnels.some(url => url.includes('vm_id=')));
      await enable.waitFor({state: 'visible'});
    }
    assert.equal(await page.evaluate(() => window.audioProbe.contexts.length), 0, 'Sound must remain off until the viewer clicks');
    await enable.click();
    await page.getByRole('button', {name: 'Mute sound', exact: true}).waitFor({state: 'visible'});
    await page.waitForFunction(() => window.audioProbe.frames >= 96000 && window.audioProbe.signal?.().every(signal => signal.rms > 0.05));
    const result = await page.evaluate(() => ({codec: window.audioProbe.codec,
      bitrate: window.audioProbe.wireBytes * 8 * 48000 / window.audioProbe.frames,
      videoPackets: window.audioProbe.videoPackets, signal: window.audioProbe.signal()}));
    assert.equal(result.codec, mode === 'credential' ? 'opus' : 'pcm');
    assert.ok(result.videoPackets > 0);
    assert.ok(Math.abs(result.signal[0].hz - 440) < 35, JSON.stringify(result));
    assert.ok(Math.abs(result.signal[1].hz - 880) < 35, JSON.stringify(result));
    assert.ok(result.bitrate < (result.codec === 'opus' ? 160000 : 1600000));
    await page.getByRole('button', {name: 'Mute sound', exact: true}).click();
    await enable.waitFor({state: 'visible'});
    await page.waitForFunction(() => window.audioProbe.contexts.every(context => context.state === 'closed'));
    assert.deepEqual(errors, []);
    t.diagnostic(JSON.stringify({mode, ...result}));
    await page.close();
  }
});
