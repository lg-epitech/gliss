import test from 'node:test';
import assert from 'node:assert/strict';
import {startMotionWindow} from '../test-support/qemu-audio-motion/observer.mjs';

async function harness(t) {
  let now = 100;
  t.mock.method(performance,'now',() => now);
  t.mock.timers.enable({apis:['setTimeout','setInterval']});
  const old = Object.getOwnPropertyDescriptor(globalThis,'window');
  const sent = [], original = [];
  const socket = new EventTarget();socket.send = value => sent.push(JSON.parse(value));
  const client = {socket,ctx:{},metrics:{frames:0,bytes:0,errors:0},onEvent:value => original.push(value),
    audio:{snapshot:() => ({enabled:true,sources:3,queuedSeconds:0.06})}};
  globalThis.window = {gliss:client,readMotionStamp:() => 20,glissMarkerProbe:{failure:null,max_poll_gap_ms:20,quality_windows_dropped:0}};
  const page = {evaluate:async (fn,plan) => fn(plan)};
  const plan = {window_ms:6000,control_probes_per_position:20,ping_interval_ms:300,sample_interval_ms:50,paint_cap_per_position:512};
  const interval = await startMotionWindow(page,plan);
  t.after(() => {window.motionStudy.finish();if (old) Object.defineProperty(globalThis,'window',old);else delete globalThis.window;});
  return {client,sent,original,interval,now: value => {now = value;},
    wire(value) {socket.dispatchEvent(new MessageEvent('message',{data:typeof value === 'string' ? value : value}));},
    finish:() => window.motionStudy.finish()};
}

test('motion observer: wire and delayed dispatch remain separate, with exact source/presentation records', async t => {
  const h = await harness(t);assert.deepEqual(h.sent,[{type:'ping',id:0}]);
  h.now(180);h.wire(JSON.stringify({type:'pong',id:0}));
  h.now(430);h.client.onEvent({type:'pong',id:0});
  h.wire(JSON.stringify({type:'pong',id:99999}));h.client.onEvent({type:'pong',id:99999});
  const packet = new ArrayBuffer(20), view = new DataView(packet);view.setUint32(0,0x31564c47,true);view.setUint32(4,7,true);
  h.now(500);h.wire(packet);
  h.now(540);h.client.presentationState = {presentedAtMs:535};
  h.client.onEvent({type:'frame',frame:{sequence:7,bytes:20},decodeMs:4});
  h.now(6100);const report = h.finish();
  assert.deepEqual(report.pings[0],{id:0,planned_ms:0,sent_ms:0,wire_received_ms:80,dispatched_ms:330});
  assert.deepEqual(report.paints,[{at_ms:435,observed_ms:440,source_frame:20,sequence:7,bytes:20,decode_ms:4,wire_received_ms:400}]);
  assert.equal(report.video_wire_bytes,20);assert.equal(report.unique_source_frames,1);
  assert.equal(report.video_marker_eligible,true);assert.equal(h.original.length,3);
});

test('motion observer: the absolute cutoff and finalization reject late callbacks without extending counts', async t => {
  const h = await harness(t);
  h.now(h.interval.end_ms);h.wire(JSON.stringify({type:'pong',id:0}));h.client.onEvent({type:'pong',id:0});
  h.client.presentationState = {presentedAtMs:h.interval.end_ms};h.client.onEvent({type:'frame',frame:{sequence:1,bytes:20},decodeMs:1});
  const report = h.finish();
  assert.equal(report.pings[0].wire_received_ms,null);assert.equal(report.pings[0].dispatched_ms,null);assert.equal(report.paints.length,0);
  h.now(6200);h.wire(JSON.stringify({type:'pong',id:0}));h.client.onEvent({type:'pong',id:0});
  assert.equal(report.pings[0].dispatched_ms,null);assert.equal(report.arrivals.length,0);
});
