// Pure transport race checks. No browser, native worker, socket, VM or GPU.
import test from 'node:test';
import assert from 'node:assert/strict';
import {ViewerMedia,codecFromHello,independentKeyPacket,MAX_PENDING_MEDIA} from '../viewer-media.js';
import {deflateSync} from 'node:zlib';
import {Decoder as LosslessDecoder} from '../../../../web/decoder.js';
import {HybridContinuityDecoder} from '../../../../web/hybrid-continuity-decoder.js';

const hello={type:'hello',codec:'hybrid-continuity',protocol:'GLH4',source:'rfb',fps_limit:30,
  hybrid_config:{base_mode:'global-cached',video_profile:'camera-scene-off',video_usage:'camera-video-real-time',
    scene_change_detection:false,mask_grid:32,base_tile:64}};
const tick=()=>new Promise(resolve=>setImmediate(resolve));
const held=()=>{let resolve;const promise=new Promise(done=>resolve=done);return{promise,resolve};};
const frame={pixels:new Uint8ClampedArray([1,2,3,255]),width:1,height:1,bytes:24,sequence:7};
const packet=()=>new ArrayBuffer(24);
function harness(options={}){
  const painted=[],sent=[],errors=[],metrics={frames:0,bytes:0,errors:0,sequences:[],resets:0,resyncs:0,
    hybrid_frames:0,hybrid_video_frames:0,hybrid_video_delta_frames:0,max_pending_media:0,recoveries:0,recovery_key_sequences:[]};
  const media=new ViewerMedia({metrics,open:()=>true,send:value=>sent.push(value),paint:value=>painted.push(value),
    boundary:async()=>{},loader:async()=>({decode:async()=>frame,close(){}}),onError:error=>errors.push(error),...options});
  media.capabilities(['display','input']);media.hello(options.hello??hello);return{media,painted,sent,metrics,errors};
}

test('only actual authorized trusted native presets select decoders',()=>{
  assert.equal(codecFromHello(hello),'hybrid-continuity');
  assert.equal(codecFromHello({type:'hello',codec:'deflate',protocol:'GLS1',source:'rfb',fps_limit:30}),'deflate');
  for(const changed of [{...hello,protocol:'GLH2'},{...hello,source:'demo'},{...hello,fps_limit:60},
    {...hello,hybrid_config:{...hello.hybrid_config,mask_grid:16}},
    {...hello,hybrid_config:{...hello.hybrid_config,scene_change_detection:0}},
    {...hello,hybrid_config:{...hello.hybrid_config,target:'secret'}}])assert.throws(()=>codecFromHello(changed));
  const {media}=harness();assert.throws(()=>media.hello(hello));
  const fresh=new ViewerMedia({metrics:{}});assert.throws(()=>fresh.hello(hello));
  assert.throws(()=>fresh.capabilities(['display','input','clipboard']));
});

test('live paint precedes one ACK and remains bounded',async()=>{
  const h=harness();h.media.enqueue(packet());await tick();
  assert.equal(h.painted.length,1);assert.deepEqual(h.sent,[{type:'ack',sequence:7}]);
  assert.equal(h.metrics.frames,1);assert.equal(h.metrics.max_pending_media,1);h.media.close();
});

test('close during delayed decoder loading disposes late decoder and never paints/ACKs',async()=>{
  const load=held();let closed=0;
  const h=harness({loader:()=>load.promise});h.media.enqueue(packet());await tick();h.media.close();
  load.resolve({decode:async()=>frame,close(){closed++;}});await tick();
  assert.equal(closed,1);assert.equal(h.painted.length,0);assert.deepEqual(h.sent,[]);assert.equal(h.metrics.errors,0);
});

test('close during decode or queued paint suppresses every stale completion',async()=>{
  for(const stage of ['decode','paint']){
    const gate=held();let closed=0;
    const h=harness({loader:async()=>({decode:()=>stage==='decode'?gate.promise:Promise.resolve(frame),close(){closed++;}}),
      boundary:()=>stage==='paint'?gate.promise:Promise.resolve()});
    h.media.enqueue(packet());await tick();h.media.close();gate.resolve(frame);await tick();
    assert.equal(closed,1);assert.equal(h.metrics.frames,0);assert.equal(h.painted.length,0);assert.deepEqual(h.sent,[]);
  }
});

test('reset invalidates old decode and admits only new epoch paint',async()=>{
  const gate=held();let instances=0,closed=0;
  const h=harness({loader:async()=>({decode:()=>++instances===1?gate.promise:Promise.resolve({...frame,sequence:9}),close(){closed++;}})});
  h.media.enqueue(packet());await tick();h.media.reset();h.media.enqueue(packet());gate.resolve(frame);await tick();await tick();
  assert.equal(closed,1);assert.equal(h.painted.length,1);assert.equal(h.painted[0].sequence,9);
  assert.deepEqual(h.sent,[{type:'ack',sequence:9}]);h.media.close();
});

test('reference failure requests one resync and holds deltas until independent key recovery',async()=>{
  const h=harness({loader:async()=>({decode:async()=>{throw new Error('missing reference');},close(){}})});
  h.media.enqueue(packet());await tick();h.media.enqueue(packet());await tick();
  assert.equal(h.metrics.errors,1);assert.deepEqual(h.sent,[{type:'resync'}]);assert.equal(h.painted.length,0);
  assert.equal(h.media.recovering,true);h.media.close();
});

