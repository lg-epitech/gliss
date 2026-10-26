import assert from 'node:assert/strict';
import test from 'node:test';
import {deflateSync} from 'node:zlib';
import {TileReceiver,parseTile,MAX_PARTIALS,MAX_PARTIAL_BYTES,MAX_COMPLETE_BYTES,newer} from '../web/tile-decoder.js';
import {TileWorkerCore} from '../web/tile-worker.js';

export function fragments({width=32,height=32,tile=0,epoch=1,sequence=1,stride=1168,kind=0,policy=0,body=null}={}){
  const x=tile%Math.ceil(width/32)*32,y=Math.floor(tile/Math.ceil(width/32))*32,raw=Math.min(32,width-x)*Math.min(32,height-y)*4;
  body??=new Uint8Array(raw).fill(sequence&255);const count=Math.ceil(body.length/stride);
  return Array.from({length:count},(_,index)=>{const payload=body.subarray(index*stride,(index+1)*stride),bytes=new Uint8Array(payload.length+32),v=new DataView(bytes.buffer);
    bytes.set([71,76,84,50]);v.setUint32(4,epoch,true);v.setUint32(8,sequence,true);v.setUint16(12,width,true);v.setUint16(14,height,true);v.setUint16(16,tile,true);bytes[18]=32;bytes[19]=kind;
    v.setUint16(20,body.length,true);bytes[22]=index;bytes[23]=count;v.setUint16(24,index*stride,true);v.setUint16(26,payload.length,true);v.setUint16(28,raw,true);bytes[30]=policy;bytes.set(payload,32);return bytes;});
}
const receive=(decoder,packets)=>{for(const packet of packets)decoder.receive(packet);};
test('both immutable negotiated strides recover reordered raw alpha bytes',async()=>{
  for(const stride of[1024,1168]){const expected=Uint8Array.from({length:4096},(_,i)=>i&255),decoder=new TileReceiver({payloadStride:stride});const packets=fragments({stride,body:expected});
    receive(decoder,[packets[3],packets[0],packets[0],packets[2],packets[1]]);const patch=await decoder.decodeNext();assert.deepEqual(new Uint8Array(patch.pixels),expected);assert.equal(decoder.stats.duplicates,1);assert.equal(decoder.stats.decodedTiles,1);assert.equal(decoder.partialBytes,0);assert.equal(decoder.completeBytes,0);
  }
});
test('media cannot change stride, flags, policy, geometry or canonical fragment lengths',()=>{
  const packet=fragments({stride:1024})[1];assert.throws(()=>parseTile(packet,1168));
  for(const mutate of[bytes=>bytes[31]=1,bytes=>bytes[30]=2,bytes=>new DataView(bytes.buffer).setUint16(24,1,true),bytes=>bytes[23]=5,bytes=>new DataView(bytes.buffer).setUint16(12,3841,true)]){const bytes=fragments()[0].slice();mutate(bytes);assert.throws(()=>parseTile(bytes,1168,0));}
  assert.throws(()=>new TileReceiver({payloadStride:1023}));assert.throws(()=>new TileReceiver({policy:3}));
});
test('conflicting duplicate is atomic and valid remaining fragments still finish',async()=>{
  const decoder=new TileReceiver(),packets=fragments();decoder.receive(packets[0]);const conflicting=packets[0].slice();conflicting[32]^=255;assert.equal(decoder.receive(conflicting),false);receive(decoder,packets.slice(1));assert.ok(await decoder.decodeNext());assert.equal(decoder.stats.malformed,1);
});
test('malformed compressed future epoch cannot commit geometry or sequence',async()=>{
  const decoder=new TileReceiver();receive(decoder,fragments({width:1,height:1,sequence:9}));await decoder.decodeNext();
  receive(decoder,fragments({width:3840,height:2160,epoch:2,kind:1,body:new Uint8Array([1,2,3])}));assert.equal(await decoder.decodeNext(),null);
  assert.equal(decoder.epoch,1);assert.equal(decoder.width,1);assert.deepEqual(decoder.sequences,[9]);assert.equal(decoder.stats.inflateFailed,1);assert.equal(decoder.geometry.size,0);
});
test('inflate rejects truncated, trailing, short and oversized output without state change',async()=>{
  const normal=deflateSync(new Uint8Array(4096)),trailing=new Uint8Array(normal.length+1);trailing.set(normal);trailing[trailing.length-1]=12;
  for(const body of[normal.subarray(0,normal.length-1),trailing,deflateSync(new Uint8Array(4095)),deflateSync(new Uint8Array(4097))]){
    const decoder=new TileReceiver();receive(decoder,fragments({kind:1,body}));assert.equal(await decoder.decodeNext(),null);assert.equal(decoder.epoch,null);assert.equal(decoder.stats.inflateFailed,1);
  }
});
test('partial TTL and complete age are separate, bounded and release staged metadata',async()=>{
  let now=0;const decoder=new TileReceiver({now:()=>now});decoder.receive(fragments()[0]);now=133;decoder.expire();assert.equal(decoder.partial.size,1);now=134;decoder.expire();assert.equal(decoder.partial.size,0);
  receive(decoder,fragments({sequence:2}));now=383;assert.ok(await decoder.decodeNext());receive(decoder,fragments({sequence:3}));now=633;assert.equal(await decoder.decodeNext(),null);assert.equal(decoder.stats.completeExpired,1);assert.equal(decoder.geometry.size,0);
});
test('future partial flood stays below both caps without committing a framebuffer',()=>{
  const decoder=new TileReceiver({now:()=>0});for(let tile=0;tile<1000;tile++)decoder.receive(fragments({width:1280,height:800,tile,epoch:2})[0]);
  assert.ok(decoder.partial.size<=MAX_PARTIALS);assert.ok(decoder.partialBytes<=MAX_PARTIAL_BYTES);assert.ok(decoder.geometry.size<=MAX_PARTIALS);assert.equal(decoder.epoch,null);assert.equal(decoder.sequences.length,0);assert.ok(decoder.stats.partialEvicted>0);
});
test('late async old result cannot overtake newer queued same-tile state',async()=>{
  let finish;const pending=new Promise(resolve=>finish=resolve),decoder=new TileReceiver({now:()=>0,inflate:()=>pending});
  receive(decoder,fragments({kind:1,body:new Uint8Array([1]),sequence:10}));const old=decoder.decodeNext();receive(decoder,fragments({sequence:11}));finish(new Uint8Array(4096));assert.equal(await old,null);assert.equal(decoder.epoch,null);
  const current=await decoder.decodeNext();assert.equal(current.sequence,11);assert.equal(decoder.stats.lateRejected,1);
});
test('closing during an awaited inflate prevents any post-close commit',async()=>{
  let finish;const decoder=new TileReceiver({now:()=>0,inflate:()=>new Promise(resolve=>finish=resolve)});receive(decoder,fragments({kind:1,body:new Uint8Array([1])}));const active=decoder.decodeNext();decoder.close();finish(new Uint8Array(4096));assert.equal(await active,null);assert.equal(decoder.epoch,null);assert.equal(decoder.geometry.size,0);
});
test('epoch and sequence wrap progress and old geometry stays rejected',async()=>{
  const decoder=new TileReceiver({now:()=>0});for(const sequence of[0xfffffffe,0xffffffff,0,1]){receive(decoder,fragments({epoch:0xffffffff,sequence}));assert.equal((await decoder.decodeNext()).sequence,sequence);}
  receive(decoder,fragments({epoch:0,width:1,height:1}));assert.equal((await decoder.decodeNext()).frameWidth,1);receive(decoder,fragments({epoch:0xffffffff,sequence:2}));assert.equal(await decoder.decodeNext(),null);assert.equal(decoder.width,1);assert.ok(newer(0,0xffffffff));assert.ok(!newer(0x80000000,0));
});
test('1000-tile burst retains complete/decoded capacity beyond128 and fair transfers',async()=>{
  const batches=[],core=new TileWorkerCore({config:{payloadStride:1168,policy:0},now:()=>0,post:(value,transfer)=>batches.push(structuredClone(value,{transfer}))});
  for(let tile=0;tile<1000;tile++)for(const packet of fragments({width:1280,height:800,tile}))core.receive(packet);
  for(let waits=0;core.decoding&&waits<2000;waits++)await new Promise(resolve=>setTimeout(resolve,1));assert.equal(core.decoding,false);assert.equal(core.receiver.stats.decodedTiles,1000);
  assert.ok(core.receiver.stats.completePeak>128);assert.ok(core.stats.patchCountPeak>128);assert.ok(core.receiver.stats.completeBytesPeak<=MAX_COMPLETE_BYTES);assert.ok(core.stats.patchBytesPeak<=MAX_COMPLETE_BYTES);assert.equal(core.stats.patchesEvicted,0);
  let acknowledged=0;while(core.inflight){assert.ok(batches.length>acknowledged);const batch=batches[acknowledged++];assert.ok(batch.patches.length<=64);assert.ok(batch.patches.reduce((sum,p)=>sum+p.pixels.byteLength,0)<=262144);core.accepted(batch.batchId);}
  assert.equal(new Set(batches.flatMap(batch=>batch.patches.map(p=>p.tile))).size,1000);assert.equal(core.bytes,0);assert.equal(core.stats.acceptedTransfers,batches.length);await core.close();
});
