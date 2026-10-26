import assert from 'node:assert/strict';
import test from 'node:test';
import {readFile} from 'node:fs/promises';
import {createHash} from 'node:crypto';
import {deflateSync,inflateSync,constants} from 'node:zlib';
import {boundedFflate,adler32} from '../web/tile-inflate.js';
import {TileReceiver,browserInflate} from '../web/tile-decoder.js';

const digest=bytes=>createHash('sha256').update(bytes).digest('hex');
const rawFile=name=>readFile(new URL('../'+name,import.meta.url));
const textFile=async name=>(await rawFile(name)).toString('utf8');
const CORE_SHA='74be8eb9e285e0684462aceba37198275f5f4fe0d65b4b4407334c589e74a1e1';
const WRAPPER_SHA='5cbe90657ac2c6f9e9584975d8452217eeba6b4f20a0d75a34fafa0403cda686';
const SAFETY_SHA='626cf6b7f7a17cc7bd1b83820501097007fd76a49cbe87e8fe16b9477fd057f9';

function packets(body,{width=32,height=32,epoch=1,sequence=1,kind=1,stride=1168}={}){
  const count=Math.ceil(body.length/stride);
  return Array.from({length:count},(_,index)=>{
    const part=body.subarray(index*stride,(index+1)*stride),out=new Uint8Array(32+part.length),view=new DataView(out.buffer);
    out.set([71,76,84,50]);view.setUint32(4,epoch,true);view.setUint32(8,sequence,true);view.setUint16(12,width,true);view.setUint16(14,height,true);out[18]=32;out[19]=kind;
    view.setUint16(20,body.length,true);out[22]=index;out[23]=count;view.setUint16(24,index*stride,true);view.setUint16(26,part.length,true);view.setUint16(28,width*height*4,true);out.set(part,32);return out;
  });
}
function receive(receiver,body,options){for(const packet of packets(body,options))receiver.receive(packet);}

