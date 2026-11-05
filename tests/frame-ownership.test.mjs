import test from 'node:test';
import assert from 'node:assert/strict';
import {GlissClient} from '../web/client.js';

function fixture({resizeThrows=false,scheduleThrows=false,drawThrows=false,predictive=false}={}) {
  const names=['window','document','location','WebSocket','requestAnimationFrame','cancelAnimationFrame'];
  const previous=new Map(names.map(name=>[name,Object.getOwnPropertyDescriptor(globalThis,name)]));
  const callbacks=new Map(),draws=[],statuses=[],operations=[];
  let serial=0,width=32,height=32;
  const target={addEventListener(){}};
  class Socket {
    static OPEN=1;
    constructor(){this.readyState=1;this.sent=[];}
    send(data){const message=JSON.parse(data);this.sent.push(message);if(message.type==='ack')operations.push('ack');}
    close(){this.readyState=3;}
  }
  Object.assign(globalThis,{window:target,document:target,location:{protocol:'http:',pathname:'/',host:'owned.invalid'},WebSocket:Socket,
    requestAnimationFrame(callback){
      if(scheduleThrows)throw new Error('schedule failed');
      const id=++serial;callbacks.set(id,callback);return id;
    },cancelAnimationFrame(id){callbacks.delete(id);}});
  const canvas={addEventListener(){},focus(){},get width(){return width;},set width(value){
    if(resizeThrows)throw new Error('resize failed');width=value;
  },get height(){return height;},set height(value){height=value;},getContext(){return{
    drawImage(frame){if(drawThrows)throw new Error('draw failed');draws.push(frame);operations.push('draw');}
  };}};
  const app=new GlissClient(canvas,status=>{
    statuses.push(status);if(status.startsWith('Display recovery:'))operations.push('recovery');
  },()=>{},{url:'ws://owned.invalid'});
  const flush=()=>{for(const [id,callback]of [...callbacks]){callbacks.delete(id);callback(performance.now());}};
  return {app,callbacks,draws,statuses,operations,flush,
    async connect(){
      await app.connect();
      app.socket.onmessage({data:JSON.stringify({type:'hello',codec:'h264'})});await app.chain;
      if(predictive){app.socket.beginVideoDecode=()=>{};app.socket.completeVideoDecode=()=>true;}
    },
    submit(sequence=1){
      let closes=0;
      const frame={width:64,height:48,sequence,inputId:0,keyframe:true,bytes:24,videoFrame:{
        close(){assert.equal(++closes,1,'VideoFrame closed more than once');operations.push('close');}
      }};
      app.decoder.decode=async()=>frame;
      const socket=app.socket;socket.onmessage({data:new ArrayBuffer(24)});
      return {socket,done:app.chain,get closes(){return closes;}};
    },
    async cleanup(){
      try{app.dispose();flush();await app.chain;}finally{
        for(const [name,descriptor]of previous){if(descriptor)Object.defineProperty(globalThis,name,descriptor);else delete globalThis[name];}
      }
    }
  };
}
const ready=()=>new Promise(setImmediate);
const acks=socket=>socket.sent.filter(message=>message.type==='ack');

test('successful presentation closes its frame before acknowledging it',{timeout:3000},async()=>{
  const f=fixture();try{
    await f.connect();const output=f.submit();await ready();f.flush();await output.done;
    assert.equal(output.closes,1);assert.deepEqual(f.operations,['draw','close','ack']);
    assert.deepEqual(acks(output.socket),[{type:'ack',sequence:1}]);
  }finally{await f.cleanup();}
});

for(const closeBy of ['disconnect','peer close'])test(`pending output is released on ${closeBy} without RAF`,{timeout:3000},async()=>{
  const f=fixture();try{
    await f.connect();const output=f.submit();await ready();assert.equal(f.callbacks.size,1);
    if(closeBy==='disconnect')f.app.disconnect();
    else{output.socket.readyState=3;output.socket.onclose({code:1000,reason:'closed'});}
    await ready();
    assert.equal(output.closes,1);assert.equal(f.callbacks.size,0);
    assert.deepEqual(f.draws,[]);assert.deepEqual(acks(output.socket),[]);assert.equal(f.app.metrics.errors,0);await output.done;
  }finally{await f.cleanup();}
});

test('a late old RAF callback cannot lose the next connection cancellation',{timeout:3000},async()=>{
  const f=fixture();try{
    await f.connect();const old=f.submit();await ready();
    const late=[...f.callbacks.values()][0];f.app.disconnect();await ready();
    assert.equal(old.closes,1);await old.done;
    await f.connect();const fresh=f.submit(2);await ready();late(performance.now());
    f.app.disconnect();await ready();
    assert.equal(old.closes,1);assert.equal(fresh.closes,1);assert.equal(f.callbacks.size,0);
    assert.deepEqual(f.draws,[]);assert.deepEqual(acks(old.socket),[]);assert.deepEqual(acks(fresh.socket),[]);
    assert.equal(f.app.metrics.errors,0);await fresh.done;
  }finally{await f.cleanup();}
});

for(const failure of ['resize','schedule','draw'])test(`${failure} failure releases the decoded output without ACK`,{timeout:3000},async()=>{
  const f=fixture({[`${failure}Throws`]:true});try{
    await f.connect();const output=f.submit();await ready();f.flush();await output.done;
    assert.equal(output.closes,1);assert.deepEqual(acks(output.socket),[]);
    assert.equal(f.app.metrics.errors,1);assert.ok(f.statuses.some(status=>status.includes(`${failure} failed`)));
    assert.deepEqual(f.operations,['close','recovery'],'recovery callbacks must observe a released frame');
  }finally{await f.cleanup();}
});

test('predictive admission failure releases the decoded output',{timeout:3000},async()=>{
  const f=fixture({predictive:true});try{
    await f.connect();const output=f.submit();await ready();
    f.app.socket.completeVideoDecode=()=>{throw new Error('admission failed');};
    f.flush();await output.done;
    assert.equal(output.closes,1);assert.deepEqual(f.draws,[]);assert.deepEqual(acks(output.socket),[]);
    assert.equal(f.app.metrics.errors,1);
  }finally{await f.cleanup();}
});
