// Paired CPU-only component. No VM, native server or production service is used.
import assert from 'node:assert/strict';
import {createServer} from 'node:http';
import {readFile, lstat, readlink, writeFile} from 'node:fs/promises';
import {existsSync} from 'node:fs';
import {getPriority} from 'node:os';
import {createHash} from 'node:crypto';
import {launchBrowser} from '../tests/helpers.mjs';
import {ownResources} from '../test-support/audio-direct-fill/resources.mjs';
import {browserCounter, browserCpu} from '../test-support/qemu-audio-motion/cpu.mjs';

assert.equal(process.argv.length, 4);assert.equal(process.argv[2], '--output');
const output = process.argv[3], hash = raw => createHash('sha256').update(raw).digest('hex');
try {await lstat(output);throw new Error('Output exists');} catch (error) {if (error.code !== 'ENOENT') throw error;}
assert.equal(getPriority(), 19);
assert.equal((await readFile('/proc/self/status', 'utf8')).match(/^Cpus_allowed_list:\s*(.+)$/m)?.[1], '0');
assert.ok(!['/dev/dri', '/dev/nvidiactl', '/dev/kfd'].some(existsSync));
assert.match(process.env.GLISS_AUDIO_HOST_NETNS ?? '', /^net:\[[0-9]+\]$/);
assert.notEqual(await readlink('/proc/self/ns/net'), process.env.GLISS_AUDIO_HOST_NETNS);
const planName = 'benchmarks/audio-direct-fill-256/plan.json', planRaw = await readFile(planName), plan = JSON.parse(planRaw);
assert.deepEqual(plan.order, ['baseline', 'direct', 'direct', 'baseline']);
assert.deepEqual(plan.codecs, ['pcm', 'opus']);
assert.equal(plan.window_ms, 6000);assert.equal(plan.warmup_ms, 1500);assert.equal(plan.packet_frames, 960);
const sourceNames = ['scripts/bench-audio-direct-fill-256.mjs', planName,
  'test-support/audio-direct-fill/audio-player.js', 'test-support/audio-direct-fill/page.mjs',
  'test-support/audio-direct-fill/candidate.test.mjs', 'web/audio-player.js', 'web/audio-adpcm.js',
  'tests/helpers.mjs', 'test-support/qemu-audio-motion/cpu.mjs', 'package-lock.json',
  'test-support/audio-direct-fill/resources.mjs'];
const sources = new Map(await Promise.all(sourceNames.map(async name => [name, await readFile(name)])));
const result = {schema: 'gliss-audio-direct-fill-results-v1', plan_sha256: hash(planRaw),
  source_pins: Object.fromEntries([...sources].map(([name, bytes]) => [name, hash(bytes)])),
  sample_proofs: [], positions: [], cleanup_complete: false};
for (const codec of plan.codecs) for (const mode of plan.order)
  result.positions.push({slot: result.positions.length + 1, codec, mode, status: 'blocked'});
const files = new Map([...sources].filter(([name]) => name.endsWith('.js') || name.endsWith('.mjs'))
  .map(([name, bytes]) => ['/' + name, bytes]));
