import assert from 'node:assert/strict';
import test from 'node:test';
import {TilePainter} from '../web/tile-painter.js';
import {tileRect} from '../web/tile-decoder.js';

function canvas(){const value={width:0,height:0,calls:[],getContext(){return this.context;}};value.context={putImageData(image,x,y){value.calls.push({image,x,y});}};return value;}
function patch(tile,{width=1280,height=800,epoch=1,sequence=1,arrival=0}={}){const r=tileRect(width,height,tile);return{...r,tile,frameWidth:width,frameHeight:height,epoch,sequence,arrival,decoded:arrival,decodeMs:.1,pixels:new Uint8ClampedArray(r.width*r.height*4)};}
async function withImageData(run){const old=globalThis.ImageData;globalThis.ImageData=class{constructor(data,width,height){this.data=data;this.width=width;this.height=height;}};try{await run();}finally{if(old===undefined)delete globalThis.ImageData;else globalThis.ImageData=old;}}
test('queue acceptance is not paint, and1000 queued tiles commit in one bounded visible update',()=>withImageData(()=>{
  const screen=canvas(),callbacks=[],events=[],painter=new TilePainter(screen,{now:()=>0,requestFrame:fn=>{callbacks.push(fn);return callbacks.length;},cancelFrame:()=>{},onPaint:event=>events.push(event)});
  for(let tile=0;tile<1000;tile+=64)painter.acceptBatch(Array.from({length:Math.min(64,1000-tile)},(_,i)=>patch(tile+i)));
  assert.equal(painter.queue.size,1000);assert.equal(screen.calls.length,0);assert.equal(painter.epoch,null);assert.equal(painter.snapshotRegion({x:0,y:0,width:1280,height:800}).ready,false);
  callbacks.shift()();
  assert.equal(callbacks.length,0);assert.equal(screen.calls.length,1);assert.equal(events.length,1);assert.equal(painter.readyCount,1000);assert.equal(painter.stats.evicted,0);assert.ok(painter.stats.queueBytesPeak<=4194304);assert.equal(painter.snapshotRegion({x:0,y:0,width:1280,height:800}).ready,true);assert.equal(painter.snapshot().backingBytes,1280*800*4);painter.close();assert.equal(painter.snapshot().backingBytes,0);
}));
test('painted epoch and sequence update only after putImageData commits',()=>withImageData(()=>{
  const screen=canvas(),callbacks=[],painter=new TilePainter(screen,{now:()=>0,requestFrame:fn=>{callbacks.push(fn);return 1;},cancelFrame:()=>{}});
  let expectedBefore=null;screen.context.putImageData=()=>assert.equal(painter.epoch===null?null:painter.painted[0]?.sequence??null,expectedBefore);
  painter.acceptBatch([patch(0,{width:2,height:1,epoch:0xffffffff,sequence:0xffffffff})]);assert.equal(painter.epoch,null);callbacks.shift()();assert.equal(painter.painted[0].sequence,0xffffffff);
  expectedBefore=0xffffffff;painter.acceptBatch([patch(0,{width:2,height:1,epoch:0xffffffff,sequence:0})]);assert.equal(painter.painted[0].sequence,0xffffffff);callbacks.shift()();assert.equal(painter.painted[0].sequence,0);
  painter.acceptBatch([patch(0,{width:1,height:1,epoch:0,sequence:1})]);assert.equal(painter.epoch,0xffffffff);expectedBefore=0;callbacks.shift()();assert.equal(painter.epoch,0);assert.equal(screen.width,1);assert.equal(painter.painted[0].sequence,1);painter.close();
}));
test('a malformed batch cannot clear committed pixels or insert any earlier patch',()=>withImageData(()=>{
  const screen=canvas(),painter=new TilePainter(screen,{now:()=>0,requestFrame:()=>1,cancelFrame:()=>{}}),first=patch(0),bad=patch(1);bad.pixels=new Uint8ClampedArray(1);
  assert.throws(()=>painter.acceptBatch([first,bad]));assert.equal(painter.queue.size,0);assert.equal(painter.pendingEpoch,null);assert.equal(screen.calls.length,0);painter.close();
}));
test('newer pending same-tile update supersedes an unpainted old update',()=>withImageData(()=>{
  const screen=canvas(),callbacks=[],painter=new TilePainter(screen,{now:()=>0,requestFrame:fn=>{callbacks.push(fn);return 1;},cancelFrame:()=>{}});
  painter.acceptBatch([patch(0,{sequence:2})]);painter.acceptBatch([patch(0,{sequence:3})]);assert.equal(painter.stats.replaced,1);assert.equal(painter.queue.size,1);assert.equal(screen.calls.length,0);callbacks.shift()();assert.equal(painter.painted[0].sequence,3);assert.equal(screen.calls.length,1);painter.close();
}));
test('a failed visible write never commits a newer tile and closes retained storage',()=>withImageData(()=>{
  let now=0;const screen=canvas(),callbacks=[],painter=new TilePainter(screen,{now:()=>now,requestFrame:fn=>{callbacks.push(fn);return callbacks.length;},cancelFrame:()=>{}});
  painter.acceptBatch([patch(0,{sequence:1})]);callbacks.shift()();assert.equal(painter.painted[0].sequence,1);
  screen.context.putImageData=()=>{throw new Error('visible write failed');};painter.acceptBatch([patch(0,{sequence:2})]);
  assert.throws(()=>callbacks.shift()(),/visible write failed/);assert.equal(painter.painted[0].sequence,1);assert.equal(painter.closed,true);assert.equal(painter.queue.size,0);assert.equal(painter.snapshot().backingBytes,0);painter.close();
}));
