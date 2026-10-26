import assert from 'node:assert/strict';
import test from 'node:test';
import {configureTileDatagrams,validateTileHello} from '../web/tile-transport.js';
import {openDatagramSocket} from '../web/tile-connect.js';

const descriptor={codec:'glt2-lossless',datagram_codec:'glt2-lossless',protocol:'GLT2-WT1-negotiated-stride',datagram_protocol:'GLT2',policy:0,congestion_control:'bbr',payload_stride_range:[1024,1168],max_datagram_bytes:1200};
const hello={...descriptor,type:'hello',transport:'webtransport-datagram',independent_tiles:true,media_retransmission:false,tile_side:32,payload_stride:1024,max_datagram_bytes:1056,initial_epoch:1,width:1280,height:800};
test('queue feature proof requires existing prototype getters/setters, before assignment',()=>{
  const fake={incomingMaxAge:250,incomingMaxBufferedDatagrams:4096};assert.throws(()=>configureTileDatagrams(fake));
  class Supported{get incomingMaxAge(){return this.age??null;}set incomingMaxAge(value){this.age=value;}get incomingMaxBufferedDatagrams(){return this.count??0;}set incomingMaxBufferedDatagrams(value){this.count=value;}}
  const supported=new Supported(),proof=configureTileDatagrams(supported);assert.equal(proof.queue_count_property,'incomingMaxBufferedDatagrams');assert.equal(proof.incoming_queue_count,4096);assert.equal(proof.incoming_max_age_ms,250);
  class Legacy{get incomingMaxAge(){return this.age;}set incomingMaxAge(value){this.age=value;}get incomingHighWaterMark(){return this.count;}set incomingHighWaterMark(value){this.count=value;}}
  assert.equal(configureTileDatagrams(new Legacy()).queue_count_property,'incomingHighWaterMark');
  class Clamped extends Supported{set incomingMaxAge(value){this.age=value/2;}get incomingMaxAge(){return this.age;}}
  assert.throws(()=>configureTileDatagrams(new Clamped()));
});
test('only authenticated matching codec, policy, exact negotiated stride and controller configure media',()=>{
  assert.deepEqual(validateTileHello(descriptor,hello),{payloadStride:1024,policy:0});
  for(const change of[{codec:'h264'},{datagram_codec:'glt2-gentle'},{policy:1},{payload_stride:1023},{max_datagram_bytes:1200},{protocol:'GLT2'},{congestion_control:'cubic'},{independent_tiles:false},{media_retransmission:true},{width:3841}])assert.throws(()=>validateTileHello(descriptor,{...hello,...change}));
  assert.throws(()=>validateTileHello({...descriptor,payload_stride_range:[0,1168]},hello));assert.throws(()=>validateTileHello({...descriptor,codec:'glt2-strong'},hello));
});
test('one descriptor fetch, cancellation after fetch or module load acquires no tile transport',async()=>{
  const original=globalThis.fetch;let calls=0,loads=0,constructs=0,current=false;
  globalThis.fetch=async()=>{calls++;return{ok:true,json:async()=>descriptor};};
  const loadTiles=async()=>{loads++;current=false;return{TileDatagramSocket:class{constructor(){constructs++;}}};};
  try{assert.equal(await openDatagramSocket({isCurrent:()=>current,loadTiles}),null);assert.equal(loads,0);current=true;assert.equal(await openDatagramSocket({isCurrent:()=>current,loadTiles}),null);assert.equal(calls,2);assert.equal(loads,1);assert.equal(constructs,0);}
  finally{globalThis.fetch=original;}
});
test('unknown authenticated codec fails before importing tiles or opening a transport',async()=>{
  const original=globalThis.fetch;let loaded=false;globalThis.fetch=async()=>({ok:true,json:async()=>({...descriptor,codec:'unknown',datagram_codec:'unknown'})});
  try{await assert.rejects(openDatagramSocket({loadTiles:async()=>{loaded=true;}}));assert.equal(loaded,false);}finally{globalThis.fetch=original;}
});
