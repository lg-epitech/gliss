import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';

export async function ownResources(tasks, memory) {
  const relative = (await readFile('/proc/self/cgroup', 'utf8')).trim().match(/^0::(\/[^\n]*)$/)?.[1];
  assert.ok(relative && !relative.split('/').includes('..'));
  const root = '/sys/fs/cgroup' + relative, snapshot = {};
  for (const name of ['pids.current', 'pids.max', 'pids.peak', 'pids.events', 'memory.current', 'memory.max', 'memory.events']) {
    const raw = (await readFile(root + '/' + name, 'utf8')).trim();
    snapshot[name] = raw.includes(' ') ? Object.fromEntries(raw.split('\n').map(line => {
      const [key, value] = line.split(/\s+/);assert.match(value, /^\d+$/);return [key, Number(value)];
    })) : Number(raw);
  }
  assert.equal(snapshot['pids.max'], tasks);assert.equal(snapshot['memory.max'], memory);
  assert.equal(snapshot['pids.events'].max, 0, 'Thread admission failed');
  for (const key of ['max', 'oom', 'oom_kill']) assert.equal(snapshot['memory.events'][key], 0, 'Memory admission failed');
  return snapshot;
}
