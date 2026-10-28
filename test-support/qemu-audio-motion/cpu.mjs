// Same counter arithmetic as the retained audio cost controller.
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {execFileSync} from 'node:child_process';
import {performance} from 'node:perf_hooks';
const hz=Number(execFileSync('getconf',['CLK_TCK'],{encoding:'utf8'}));
export async function processCounter(pid) {
  const begin = performance.now(), stat = await readFile(`/proc/${pid}/stat`, 'utf8');
  const fields = stat.slice(stat.lastIndexOf(')') + 2).trim().split(/\s+/);
  const allowed = (await readFile(`/proc/${pid}/status`, 'utf8')).match(/^Cpus_allowed_list:\s*(.+)$/m)?.[1];
  assert.equal(Number(fields[16]), 19, 'Owned measured process must retain nice 19');
  return {begin_ms: begin, end_ms: performance.now(), start: fields[19], allowed_cpus: allowed, ticks: Number(fields[11]) + Number(fields[12])};
}

export function processCpu(before, after) {
  assert.equal(before.start, after.start);assert.ok(after.ticks >= before.ticks);
  assert.equal(before.allowed_cpus, after.allowed_cpus);
  const elapsed = (after.begin_ms + after.end_ms - before.begin_ms - before.end_ms) / 2;
  return {scope: 'Owned process and its threads; independent midpoint counter window',
    allowed_cpus: before.allowed_cpus, ticks: after.ticks - before.ticks, ticks_per_second: hz, elapsed_ms: elapsed,
    percent_of_one_core: (after.ticks - before.ticks) / hz * 100000 / elapsed};
}

export async function browserCounter(cdp) {
  const begin = performance.now(), {processInfo} = await cdp.send('SystemInfo.getProcessInfo');
  const rows = await Promise.all(processInfo.map(async row => {
    const measured = await processCounter(row.id);assert.equal(measured.allowed_cpus, '0');
    return {start: measured.start, pid: row.id, cpu: row.cpuTime};
  }));
  return {begin_ms: begin, end_ms: performance.now(), rows};
}

export function browserCpu(before, after) {
  const first = new Map(before.rows.map(row => [`${row.pid}:${row.start}`, row.cpu]));
  const common = after.rows.filter(row => first.has(`${row.pid}:${row.start}`));
  const seconds = common.reduce((sum, row) => sum + row.cpu - first.get(`${row.pid}:${row.start}`), 0);
  const elapsed = (after.begin_ms + after.end_ms - before.begin_ms - before.end_ms) / 2;
  return {scope: 'CDP endpoint PID/start intersection; independent midpoint counter window',
    matched_processes: common.length, before_processes: before.rows.length, after_processes: after.rows.length,
    complete_endpoint_set: common.length === before.rows.length && common.length === after.rows.length,
    cpu_seconds: seconds, elapsed_ms: elapsed, percent_of_one_core: seconds * 100000 / elapsed};
}

