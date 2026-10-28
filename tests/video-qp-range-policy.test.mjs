import test from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {validateRangePlan} from '../test-support/video-qp-range/policy.mjs';
const plan=JSON.parse(await readFile(new URL('../benchmarks/video-qp-high-floor/plan.json',import.meta.url)));

test('QP pair policy: accepts the declared pair and rejects timing, source and budget drift',()=>{
  assert.equal(validateRangePlan(structuredClone(plan)).video_in_flight_frames,8);
  for (const alter of [p=>p.pulse_deadline_ms=701,p=>p.video_in_flight_frames=1,p=>p.fps_cap=15,
    p=>p.profiles[1].mbps=2,p=>p.source_alsa_buffer_us=80000,p=>p.window_ms=7000,p=>p.unknown=true]) {
    const changed=structuredClone(plan);alter(changed);assert.throws(()=>validateRangePlan(changed));
  }
});

test('QP pair policy: requires the declared bounded minimum-only ABBA comparison',()=>{
  for (const alter of [p=>p.order=[p.order[1],p.order[0],p.order[0],p.order[1]],p=>p.order[2]='floor36',p=>p.frame_skip.floor42=true,
    p=>p.qp_bounds.floor42.max=50,p=>p.qp_bounds.floor42.min=36,p=>p.qp_bounds.floor42.extra=0,
    p=>p.qp_bounds.floor42.min=52,p=>p.frame_skip.extra=false]) {
    const changed=structuredClone(plan);alter(changed);assert.throws(()=>validateRangePlan(changed));
  }
});