const html = '<!doctype html><button id="sound">Sound</button><script type="module" src="/test-support/audio-direct-fill/page.mjs"></script>';
const server = createServer((req, res) => {
  const headers = {'Cross-Origin-Opener-Policy': 'same-origin', 'Cross-Origin-Embedder-Policy': 'require-corp', 'Cache-Control': 'no-store'};
  if (req.url === '/') {res.writeHead(200, {...headers, 'Content-Type': 'text/html'});res.end(html);}
  else if (files.has(req.url)) {res.writeHead(200, {...headers, 'Content-Type': 'text/javascript'});res.end(files.get(req.url));}
  else {res.writeHead(404, headers);res.end();}
});
let browser, cdp;
const errors = [];
function phase(value) {result.phase = value;console.log('phase ' + value);}
async function open(mode, codec, pool, capture) {
  phase(`${capture ? 'proof' : 'cost'}-${codec}-${mode}-new-page`);
  const page = await browser.newPage();page.on('pageerror', error => errors.push(error.name));
  phase(`${capture ? 'proof' : 'cost'}-${codec}-${mode}-navigate`);
  await page.goto(`http://127.0.0.1:${server.address().port}`);
  phase(`${capture ? 'proof' : 'cost'}-${codec}-${mode}-module`);
  await page.waitForFunction(() => !!window.configure);
  phase(`${capture ? 'proof' : 'cost'}-${codec}-${mode}-configure`);
  await page.evaluate(({mode, codec, pool, capture}) => window.configure(mode, codec, pool, capture), {mode, codec, pool, capture});
  phase(`${capture ? 'proof' : 'cost'}-${codec}-${mode}-click`);
  await page.click('#sound');phase(`${capture ? 'proof' : 'cost'}-${codec}-${mode}-enable`);
  await page.evaluate(() => window.soundReady);
  return page;
}
try {
  result.resource_admission = await ownResources(plan.tasks_max, plan.memory_max_bytes);
  await new Promise((resolve, reject) => {server.once('error', reject);server.listen(0, '127.0.0.1', resolve);});
  browser = await launchBrowser({handleSIGINT: false, handleSIGTERM: false});
  result.browser_version = await browser.version();cdp = await browser.newBrowserCDPSession();
  const preparer = await browser.newPage();await preparer.goto(`http://127.0.0.1:${server.address().port}`);
  await preparer.waitForFunction(() => !!window.preparePool);
  const pool = await preparer.evaluate(() => window.preparePool());await preparer.close();
  result.pools = Object.fromEntries(Object.entries(pool).map(([codec, packets]) => [codec, {
    packets: packets.length, payload_bytes: packets.reduce((s, p) => s + p.length, 0),
    payload_sha256: hash(Buffer.concat(packets.map(p => Buffer.from(p))))}]));
  for (const codec of plan.codecs) {
    const proof = {codec, exact_samples_equal: false, modes: []};result.sample_proofs.push(proof);
    let reference;
    for (const mode of ['baseline', 'direct']) {
      const page = await open(mode, codec, pool[codec], true);
      try {
        phase(`proof-${codec}-${mode}-samples`);
        const observed = await page.evaluate(() => window.verifySamples());
        assert.equal(observed.captures.length, 8);
        const bytes = Buffer.concat(observed.captures.flat().map(words => {
          const out = Buffer.alloc(words.length * 4);words.forEach((value, i) => out.writeUInt32LE(value, i * 4));return out;
        }));
        if (reference) assert.deepEqual(bytes, reference);else reference = bytes;
        if (codec === 'pcm') {
          const expected = Buffer.alloc(8 * 960 * 2 * 4);let at = 0;
          for (let i = 0; i < 8; i++) for (let channel = 0; channel < 2; channel++) for (let f = 0; f < 960; f++) {
            expected.writeFloatLE(Buffer.from(pool.pcm[i]).readInt16LE((f * 2 + channel) * 2) / 32768, at);at += 4;
          }
          assert.deepEqual(bytes, expected);
        }
        proof.modes.push({mode, buffers: observed.captures.length, bytes: bytes.length, sha256: hash(bytes), snapshot: observed.snapshot});
      } finally {phase(`proof-${codec}-${mode}-stop`);await page.evaluate(() => window.stopAudio());await page.close();}
    }
    proof.exact_samples_equal = true;
  }
  for (const position of result.positions) {
    const page = await open(position.mode, position.codec, pool[position.codec], false);
    try {
      position.warmup = await page.evaluate(() => window.warmup());
      const before = await browserCounter(cdp);
      position.metrics = await page.evaluate(() => window.measure());
      const after = await browserCounter(cdp);position.cpu = browserCpu(before, after);
      position.resource_after = await ownResources(plan.tasks_max, plan.memory_max_bytes);
      const m = position.metrics;
      position.measurement_valid = m.isolated && position.cpu.complete_endpoint_set && m.errors.length === 0 && errors.length === 0 &&
        m.end_ms - m.start_ms === 6000 && m.sends.length === 300 && m.schedule_timings.length === 300 &&
        m.sends.every((row, i) => row.index === i && row.lateness_ms <= 100) &&
        m.schedule_timings.every(row => row.at_ms >= 0 && row.at_ms < 6000 && row.duration_ms >= 0) &&
        m.after.received - m.before.received === 300 && m.after.scheduled - m.before.scheduled === 300 &&
        m.after.dropped === m.before.dropped && m.after.sources <= 12 && m.after.queuedSeconds <= .2 && m.after.pendingDecodes === 0;
      position.status = 'measured';
      assert.ok(position.measurement_valid, 'Invalid component window');
      console.log(JSON.stringify({slot: position.slot, codec: position.codec, mode: position.mode,
        cpu_percent: position.cpu.percent_of_one_core, schedule_wall_ms: m.schedule_timings.reduce((sum, row) => sum + row.duration_ms, 0),
        packets: m.sends.length, valid: position.measurement_valid}));
    } finally {position.stopped = await page.evaluate(() => window.stopAudio());await page.close();}
  }
} catch (error) {result.failure = error.name;console.error(error);process.exitCode = 1;}
finally {
  try {if (browser) await browser.close();await new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve()));result.cleanup_complete = true;}
  catch {result.cleanup_complete = false;process.exitCode = 2;}
  result.page_errors = errors;
  await writeFile(output, JSON.stringify(result, null, 2) + '\n', {flag: 'wx'});
}
