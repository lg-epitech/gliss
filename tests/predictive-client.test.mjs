// Execute the actual GlissClient class body with controlled DOM/transport IO.
// These are client-path/adapter error tests, not a real codec/browser proof.
import test from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {VideoDecoderAdapter} from '../web/video-decoder.js';
import {AudioPlayer,isAudioPacket} from '../web/audio-player.js';
const source=await readFile(new URL('../web/client.js',import.meta.url),'utf8');
const begin='const keysyms =';
assert.equal(source.split(begin).length,2);
const actualClass=source.slice(source.indexOf(begin)).replace('export class GlissClient','class GlissClient');
function actualClient(predictive){
  const events=[],sent=[];let width=32,height=32,registered,callback;
  const ctx={drawImage(){events.push(['draw',width,height]);}};
  const canvas={get width(){return width;},set width(value){width=value;events.push(['width',value]);},
    get height(){return height;},set height(value){height=value;events.push(['height',value]);},getContext(){return ctx;},addEventListener(){}};
  const raf=work=>{callback=work;registered();return 1;};
  class Decoder{close(){}reset(){}}
  class Socket{static OPEN=1;constructor(){this.readyState=1;this.predictiveVideo=predictive;}send(text){sent.push(JSON.parse(text));}close(){this.readyState=3;}}
  const Client=new Function('Decoder','VideoDecoderAdapter','decodeProbe','AudioPlayer','isAudioPacket','window','document','location','WebSocket','requestAnimationFrame','performance',
    actualClass+'\nreturn GlissClient;')(Decoder,VideoDecoderAdapter,()=>0,AudioPlayer,isAudioPacket,{addEventListener(){}},{addEventListener(){}},
      {pathname:'/',protocol:'http:',host:'127.0.0.1'},Socket,raf,performance);
  const app=new Client(canvas);
  return{app,events,sent,canvas,
    async hello(){await app.connect();app.socket.onmessage({data:JSON.stringify({type:'hello',codec:'h264'})});await app.chain;},
    async frame(eligible,sequence){
      let closed=0;const frame={width:64,height:48,sequence,inputId:0,keyframe:true,bytes:24,videoFrame:{close(){closed++;}}};
      app.decoder.decode=async()=>frame;
      if(predictive){app.socket.beginVideoDecode=()=>{};app.socket.completeVideoDecode=actual=>{assert.equal(actual,sequence);events.push(['eligible',eligible]);return eligible;};}
      const ready=new Promise(resolve=>registered=resolve);app.socket.onmessage({data:new ArrayBuffer(24)});await ready;
      const before=events.slice();callback(performance.now());await app.chain;
      return{before,closed};
    }};
}
test('actual client defers predictive resize until approval; rejected output leaves Canvas untouched',async()=>{
  const fixture=actualClient(true);await fixture.hello();assert.equal(fixture.app.decoder.timeoutMs,250);assert.equal(fixture.app.decoder.configureTimeoutMs,250);
  const rejected=await fixture.frame(false,1);
  assert.deepEqual(rejected.before,[]);assert.deepEqual(fixture.events,[['eligible',false]]);assert.equal(rejected.closed,1);
  assert.equal(fixture.canvas.width,32);assert.equal(fixture.canvas.height,32);assert.equal(fixture.app.metrics.frames,0);
  fixture.events.length=0;const accepted=await fixture.frame(true,2);
  assert.deepEqual(accepted.before,[]);assert.deepEqual(fixture.events,[['eligible',true],['width',64],['height',48],['draw',64,48]]);
  assert.equal(accepted.closed,1);assert.equal(fixture.app.metrics.frames,1);assert.equal(fixture.app.metrics.errors,0);
  const legacy=actualClient(false);await legacy.hello();assert.equal(legacy.app.decoder.timeoutMs,5000);assert.equal(legacy.app.decoder.configureTimeoutMs,null);
  const old=await legacy.frame(true,1);assert.deepEqual(old.before,[['width',64],['height',48]]);
  assert.deepEqual(legacy.events,[['width',64],['height',48],['draw',64,48]]);
});
function idr(){
  const nal=[0,0,1,103,66,0,30,128,0,0,1,104,128,0,0,1,101,128],raw=new Uint8Array(24+nal.length),view=new DataView(raw.buffer);
  raw.set([71,76,86,49]);view.setUint32(4,1,true);view.setUint16(12,32,true);view.setUint16(14,32,true);
  view.setUint16(18,1,true);view.setUint32(20,nal.length,true);raw.set(nal,24);return raw.buffer;
}
async function controlledBackend(work){
  const previous={VideoDecoder:globalThis.VideoDecoder,EncodedVideoChunk:globalThis.EncodedVideoChunk};
  class Backend{
    static async isConfigSupported(config){return{supported:true,config};}
    constructor(callbacks){this.callbacks=callbacks;this.state='unconfigured';this.decodeQueueSize=0;}
    configure(){this.state='configured';}decode(){}close(){this.state='closed';}
  }
  globalThis.VideoDecoder=Backend;globalThis.EncodedVideoChunk=class{constructor(value){Object.assign(this,value);}};
  try{await work(Backend);}finally{for(const [name,value]of Object.entries(previous)){if(value===undefined)delete globalThis[name];else globalThis[name]=value;}}
}
test('actual adapter250ms pending-output timer closes backend and rejects late output',{timeout:3000},async()=>{
  await controlledBackend(async()=>{
    const decoder=new VideoDecoderAdapter({timeoutMs:250});
    try{
      const pending=decoder.decode(idr()),rejected=assert.rejects(pending,/output timed out/);
      await new Promise(setImmediate);const backend=decoder.decoder;assert.equal(decoder.pending.size,1);
      await rejected;assert.equal(backend.state,'closed');assert.equal(decoder.pending.size,0);assert.equal(decoder.failed,true);
      decoder.reset();let closed=0;backend.callbacks.output({timestamp:1,close(){closed++;}});assert.equal(closed,1);assert.equal(decoder.sequence,null);
    }finally{decoder.close();}
  });
});
test('actual configuration await is separate and late support result is fenced by reset',async()=>{
  await controlledBackend(async Backend=>{
    let supported;Backend.isConfigSupported=config=>new Promise(resolve=>supported=()=>resolve({supported:true,config}));
    const decoder=new VideoDecoderAdapter({timeoutMs:250,configureTimeoutMs:250});
    try{
      const pending=decoder.decode(idr()),rejected=assert.rejects(pending,/reset during configuration/);
      assert.equal(decoder.configuring,true);assert.equal(decoder.pending.size,0);assert.equal(decoder.decoder,null);
      decoder.reset();supported();await rejected;assert.equal(decoder.decoder,null);assert.equal(decoder.pending.size,0);
    }finally{decoder.close();}
  });
});
test('actual predictive support timeout rejects and late fulfillment cannot construct a decoder',{timeout:3000},async()=>{
  await controlledBackend(async Backend=>{
    let supported,created=0;
    globalThis.VideoDecoder=class extends Backend{
      static isConfigSupported(config){return new Promise(resolve=>supported=()=>resolve({supported:true,config}));}
      constructor(callbacks){super(callbacks);created++;}
    };
    const decoder=new VideoDecoderAdapter({timeoutMs:250,configureTimeoutMs:250});
    try{
      await assert.rejects(decoder.decode(idr()),/configuration timed out/);
      assert.equal(decoder.configuring,false);assert.equal(decoder.pending.size,0);assert.equal(created,0);
      supported();await new Promise(setImmediate);
      assert.equal(created,0);assert.equal(decoder.decoder,null);
    }finally{decoder.close();}
  });
});