test('production core and exact wrapper are the sealed sixteen-test implementation',async()=>{
  const core=await rawFile('web/vendor/fflate-bounded-core.js'),researchCore=await rawFile('third_party/fflate/fflate-bounded-core.mjs');
  assert.equal(digest(core),CORE_SHA);assert.deepEqual(core,researchCore);
  const wrapper=await textFile('third_party/fflate/inflate.mjs');assert.equal(digest(wrapper),WRAPPER_SHA);
  const piece=(start,end)=>wrapper.slice(wrapper.indexOf(start),wrapper.indexOf(end));
  const expected="// CPU GLT2 zlib inflate. Guarded fflate0.8.3 fork; see vendor/fflate-provenance.json.\nimport {glissRawInflate} from './vendor/fflate-bounded-core.js';\n"+
    piece('const require=','export function combineAdler')+piece('const footer=','export async function boundedStream');
  assert.equal(await textFile('web/tile-inflate.js'),expected);
  const suite=await rawFile('third_party/fflate/inflate.test.mjs');assert.equal(digest(suite),SAFETY_SHA);assert.equal((suite.toString().match(/^test\(/gm)??[]).length,16);
  const provenance=JSON.parse(await textFile('web/vendor/fflate-provenance.json'));
  assert.equal(provenance.pinned_sources.guarded_core.sha256,CORE_SHA);assert.equal(provenance.pinned_sources.wrapper_source.sha256,WRAPPER_SHA);assert.equal(provenance.pinned_sources.safety_suite.sha256,SAFETY_SHA);
  for(const[name,record]of Object.entries(provenance.production)){const bytes=await rawFile(name);assert.equal(bytes.length,record.bytes);assert.equal(digest(bytes),record.sha256);}
  assert.deepEqual(await rawFile('web/vendor/fflate-LICENSE'),await rawFile('third_party/fflate/vendor/LICENSE'));
  const patch=await rawFile(provenance.pinned_sources.patch_recipe.path);assert.equal(digest(patch),provenance.pinned_sources.patch_recipe.sha256);
});

test('production inflater decodes native strategy outputs with exact fixed-size bounds',()=>{
  for(const strategy of[constants.Z_DEFAULT_STRATEGY,constants.Z_FIXED,constants.Z_HUFFMAN_ONLY,constants.Z_RLE]){
    for(const length of[4,257,1024,4096]){
      const expected=Uint8Array.from({length},(_,index)=>index%7===0?65:66),body=new Uint8Array(deflateSync(expected,{level:1,strategy}));
      assert.ok(body.length<=4096);assert.deepEqual(new Uint8Array(inflateSync(body)),expected);assert.deepEqual(boundedFflate(body,length),expected);
      assert.throws(()=>boundedFflate(body,length-1));assert.throws(()=>boundedFflate(body,length+1));
    }
  }
  const zeroAdler=new Uint8Array(257).fill(255);zeroAdler[256]=240;assert.equal(adler32(zeroAdler)&65535,0);assert.deepEqual(boundedFflate(new Uint8Array(deflateSync(zeroAdler)),257),zeroAdler);
});

test('production wrapper rejects reserved symbols, malformed checksum, extent and overflow',()=>{
  for(const[body,limit]of[[new Uint8Array([120,1,3,126,0,0,3,0,1]),3],[new Uint8Array([120,1,3,126,12,0,0,4,0,1]),4]]){
    assert.throws(()=>inflateSync(body));assert.throws(()=>boundedFflate(body,limit));
  }
  const body=new Uint8Array(deflateSync(new Uint8Array(4096).fill(71))),bad=body.slice(),trailing=new Uint8Array(body.length+1);bad[bad.length-1]^=1;
  trailing.set(body.subarray(0,-4));trailing.set(body.subarray(-4),body.length-3);
  for(const malformed of[bad,body.subarray(0,-1),trailing,new Uint8Array(deflateSync(new Uint8Array(4097)))])assert.throws(()=>boundedFflate(malformed,4096));
  assert.throws(()=>boundedFflate(body,0));assert.throws(()=>boundedFflate(body,4097));assert.throws(()=>boundedFflate(new Uint8Array(4097),4096));
});

test('default actual receiver decodes compressed alpha tiles at both negotiated strides',async()=>{
  const expected=new Uint8Array(4096);let state=0x7139a25d;
  for(let index=0;index<2048;index++){state=(Math.imul(state,1664525)+1013904223)>>>0;expected[index]=expected[index+2048]=state>>>24;}
  const body=new Uint8Array(deflateSync(expected,{level:1}));assert.ok(body.length>1168&&body.length<4096,'actual compressed input spans multiple fragments');
  for(const stride of[1024,1168]){
    const receiver=new TileReceiver({payloadStride:stride,now:()=>0});assert.equal(receiver.inflate,boundedFflate);
    for(const packet of packets(body,{stride}).reverse())receiver.receive(packet);
    const patch=await receiver.decodeNext();assert.ok(patch);assert.deepEqual(new Uint8Array(patch.pixels),expected);
    assert.equal(receiver.stats.inflateAttempted,1);assert.equal(receiver.stats.inflateCompleted,1);assert.equal(receiver.stats.inflateFailed,0);receiver.close();
  }
  assert.equal(typeof browserInflate,'function');const injected=()=>new Uint8Array(4096),receiver=new TileReceiver({inflate:injected});assert.equal(receiver.inflate,injected);receiver.close();
});

test('default inflater rejects bad future state and later valid epoch still commits',async()=>{
  const receiver=new TileReceiver({now:()=>0}),old=new Uint8Array([17,23,31,47]);receive(receiver,old,{width:1,height:1,kind:0,sequence:9});assert.deepEqual(new Uint8Array((await receiver.decodeNext()).pixels),old);
  receive(receiver,new Uint8Array([120,1,3,126,0,0,3,0,1]),{epoch:2,sequence:10});assert.equal(await receiver.decodeNext(),null);
  assert.equal(receiver.epoch,1);assert.equal(receiver.width,1);assert.equal(receiver.height,1);assert.deepEqual(receiver.sequences,[9]);assert.equal(receiver.geometry.size,0);assert.equal(receiver.stats.inflateFailed,1);
  const current=new Uint8Array(4096).fill(83);receive(receiver,new Uint8Array(deflateSync(current)),{epoch:2,sequence:10});const patch=await receiver.decodeNext();assert.deepEqual(new Uint8Array(patch.pixels),current);assert.equal(receiver.epoch,2);assert.equal(receiver.width,32);receiver.close();
});

test('tile inflater preserves wire transforms and unchanged transport/worker modules',async()=>{
  const prefix='test-support/browser/fixtures/pre-inflate/';
  // The original experiment's whole-frontend comparison remains in its archived
  // evidence. Subsequent predictive video changes client negotiation and H264
  // decoding, covered by predictive-client/datagram tests. Keep this living
  // regression focused on the unchanged worker and transport. The historical
  // painter stays frozen in pre-inflate; current presentation correctness is
  // covered by tile-coherent-paint and tile-painter tests after the October port.
  for(const name of['tile-worker.js','tile-transport.js','datagram-transport.js'])assert.deepEqual(await rawFile('web/'+name),await rawFile(prefix+name),name);
  const before=await textFile(prefix+'tile-decoder.js'),after=await textFile('web/tile-decoder.js');
  const expected=before.replace('// inversePredicted/browserInflate preserve the pinned experiment semantics.',"// GLT2 transform/wire semantics stay fixed; the default inflater is bounded CPU JS.\nimport {boundedFflate} from './tile-inflate.js';").replace('inflate=browserInflate}={})','inflate=boundedFflate}={})');
  assert.equal(after,expected,'only default inflater import/selection may change');
});
