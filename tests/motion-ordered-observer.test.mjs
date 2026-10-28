import test from 'node:test';
import assert from 'node:assert/strict';
import {startMotionWindow} from '../test-support/qemu-audio-motion/observer-budget.mjs';

test('ordered motion observer: constructor delivery is before direct dispatch and exact cutoff is retained', async t => {
  let now = 100;
  t.mock.method(performance,'now',() => now);t.mock.timers.enable({apis:['setTimeout','setInterval']});
  const previous = Object.getOwnPropertyDescriptor(globalThis,'window'), sent = [];
  const client = {ctx:{},metrics:{frames:0,bytes:0,errors:0},onEvent() {},socket:{send:text => sent.push(JSON.parse(text))},
    audio:{snapshot:() => ({enabled:true,sources:3,queuedSeconds:0.06})}};
  globalThis.window = {gliss:client,readMotionStamp:() => 7,glissMarkerProbe:{failure:null,max_poll_gap_ms:20,quality_windows_dropped:0}};
  try {
    await startMotionWindow({evaluate:async(fn,plan) => fn(plan)},
      {window_ms:6000,control_probes_per_position:20,ping_interval_ms:300,sample_interval_ms:50,paint_cap_per_position:512});
    assert.deepEqual(sent,[{type:'ping',id:0}]);
    const hook = window.motionWireTap;
    now = 180;hook({data:JSON.stringify({type:'pong',id:0})});
    now = 180.25;client.onEvent({type:'pong',id:0});
    now = 6100;hook({data:JSON.stringify({type:'pong',id:1})});client.onEvent({type:'pong',id:1});
    const result = window.motionStudy.finish();
    assert.equal(result.pings[0].wire_received_ms,80);assert.equal(result.pings[0].dispatched_ms,80.25);
    assert.equal(result.pings[1].wire_received_ms,null);assert.equal(result.pings[1].dispatched_ms,null);
    assert.equal(window.motionWireTap,null);
    now = 6200;hook({data:JSON.stringify({type:'pong',id:0})});
    assert.equal(result.pings[0].wire_received_ms,80);
  } finally {
    window.motionStudy?.finish();if (previous) Object.defineProperty(globalThis,'window',previous);else delete globalThis.window;
  }
});
