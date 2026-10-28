// The only variable in a pair is requested minimum QP. Keep the workload fixed.
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
const base=JSON.parse(await readFile(new URL('../../benchmarks/video-qp/plan.json',import.meta.url)));
const variables=['schema','order','frame_skip','qp_bounds','qualification'];
const fixed=value=>Object.fromEntries(Object.entries(value).filter(([key])=>!variables.includes(key)));
export function validateRangePlan(plan) {
  assert.equal(plan.schema,'gliss-video-qp-range-plan-v1');
  assert.deepEqual(fixed(plan),fixed(base));
  assert.equal(typeof plan.qualification,'string');assert.ok(plan.qualification.length>0&&plan.qualification.length<2048);
  assert.ok(Array.isArray(plan.order)&&plan.order.length===4);
  const [low,high]=plan.order;assert.deepEqual(plan.order,[low,high,high,low]);assert.notEqual(low,high);
  assert.deepEqual(Object.keys(plan.qp_bounds).sort(),[low,high].sort());
  assert.deepEqual(Object.keys(plan.frame_skip).sort(),[low,high].sort());
  for (const name of [low,high]) {
    const q=plan.qp_bounds[name];assert.deepEqual(Object.keys(q).sort(),['max','min']);
    assert.ok(Number.isInteger(q.min)&&q.min>=1&&q.min<=51);assert.equal(q.max,51);
    assert.equal(name,'floor'+q.min);assert.equal(plan.frame_skip[name],false);
  }
  assert.ok(plan.qp_bounds[low].min<plan.qp_bounds[high].min);
  return plan;
}
