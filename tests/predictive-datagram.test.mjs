import test from 'node:test';
import assert from 'node:assert/strict';
import {PredictiveDatagramSocket,requirePredictiveCapability} from '../web/predictive-datagram.js';
import {PredictiveReceiver} from '../web/predictive-state.js';
const capability={media:'GLD1/GLV1-predictive-v1',datagram_codec:'h264-predictive-gop4',protocol:'GLV1',datagram_protocol:'GLD1',
  independent_frames:false,media_retransmission:false,reference_policy:'contiguous-since-idr',gop_frames:4,
  maximum_access_unit_bytes:1048576,encoded_head_interval_ms:1000,encoded_head_deadline_ms:250,recovery_interval_ms:1000};
// Syntactic GLV1 parser/state fixtures only; the separate real codec proof is
// required before these integration contracts can support live admission.
function packet(sequence,keyframe){
  const nal=keyframe?[0,0,1,103,66,0,30,128,0,0,1,104,128,0,0,1,101,128]:[0,0,1,65,128];
  const raw=new Uint8Array(24+nal.length),view=new DataView(raw.buffer);raw.set([71,76,86,49]);
  view.setUint32(4,sequence,true);view.setUint16(12,32,true);view.setUint16(14,32,true);
  view.setUint16(18,Number(keyframe),true);view.setUint32(20,nal.length,true);raw.set(nal,24);return raw.buffer;
}
function bare(){
  const socket=Object.create(PredictiveDatagramSocket.prototype);
  Object.assign(socket,{closedOnce:false,hello:true,receiver:new PredictiveReceiver(),selected:null,delivering:false,
    transportStats:{presented:0,decodedOutputSuppressed:0},events:[]});
  socket._emit=(_kind,event)=>socket.events.push(event);socket._requestRecovery=()=>{};return socket;
}
test('all-IDR and partially matching capabilities cannot select predictive decoding',()=>{
  requirePredictiveCapability(capability);
  for(const [key,bad] of [['media','GLD1/GLV1-all-IDR'],['independent_frames',true],['gop_frames',1],['encoded_head_interval_ms',999],
    ['maximum_access_unit_bytes',16777216],['reference_policy','latest-frame']]){
    assert.throws(()=>requirePredictiveCapability({...capability,[key]:bad}));
  }
});
test('XOR selection requires the exact compact50 profile and old profiles omit every FEC field',()=>{
  const compact={...capability,datagram_codec:'h264-predictive-gop4-compact50',encoded_head_interval_ms:50,
    encoded_head_notification:'on-change-coalesced-v1',statistics_interval_ms:1000,encoded_head_max_body_bytes:128};
  const xor={...compact,datagram_codec:'h264-predictive-gop4-compact50-xor8',fec:'xor-grouped-v1',
    fec_scope:'all-access-units',fec_data_fragments_per_group:8,fec_parity_fragments_per_group:1,parity_datagram_protocol:'GLP1'};
  assert.equal(requirePredictiveCapability(compact),50);assert.equal(requirePredictiveCapability(xor),50);
  for(const key of ['fec','fec_scope','fec_data_fragments_per_group','fec_parity_fragments_per_group','parity_datagram_protocol']){
    assert.throws(()=>requirePredictiveCapability({...compact,[key]:xor[key]}));
    assert.throws(()=>requirePredictiveCapability({...xor,[key]:key.includes('fragments')?2:'wrong'}));
  }
  assert.throws(()=>requirePredictiveCapability({...xor,datagram_codec:'h264-predictive-gop4-compact50'}));
  assert.throws(()=>requirePredictiveCapability({...compact,datagram_codec:'h264-predictive-gop4-compact50-xor8'}));
});
function controlReadable(message){
  const body=new TextEncoder().encode(JSON.stringify(message)),framed=new Uint8Array(body.length+4);new DataView(framed.buffer).setUint32(0,body.length,true);framed.set(body,4);
  let delivered=false;return{getReader:()=>({async read(){if(delivered)return{done:true};delivered=true;return{done:false,value:framed};},releaseLock(){}})};
}
test('reliable hello must repeat the descriptor-selected XOR profile exactly',async()=>{
  const compact={...capability,type:'hello',transport:'webtransport-datagram',codec:'h264',datagram_codec:'h264-predictive-gop4-compact50',
    encoded_head_interval_ms:50,encoded_head_notification:'on-change-coalesced-v1',statistics_interval_ms:1000,encoded_head_max_body_bytes:128};
  const socket=Object.create(PredictiveDatagramSocket.prototype);
  Object.assign(socket,{closedOnce:false,hello:false,profile:'h264-predictive-gop4-compact50-xor8',headInterval:50,compactHead:true,
    receiver:new PredictiveReceiver(),transportStats:{compactHeadMessagesReceived:0},_emit:()=>{},_pump:()=>{}});
  await assert.rejects(()=>socket._readControl(controlReadable(compact)),/Predictive head profile changed/);
  const xor={...compact,datagram_codec:'h264-predictive-gop4-compact50-xor8',fec:'xor-grouped-v1',fec_scope:'all-access-units',
    fec_data_fragments_per_group:8,fec_parity_fragments_per_group:1,parity_datagram_protocol:'wrong'};
  await assert.rejects(()=>socket._readControl(controlReadable(xor)),/Unsupported predictive FEC mode/);
});
test('actual selected token gates reset, serial decode and pre-Canvas eligibility',()=>{
  const socket=bare(),now=performance.now();
  socket.receiver.gate.offer(packet(1,true),now);socket.receiver.gate.offer(packet(2,false),now);socket._pump();
  assert.equal(socket.events.length,1);assert.equal(socket.selected.sequence,1);
  let resets=0;socket.beginVideoDecode(socket.events[0].data,{reset(){resets++;}});assert.equal(resets,1);
  socket.send(JSON.stringify({type:'ack',sequence:1}));assert.equal(socket.events.length,1);
  assert.equal(socket.completeVideoDecode(1,true),true);assert.equal(socket.events.length,2);assert.equal(socket.selected.sequence,2);
  socket.receiver.gate.loss(3,performance.now());
  assert.equal(socket.completeVideoDecode(2,true),false);assert.equal(socket.transportStats.decodedOutputSuppressed,1);
  assert.equal(socket.receiver.snapshot().reference.lastDecodedSequence,1);
  socket.receiver.gate.offer(packet(4,true),performance.now());socket._pump();
  socket.beginVideoDecode(socket.events.at(-1).data,{reset(){resets++;}});
  assert.equal(resets,2);assert.equal(socket.completeVideoDecode(4,true),true);
});
test('startup waits for real media/loss or head evidence instead of immediately requesting reset',()=>{
  const socket=bare();let calls=0;
  socket.receiver.recovery=()=>{calls++;return null;};
  PredictiveDatagramSocket.prototype._requestRecovery.call(socket);assert.equal(calls,0);
  socket.receiver.gate.observeHead(1,performance.now());
  // An unexpired initial head alone is still not a missing-frame observation.
  PredictiveDatagramSocket.prototype._requestRecovery.call(socket);assert.equal(calls,0);
  socket.receiver.gate.stats.headTimeouts=1;
  PredictiveDatagramSocket.prototype._requestRecovery.call(socket);assert.equal(calls,1);
});
test('base receiver close clears the active token and rejects late epoch history',()=>{
  const receiver=new PredictiveReceiver(),now=performance.now();receiver.gate.offer(packet(9,true),now);
  assert.equal(receiver.next(now).sequence,9);receiver.close();
  assert.equal(receiver.snapshot().reference.activeSequence,null);assert.equal(receiver.snapshot().reference.pendingBytes,0);
  assert.throws(()=>receiver.receive(new Uint8Array(20),now+1));
});
