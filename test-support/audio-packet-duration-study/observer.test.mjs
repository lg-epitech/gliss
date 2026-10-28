import test from 'node:test';
import assert from 'node:assert/strict';
import {startMotionWindow} from './observer.mjs';
const keys=['stale_raw_events','stale_raw_bytes','stale_pending_discards','stale_pending_bytes','reset_pending_discards','reset_pending_bytes','raw_lag_events','raw_lag_messages','encoder_init_errors','encoder_errors','emitted_packets','emitted_frames','emitted_wire_bytes','writer_stale_packets','writer_stale_bytes','writer_lag_events','writer_lag_messages'];
function packet(sequence,frames){const data=new ArrayBuffer(21),v=new DataView(data);v.setUint32(0,0x31414c47,true);v.setUint8(4,1);v.setUint8(5,2);v.setUint32(8,48000,true);v.setUint32(12,sequence,true);v.setUint32(16,frames,true);return data;}
async function setup(t,ms=20){
 let now=100;t.mock.method(performance,'now',()=>now);t.mock.timers.enable({apis:['setTimeout','setInterval']});
 const previous=Object.getOwnPropertyDescriptor(globalThis,'window');
 const client={ctx:{},metrics:{frames:0,bytes:0,errors:0},onEvent(){},socket:{send(){}},audio:{snapshot:()=>({})}};
 globalThis.window={gliss:client,readMotionStamp:()=>7,glissMarkerProbe:{failure:null,max_poll_gap_ms:20,quality_windows_dropped:0}};
 t.after(()=>{window.motionStudy.finish();if(previous)Object.defineProperty(globalThis,'window',previous);else delete globalThis.window;});
 await startMotionWindow({evaluate:async(fn,p)=>fn(p)},{window_ms:20000,control_probes_per_position:20,ping_interval_ms:900,sample_interval_ms:50,paint_cap_per_position:1024,audio_sample_cap_per_position:404,frame_skip:{current:false},current_mode:'current',audio_packet_ms:{current:ms}});
 return {time:value=>{now=value;},receive:data=>window.motionWireTap({data}),finish:()=>window.motionStudy.finish()};
}
for(const ms of [20,40])test(`audio duration observer validates ${ms}ms frames and wrap-safe sequence at the absolute cutoff`,async t=>{
 const h=await setup(t,ms);h.receive(packet(0xffffffff,ms*48));h.receive(packet(0,ms*48));
 h.receive(packet(1,ms===20?1920:960));h.time(20100);h.receive(packet(1,ms*48));
 const result=h.finish();assert.equal(result.wire_packets,2);assert.deepEqual(result.audio_arrivals.map(v=>v.sequence),[0xffffffff,0]);assert.equal(result.wire_frames,ms*48*2);assert.equal(result.wire_bytes,42);assert.equal(result.audio_sequence_gaps,0);assert.equal(result.invalid_audio_headers,1);
});
test('audio duration observer rejects malformed Opus headers and counts genuine gaps',async t=>{
 const h=await setup(t);h.receive(packet(10,960));h.receive(packet(12,960));
 for(const [offset,value] of [[4,0],[5,1],[6,1],[8,0]]){const p=packet(13,960);new DataView(p).setUint8(offset,value);h.receive(p);}
 h.receive(packet(13,960).slice(0,20));const result=h.finish();assert.equal(result.audio_sequence_gaps,1);assert.equal(result.invalid_audio_headers,5);assert.equal(result.wire_packets,2);
});
test('native audio samples require exact fields, safe monotone counters and the selected duration',async t=>{
 const h=await setup(t,40),zero=Object.fromEntries(keys.map(k=>[k,0]));
 const send=(c,ms=40)=>h.receive(JSON.stringify({type:'stream_stats',audio:{packet_ms:ms,counters:c}}));
 send(zero);send({...zero,emitted_packets:5});send({...zero,emitted_packets:4});send({...zero,writer_lag_messages:-1});send({...zero,writer_lag_messages:'1'});send({...zero,emitted_packets:6,extra:0});send(zero,20);send({...zero,emitted_packets:6});
 h.time(20100);send({...zero,emitted_packets:7});const r=h.finish();assert.equal(r.native_audio_samples.length,3);assert.equal(r.invalid_audio_stats,5);assert.deepEqual(r.native_audio_samples.at(-1).counters,{...zero,emitted_packets:6});
});
test('native telemetry has a fixed32-sample bound',async t=>{
 const h=await setup(t),counters=Object.fromEntries(keys.map(k=>[k,0]));for(let i=0;i<33;i++)h.receive(JSON.stringify({type:'stream_stats',audio:{packet_ms:20,counters}}));
 const r=h.finish();assert.equal(r.native_audio_samples.length,32);assert.equal(r.overflow,true);
});
