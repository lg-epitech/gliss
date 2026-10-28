// Owned, opt-in kernel TCP treatment inside a fresh network namespace only.
// The RFB port and all other traffic remain in the unshaped third band.
import assert from 'node:assert/strict';
import {readlink} from 'node:fs/promises';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';

const exec = promisify(execFile);
const tc = async args => (await exec('tc', args, {timeout: 2000, maxBuffer: 131072})).stdout;
const qdiscs = async () => JSON.parse(await tc(['-s', '-j', 'qdisc', 'show', 'dev', 'lo']));

export async function startKernelLink({targetPort, rttMs, mbps}) {
  assert.ok(Number.isInteger(targetPort) && targetPort > 0 && targetPort <= 65535);
  assert.equal(rttMs, 80);assert.equal(mbps, 1);
  const host = process.env.GLISS_AUDIO_HOST_NETNS;
  assert.match(host ?? '', /^net:\[[0-9]+\]$/);
  assert.notEqual(await readlink('/proc/self/ns/net'), host, 'Kernel study requires a fresh network namespace');
  assert.ok((await qdiscs()).every(row => row.kind === 'noqueue'), 'Refuse to replace an existing queue');
  let installed = false, stopping, cleanup;
  const snapshot = async () => {
    const rows = await qdiscs();
    const root = rows.find(row => row.handle === '1:');
    assert.equal(root?.kind, 'prio');assert.equal(root.root, true);
    assert.equal(root.options.bands, 3);assert.deepEqual(root.options.priomap, Array(16).fill(2));
    const selected = [];
    for (const [handle, parent, direction] of [['10:', '1:1', 'server-to-browser'], ['20:', '1:2', 'browser-to-server']]) {
      const row = rows.find(row => row.handle === handle);
      assert.equal(row?.kind, 'netem');assert.equal(row.parent, parent);
      assert.equal(row.options.limit, 1024);assert.equal(row.options.delay.delay, 0.04);
      assert.equal(row.options.delay.jitter, 0);assert.equal(row.options.rate.rate, 125000);
      selected.push({handle, parent, direction, limit_packets: row.options.limit, delay_seconds: row.options.delay.delay,
        rate_bytes_per_second: row.options.rate.rate, bytes: row.bytes, packets: row.packets,
        drops: row.drops, backlog: row.backlog, qlen: row.qlen});
    }
    return {scope: 'Cumulative kernel queue counters including HTTP and warmup; native TCP data and ACKs are delayed.',
      target_port: targetPort, queues: selected, filters: await tc(['filter', 'show', 'dev', 'lo', 'parent', '1:'])};
  };
  const stop = () => stopping ??= (async () => {
    if (!installed) return;
    const before = await snapshot();
    await tc(['qdisc', 'del', 'dev', 'lo', 'root']);
    const after = await qdiscs();
    assert.ok(after.every(row => row.kind === 'noqueue'), 'Owned kernel queues remain after removal');
    cleanup = {before_removal: before, root_removed: true};
  })();
  try {
    await tc(['qdisc', 'add', 'dev', 'lo', 'root', 'handle', '1:', 'prio', 'bands', '3', 'priomap', ...Array(16).fill('2')]);
    installed = true;
    for (const [parent, handle] of [['1:1', '10:'], ['1:2', '20:']])
      await tc(['qdisc', 'add', 'dev', 'lo', 'parent', parent, 'handle', handle, 'netem',
        'delay', '40ms', 'rate', '1mbit', 'limit', '1024']);
    for (const [priority, portField, flow] of [['1', 'sport', '1:1'], ['2', 'dport', '1:2']])
      await tc(['filter', 'add', 'dev', 'lo', 'protocol', 'ip', 'parent', '1:', 'prio', priority,
        'u32', 'match', 'ip', 'protocol', '6', '0xff', 'match', 'ip', portField, String(targetPort), '0xffff', 'flowid', flow]);
    await snapshot();
    return {url: `http://127.0.0.1:${targetPort}`, snapshot, stop, get cleanup() {return cleanup;}};
  } catch (error) {
    if (installed) await tc(['qdisc', 'del', 'dev', 'lo', 'root']);
    throw error;
  }
}
