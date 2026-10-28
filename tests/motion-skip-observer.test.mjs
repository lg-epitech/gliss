import test from 'node:test';
import assert from 'node:assert/strict';
import {startMotionWindow} from '../test-support/qemu-audio-motion/observer-skip.mjs';

test('frame-skip observer retains monotone counters, rejects malformed stats and counts actual wire gaps', async t => {
  let now = 100;
  t.mock.method(performance,'now',() => now);t.mock.timers.enable({apis:['setTimeout','setInterval']});
  const previous = Object.getOwnPropertyDescriptor(globalThis,'window');
  const client = {ctx:{},metrics:{frames:0,bytes:0,errors:0},onEvent() {},socket:{send() {}},audio:{snapshot:() => ({})}};
  globalThis.window = {gliss:client,readMotionStamp:() => 7,glissMarkerProbe:{failure:null,max_poll_gap_ms:20,quality_windows_dropped:0}};
  const plan = {window_ms:6000,control_probes_per_position:20,ping_interval_ms:300,sample_interval_ms:50,
    paint_cap_per_position:512,frame_skip:{disabled:false,enabled:true},current_mode:'enabled'};
  try {
    await startMotionWindow({evaluate:async(fn,value) => fn(value)},plan);
    const hook = window.motionWireTap;
    const stats = value => hook({data:JSON.stringify({type:'stream_stats',native_skipped_frames:value})});
    now = 200;stats(3);now = 1200;stats(6);stats(5);stats('7');
    for (const sequence of [0xffffffff,0,1,3]) {
      const data = new ArrayBuffer(20), view = new DataView(data);
      view.setUint32(0,0x31564c47,true);view.setUint32(4,sequence,true);hook({data});
    }
    now = 6100;stats(9);
    const result = window.motionStudy.finish();
    assert.deepEqual(result.skip_samples,[{at_ms:100,count:3},{at_ms:1100,count:6}]);
    assert.equal(result.invalid_skip_stats,2);assert.equal(result.sequence_gaps,1);
    now = 6200;stats(12);assert.equal(result.skip_samples.length,2);
    await startMotionWindow({evaluate:async(fn,value) => fn(value)},{...plan,current_mode:'disabled'});
    window.motionWireTap({data:JSON.stringify({type:'stream_stats'})});
    window.motionWireTap({data:JSON.stringify({type:'stream_stats',native_skipped_frames:0})});
    const disabled = window.motionStudy.finish();
    assert.deepEqual(disabled.skip_samples,[]);assert.equal(disabled.invalid_skip_stats,1);
  } finally {
    window.motionStudy?.finish();if (previous) Object.defineProperty(globalThis,'window',previous);else delete globalThis.window;
  }
});
