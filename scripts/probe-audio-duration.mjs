// Prerequisite only: actual native Opus -> current browser player accounting.
import assert from 'node:assert/strict';
import {createServer} from 'node:http';
import {readFile, writeFile, lstat, readlink} from 'node:fs/promises';
import {existsSync} from 'node:fs';
import {getPriority} from 'node:os';
import {createHash} from 'node:crypto';
import {launchBrowser} from '../tests/helpers.mjs';
import {ownResources} from '../test-support/audio-direct-fill/resources.mjs';

assert.equal(process.argv.length, 6);assert.equal(process.argv[2], '--native');assert.equal(process.argv[4], '--output');
const hash = raw => createHash('sha256').update(raw).digest('hex'), output = process.argv[5];
try {await lstat(output);throw new Error('Output exists');} catch (error) {if (error.code !== 'ENOENT') throw error;}
assert.equal(getPriority(), 19);
assert.equal((await readFile('/proc/self/status', 'utf8')).match(/^Cpus_allowed_list:\s*(.+)$/m)?.[1], '0');
assert.ok(!['/dev/dri', '/dev/nvidiactl', '/dev/kfd'].some(existsSync));
assert.match(process.env.GLISS_AUDIO_HOST_NETNS ?? '', /^net:\[[0-9]+\]$/);
assert.notEqual(await readlink('/proc/self/ns/net'), process.env.GLISS_AUDIO_HOST_NETNS);
const raw = await readFile(process.argv[3]), native = JSON.parse(raw);
assert.equal(native.schema, 'gliss-audio-duration-prerequisite-v1');
assert.deepEqual(native.arms.map(a => a.packet_frames), [960, 1920, 1920, 960]);
const result = {schema: 'gliss-audio-duration-browser-prerequisite-v1', native_report_sha256: hash(raw),
  sources: {}, positions: [], cleanup_complete: false};
const paths = ['scripts/probe-audio-duration.mjs', 'test-support/audio-duration/page.mjs',
  'web/audio-player.js', 'web/audio-adpcm.js', 'tests/helpers.mjs',
  'test-support/audio-direct-fill/resources.mjs', 'package-lock.json'];
const files = new Map();
for (const name of paths) {const bytes = await readFile(name);result.sources[name] = hash(bytes);files.set('/' + name, bytes);}
for (const arm of native.arms) {
  const packets = arm.chunks.map(bytes => Buffer.from(bytes));
  assert.equal(packets.length, 15360 / arm.packet_frames + 1);
  packets.forEach((p, i) => {
    assert.equal(p.subarray(0, 4).toString(), 'GLA1');assert.equal(p[4], 1);assert.equal(p[5], 2);
    assert.equal(p.readUInt16LE(6), 0);assert.equal(p.readUInt32LE(8), 48000);
    assert.equal(p.readUInt32LE(12), i);assert.equal(p.readUInt32LE(16), arm.packet_frames);
    assert.ok(p.length > 20 && p.length <= 1295);
  });
  result.positions.push({slot: result.positions.length + 1, packet_frames: arm.packet_frames,
    packets: packets.length, wire_sha256: hash(Buffer.concat(packets)), status: 'blocked'});
}
const server = createServer((req, res) => {
  res.setHeader('Cache-Control', 'no-store');
  if (req.url === '/') {res.setHeader('Content-Type', 'text/html');res.end('<!doctype html><button id="sound">Sound</button><script type="module" src="/test-support/audio-duration/page.mjs"></script>');}
  else if (files.has(req.url)) {res.setHeader('Content-Type', 'text/javascript');res.end(files.get(req.url));}
  else {res.writeHead(404);res.end();}
});
let browser;
try {
  result.resource_admission = await ownResources(256, 2147483648);
  await new Promise((resolve, reject) => {server.once('error', reject);server.listen(0, '127.0.0.1', resolve);});
  browser = await launchBrowser({handleSIGINT: false, handleSIGTERM: false});result.browser_version = await browser.version();
  for (const position of result.positions) {
    const page = await browser.newPage(), errors = [];page.on('pageerror', error => errors.push(error.name));
    try {
      await page.goto(`http://127.0.0.1:${server.address().port}`);await page.waitForFunction(() => !!window.runPackets);
      await page.click('#sound');assert.equal(await page.evaluate(() => window.soundReady), 'opus');
      position.metrics = await page.evaluate(packets => window.runPackets(packets), native.arms[position.slot - 1].chunks);
      position.page_errors = errors;position.resource_after = await ownResources(256, 2147483648);
      const m = position.metrics, expected = position.packets;
      position.passed = m.errors.length === 0 && errors.length === 0 && m.sends.length === expected &&
        m.outputs.length === expected && m.schedules.length === expected &&
        m.outputs.every((row, i) => row.frames === position.packet_frames && row.channels === 2 && row.sample_rate === 48000 &&
          Math.abs(row.timestamp - m.sends[i].timestamp) <= 1) && m.schedules.every(frames => frames === position.packet_frames) &&
        m.snapshot.received === expected && m.snapshot.scheduled === expected && m.snapshot.dropped === 0 &&
        m.snapshot.pendingDecodes === 0 && m.snapshot.enabled && m.snapshot.sources <= 12 && m.snapshot.queuedSeconds <= .2;
      position.status = 'measured';console.log(JSON.stringify({slot: position.slot, frames: position.packet_frames,
        packets: expected, output_frames: m.outputs.map(row => row.frames), passed: position.passed}));
    } catch (error) {position.failure = error.name;position.status = 'failed';process.exitCode = 1;}
    finally {
      try {position.stopped = await page.evaluate(() => window.stopAudio());await page.close();}
      catch {position.cleanup_failed = true;process.exitCode = 2;}
    }
    if (position.cleanup_failed) break;
  }
} catch (error) {result.failure = error.name;process.exitCode = 1;}
finally {
  try {if (browser) await browser.close();await new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve()));result.cleanup_complete = true;}
  catch {process.exitCode = 2;}
  if (!result.positions.every(p => p.passed)) process.exitCode ||= 1;
  await writeFile(output, JSON.stringify(result, null, 2) + '\n', {flag: 'wx'});
}