test('queue overflow is rejected while decode is pending',async()=>{
  const gate=held();const h=harness({loader:async()=>({decode:()=>gate.promise,close(){}})});
  h.media.enqueue(packet());await tick();h.media.enqueue(packet());
  assert.equal(h.metrics.max_pending_media,MAX_PENDING_MEDIA);assert.throws(()=>h.media.enqueue(packet()),/queue/);
  h.media.close();gate.resolve(frame);await tick();assert.equal(h.painted.length,0);
});

test('decoder load timeout disposes a late candidate without paint',async()=>{
  const gate=held();let closed=0;
  const h=harness({loadTimeoutMs:5,loader:()=>gate.promise});h.media.enqueue(packet());
  await new Promise(resolve=>setTimeout(resolve,15));assert.equal(h.metrics.errors,1);
  gate.resolve({decode:async()=>frame,close(){closed++;}});await tick();
  assert.equal(closed,1);assert.equal(h.painted.length,0);h.media.close();
});


function protocolPacket(codec,{sequence=9,key=true}={}){
  const body=Buffer.alloc(20);body.writeUInt16LE(1,4);body.writeUInt16LE(1,6);body.writeUInt32LE(4,12);
  body.set([1,2,3,255],16);
  const header=Buffer.alloc(24);header.write(codec==='deflate'?'GLS1':'GLB2');header.writeUInt32LE(sequence,4);
  header.writeUInt16LE(1,12);header.writeUInt16LE(1,14);header.writeUInt16LE(1,16);header.writeUInt16LE(Number(key),18);
  let bytes;
  if(codec==='deflate')bytes=Buffer.concat([header,body]);
  else{
    header.writeUInt32LE(20,20);const base=Buffer.concat([header,deflateSync(body)]),wrapper=Buffer.alloc(64);
    wrapper.write('GLH4');wrapper.writeUInt32LE(sequence,4);wrapper.writeUInt16LE(1,12);wrapper.writeUInt16LE(1,14);
    wrapper.writeUInt16LE(32,16);wrapper.writeUInt16LE(Number(key),18);wrapper.writeUInt32LE(base.length,32);wrapper.writeUInt32LE(sequence,40);
    bytes=Buffer.concat([wrapper,base]);
  }
  return bytes.buffer.slice(bytes.byteOffset,bytes.byteOffset+bytes.byteLength);
}
async function idle(media){for(let attempts=0;media.busy&&attempts<100;attempts++)await tick();assert.equal(media.busy,false);}

test('actual GLS1 and GLH4 independent key packets recover without a reset control message',async()=>{
  for(const codec of ['deflate','hybrid-continuity']){
    const h=harness({hello:codec==='deflate'?{type:'hello',codec:'deflate',protocol:'GLS1',source:'rfb',fps_limit:30}:hello,
      loader:async()=>codec==='deflate'?new LosslessDecoder():new HybridContinuityDecoder()});
    h.media.requestRecovery();h.media.enqueue(protocolPacket(codec,{sequence:8,key:false}));
    await tick();assert.equal(h.media.recovering,true);assert.equal(h.metrics.frames,0);
    const key=protocolPacket(codec);assert.equal(independentKeyPacket(key,codec),true);
    h.media.enqueue(key);await idle(h.media);
    assert.equal(h.metrics.errors,0);assert.equal(h.metrics.frames,1);assert.equal(h.metrics.recoveries,1);
    assert.deepEqual(h.metrics.recovery_key_sequences,[9]);assert.deepEqual(Array.from(h.painted[0].pixels),[1,2,3,255]);
    assert.deepEqual(h.sent,[{type:'resync'},{type:'ack',sequence:9}]);h.media.close();
  }
});

test('resync invalidates an old pending decode and paints only the independent key in the new epoch',async()=>{
  const gate=held();let created=0,closed=0;
  const h=harness({loader:async()=>{const index=++created;return{decode:()=>index===1?gate.promise:Promise.resolve({...frame,keyframe:true,sequence:9}),close(){closed++;}};}});
  h.media.enqueue(packet());await tick();h.media.requestRecovery();
  h.media.enqueue(protocolPacket('hybrid-continuity',{sequence:8,key:false}));h.media.enqueue(protocolPacket('hybrid-continuity'));
  gate.resolve(frame);await idle(h.media);
  assert.equal(closed,1);assert.equal(h.metrics.frames,1);assert.equal(h.metrics.recoveries,1);
  assert.deepEqual(h.sent,[{type:'resync'},{type:'ack',sequence:9}]);h.media.close();
});

test('a key header never bypasses the existing full packet decoder or grants paint/ACK',async()=>{
  const h=harness({loader:async()=>new HybridContinuityDecoder()});h.media.requestRecovery();
  const malformed=protocolPacket('hybrid-continuity');new Uint8Array(malformed)[64]=0;
  h.media.enqueue(malformed);await idle(h.media);
  assert.equal(h.metrics.frames,0);assert.equal(h.metrics.recoveries,0);assert.equal(h.metrics.errors,1);
  assert.deepEqual(h.sent,[{type:'resync'},{type:'resync'}]);assert.equal(h.media.recovering,true);h.media.close();
});
