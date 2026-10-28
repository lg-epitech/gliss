import test from 'node:test';
import assert from 'node:assert/strict';
import {startMotionWindow} from '../test-support/video-qp/observer.mjs';

test('QP observer: retains IDR flags and rejects malformed video headers within the original cutoff',async t=>{
  let now=100;t.mock.method(performance,'now',()=>now);t.mock.timers.enable({apis:['setTimeout','setInterval']});
  const previous=Object.getOwnPropertyDescriptor(globalThis,'window');
  const client={ctx:{},metrics:{frames:0,bytes:0,errors:0},onEvent(){},socket:{send(){}},audio:{snapshot:()=>({})}};
  globalThis.window={gliss:client,readMotionStamp:()=>7,glissMarkerProbe:{failure:null,max_poll_gap_ms:20,quality_windows_dropped:0}};
  const packet=(sequence,flags=0,reserved=0,length=1)=>{
    const b=new ArrayBuffer(25),v=new DataView(b);v.setUint32(0,0x31564c47,true);v.setUint32(4,sequence,true);
    v.setUint16(16,reserved,true);v.setUint16(18,flags,true);v.setUint32(20,length,true);return b;
  };
  try {
    await startMotionWindow({evaluate:async(fn,p)=>fn(p)},{window_ms:6000,control_probes_per_position:20,
      ping_interval_ms:300,sample_interval_ms:50,paint_cap_per_position:512,frame_skip:{default:false},current_mode:'default'});
    now=200;window.motionWireTap({data:packet(1,1)});window.motionWireTap({data:packet(2)});
    window.motionWireTap({data:packet(3,2)});window.motionWireTap({data:packet(3,0,1)});window.motionWireTap({data:packet(3,0,0,2)});
    now=6100;window.motionWireTap({data:packet(3,1)});
    const result=window.motionStudy.finish();
    assert.deepEqual(result.arrivals.map(p=>[p.sequence,p.keyframe]),[[1,true],[2,false]]);
    assert.equal(result.video_wire_bytes,50);assert.equal(result.invalid_video_headers,3);assert.equal(result.sequence_gaps,0);
  } finally {
    window.motionStudy?.finish();if(previous)Object.defineProperty(globalThis,'window',previous);else delete globalThis.window;
  }
});
