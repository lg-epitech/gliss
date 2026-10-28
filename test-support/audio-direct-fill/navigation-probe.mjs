// Separate setup diagnostic. Its five-second navigation bound is not a media gate.
import assert from 'node:assert/strict';
import {createServer} from 'node:http';
import {readFile, readlink, lstat, writeFile} from 'node:fs/promises';
import {existsSync} from 'node:fs';
import {getPriority} from 'node:os';
import {createHash} from 'node:crypto';
import {launchBrowser} from '../../tests/helpers.mjs';

assert.equal(process.argv.length, 6);assert.equal(process.argv[2], '--tasks');assert.equal(process.argv[4], '--output');
const expectedTasks = Number(process.argv[3]), output = process.argv[5];
assert.ok([128, 256].includes(expectedTasks));assert.equal(getPriority(), 19);
assert.equal((await readFile('/proc/self/status', 'utf8')).match(/^Cpus_allowed_list:\s*(.+)$/m)?.[1], '0');
assert.ok(!['/dev/dri', '/dev/nvidiactl', '/dev/kfd'].some(existsSync));
assert.match(process.env.GLISS_AUDIO_HOST_NETNS ?? '', /^net:\[[0-9]+\]$/);
assert.notEqual(await readlink('/proc/self/ns/net'), process.env.GLISS_AUDIO_HOST_NETNS);
try {await lstat(output);throw new Error('Output exists');} catch (error) {if (error.code !== 'ENOENT') throw error;}
const relative = (await readFile('/proc/self/cgroup', 'utf8')).trim().match(/^0::(\/[^\n]*)$/)?.[1];
assert.ok(relative && !relative.split('/').includes('..'));
const cgroup = '/sys/fs/cgroup' + relative;
const hash = raw => createHash('sha256').update(raw).digest('hex');
async function audit() {
  const values = {};
  for (const file of ['pids.current', 'pids.max', 'pids.peak', 'pids.events', 'memory.current', 'memory.max', 'memory.events']) {
    const raw = (await readFile(cgroup + '/' + file, 'utf8')).trim();
    values[file] = raw.includes(' ') ? Object.fromEntries(raw.split('\n').map(line => {
      const [key, value] = line.split(/\s+/);return [key, Number(value)];
    })) : raw === 'max' ? 'max' : Number(raw);
  }
  assert.equal(values['pids.max'], expectedTasks);return values;
}
const result = {schema: 'gliss-audio-navigation-probe-v1', tasks_max: expectedTasks,
  sources: {}, checkpoints: [], sample_proofs: [], cost_positions_executed: 0, cleanup_complete: false};
const names = ['test-support/audio-direct-fill/navigation-probe.mjs', 'test-support/audio-direct-fill/page.mjs',
  'test-support/audio-direct-fill/audio-player.js', 'web/audio-player.js', 'web/audio-adpcm.js', 'tests/helpers.mjs', 'package-lock.json'];
const files = new Map();
for (const name of names) {const raw = await readFile(name);result.sources[name] = hash(raw);files.set('/' + name, raw);}
const server = createServer((req, res) => {
  const headers = {'Cross-Origin-Opener-Policy': 'same-origin', 'Cross-Origin-Embedder-Policy': 'require-corp', 'Cache-Control': 'no-store'};
  if (req.url === '/') {
    res.writeHead(200, {...headers, 'Content-Type': 'text/html'});
    res.end('<!doctype html><button id="sound">Sound</button><script type="module" src="/test-support/audio-direct-fill/page.mjs"></script>');
  } else if (files.has(req.url)) {res.writeHead(200, {...headers, 'Content-Type': 'text/javascript'});res.end(files.get(req.url));}
  else {res.writeHead(404, headers);res.end();}
});
let browser, phase = 'start';
async function checkpoint(label) {
  assert.ok(result.checkpoints.length < 64);phase = label;
  const row = {phase: label, audit: await audit()};result.checkpoints.push(row);console.log(JSON.stringify(row));
}
async function open(label) {
  await checkpoint(label + '-before-new-page');const page = await browser.newPage();
  page.setDefaultTimeout(5000);page.setDefaultNavigationTimeout(5000);
  await checkpoint(label + '-before-navigation');await page.goto(`http://127.0.0.1:${server.address().port}`);
  await page.waitForFunction(() => !!window.configure);await checkpoint(label + '-ready');return page;
}
try {
  await checkpoint('before-listen');
  await new Promise((resolve, reject) => {server.once('error', reject);server.listen(0, '127.0.0.1', resolve);});
  browser = await launchBrowser({handleSIGINT: false, handleSIGTERM: false});result.browser_version = await browser.version();
  const preparer = await open('prepare');const pool = await preparer.evaluate(() => window.preparePool());await preparer.close();
  await checkpoint('prepare-closed');
  for (const codec of ['pcm', 'opus']) for (const mode of ['baseline', 'direct']) {
    const label = codec + '-' + mode, page = await open(label);
    try {
      await page.evaluate(({mode, codec, pool}) => window.configure(mode, codec, pool, true), {mode, codec, pool: pool[codec]});
      await page.click('#sound');await page.evaluate(() => window.soundReady);await checkpoint(label + '-enabled');
      const value = await page.evaluate(() => window.verifySamples());
      result.sample_proofs.push({codec, mode, buffers: value.captures.length,
        samples_sha256: hash(JSON.stringify(value.captures)), snapshot: value.snapshot});
      await checkpoint(label + '-samples');
    } finally {await page.evaluate(() => window.stopAudio());await page.close();}
    await checkpoint(label + '-closed');
  }
  const firstCostPage = await open('cost-pcm-baseline');await firstCostPage.close();
  result.setup_passed = true;
} catch (error) {
  result.failure = {phase, name: error.name};
  try {await checkpoint('failed');} catch {}
  process.exitCode = 1;
} finally {
  try {if (browser) await browser.close();await new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve()));result.cleanup_complete = true;}
  catch {process.exitCode = 2;}
  try {await checkpoint('terminal');} catch {}
  await writeFile(output, JSON.stringify(result, null, 2) + '\n', {flag: 'wx'});
}
