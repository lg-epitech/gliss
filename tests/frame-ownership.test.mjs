import test from 'node:test';
import assert from 'node:assert/strict';
import {GlissClient} from '../web/client.js';
import {PredictiveDatagramSocket} from '../web/predictive-datagram.js';
import {PredictiveReceiver} from '../web/predictive-state.js';

function fixture({resizeThrows=false,scheduleThrows=false,drawThrows=false,predictive=false}={}) {
  const names=['window','document','location','WebSocket','requestAnimationFrame','cancelAnimationFrame'];
  const previous=new Map(names.map(name=>[name,Object.getOwnPropertyDescriptor(globalThis,name)]));
  const callbacks=new Map(),draws=[],statuses=[],operations=[];
  let serial=0,width=32,height=32;
  const target={addEventListener(){}};
  class Socket extends EventTarget {
    static OPEN=1;
    constructor(){super();this.readyState=1;this.sent=[];this.predictiveVideo=predictive;}
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
    submit(sequence=1,{buffer=new ArrayBuffer(24),width=64,height=48,keyframe=true}={}){
      let closes=0;
      const frame={width,height,sequence,inputId:0,keyframe,bytes:buffer.byteLength,videoFrame:{
        close(){assert.equal(++closes,1,'VideoFrame closed more than once');operations.push('close');}
      }};
      app.decoder.decode=async()=>frame;
      const socket=app.socket;socket.onmessage({data:buffer});
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

function packet(sequence,keyframe){
  const nal=keyframe?[0,0,1,103,66,0,30,128,0,0,1,104,128,0,0,1,101,128]:[0,0,1,65,128];
  const raw=new Uint8Array(24+nal.length),view=new DataView(raw.buffer);raw.set([71,76,86,49]);
  view.setUint32(4,sequence,true);view.setUint16(12,32,true);view.setUint16(14,32,true);
  view.setUint16(18,+keyframe,true);view.setUint32(20,nal.length,true);raw.set(nal,24);return raw.buffer;
}

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

test('real predictive recovery can close inside a successful admission',{timeout:3000},async()=>{
  const f=fixture({predictive:true});try{
    await f.connect();const receiver=new PredictiveReceiver(),now=performance.now()-12;
    receiver.offerAccessUnit(packet(1,true),now);const first=receiver.next(now);receiver.complete(first.token,true,now);
    receiver.offerAccessUnit(packet(2,false),now);const selected=receiver.next(now);
    receiver.offerAccessUnit(packet(4,false),now);
    // Omit constructor IO; use the real admission, gap recovery and close methods.
    const transport=f.app.socket;
    delete transport.beginVideoDecode;delete transport.completeVideoDecode;
    Object.setPrototypeOf(transport,PredictiveDatagramSocket.prototype);
    Object.assign(transport,{receiver,selected,closedOnce:false,hello:true,delivering:true,
      queuedControls:32,queuedControlBytes:0,writes:Promise.resolve(),expiryTimer:null,assembler:{close(){}},
      transportStats:{presented:0,decodedOutputSuppressed:0,recoveryEnqueued:0,recoveryWriteCompleted:0}});
    const send=transport.send.bind(transport);
    transport.send=data=>{transport.sent.push(JSON.parse(data));return send(data);};
    const output=f.submit(2,{buffer:selected.buffer,width:32,height:32,keyframe:false});await ready();
    f.flush();await output.done;
    assert.equal(transport.closedOnce,true);assert.equal(output.closes,1);
    assert.deepEqual(f.draws,[]);assert.deepEqual(acks(output.socket),[]);
    assert.equal(f.app.metrics.frames,0);assert.equal(f.app.metrics.errors,0);
  }finally{await f.cleanup();}
});

for(const closeBy of ['disconnect','peer close','dispose'])test(`throwing audio callback cannot retain output on ${closeBy}`,{timeout:3000},async()=>{
  const f=fixture();try{
    await f.connect();const output=f.submit();await ready();
    f.app.onEvent=event=>{if(event.type==='audio_state')throw new Error('embedded handler failed');};
    assert.throws(()=>{
      if(closeBy==='peer close'){output.socket.readyState=3;output.socket.onclose({code:1000,reason:'closed'});}
      else f.app[closeBy]();
    },/embedded handler failed/);
    await ready();
    assert.equal(output.closes,1);assert.equal(f.callbacks.size,0);assert.equal(output.socket.readyState,3);
    assert.equal(f.app.decoder.closed,true);assert.deepEqual(f.draws,[]);assert.deepEqual(acks(output.socket),[]);
    if(closeBy==='dispose')assert.equal(f.app.listeners.signal.aborted,true);
    await output.done;
  }finally{f.app.onEvent=()=>{};await f.cleanup();}
});
