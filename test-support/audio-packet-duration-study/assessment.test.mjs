import test from 'node:test';
import assert from 'node:assert/strict';
import {assessAudioWindow} from './assessment.mjs';
const keys=['stale_raw_events','stale_raw_bytes','stale_pending_discards','stale_pending_bytes','reset_pending_discards','reset_pending_bytes','raw_lag_events','raw_lag_messages','encoder_init_errors','encoder_errors','emitted_packets','emitted_frames','emitted_wire_bytes','writer_stale_packets','writer_stale_bytes','writer_lag_events','writer_lag_messages'];
function good(ms){const packets=20000/ms,counters=Object.fromEntries(keys.map(k=>[k,0]));return {native_audio_samples:Array.from({length:20},(_,i)=>({at_ms:500+i*1000,packet_ms:ms,counters:{...counters,emitted_packets:50*i}})),invalid_audio_headers:0,audio_sequence_gaps:0,wire_packets:packets,wire_frames:960000,audio_before:{dropped:0,scheduled:3},audio_after:{dropped:0,scheduled:3+packets},invalid_audio_stats:0};}
test('both durations require equivalent delivered sample duration and explicit diagnostic span',()=>{for(const ms of [20,40]){const r=assessAudioWindow(good(ms),ms);assert.equal(r.valid,true);assert.equal(r.native_sample_span_ms,19000);assert.equal(r.counter_deltas.emitted_packets,950);}});
test('less sound, browser loss, decoder under-scheduling and missing native coverage deny CPU eligibility',()=>{
 for(const change of [m=>{m.wire_frames=480000;},m=>{m.audio_after.dropped=1;},m=>{m.audio_after.scheduled-=5;},m=>{m.native_audio_samples=[];},m=>{m.native_audio_samples=m.native_audio_samples.slice(4);},m=>{m.native_audio_samples.at(-1).counters.writer_lag_messages=1;}]){const m=good(40);change(m);assert.equal(assessAudioWindow(m,40).valid,false);}
});
test('a warmup counter value is retained without inventing a window increment',()=>{const m=good(40);for(const s of m.native_audio_samples)s.counters.stale_raw_events=2;const r=assessAudioWindow(m,40);assert.equal(r.valid,true);assert.equal(r.counter_deltas.stale_raw_events,0);});
