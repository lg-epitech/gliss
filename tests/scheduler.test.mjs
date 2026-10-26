import test from 'node:test';
import assert from 'node:assert/strict';
import {launchBrowser,startServer,loginPage} from './helpers.mjs';
import {startRfb} from './mock-rfb.mjs';

// Delay only outgoing ACKs. Production packet parsing, decoding, painting and
// reference handling remain active. A gate can pause the existing media chain.
async function observe(page,{codec,video=true}={}){
  await page.evaluate(({codec,video})=>{
    const client=window.gliss;
    const state=window.schedulerTest={hold:false,acks:[],packets:[],frames:[],stats:[],validationErrors:[],keyPackets:[],fence:null,codec,video};
    const send=client.socket.send.bind(client.socket);
    state.send=value=>send(JSON.stringify(value));
    client.socket.send=data=>{
      if(typeof data==='string'){
        const message=JSON.parse(data);
        if(message.type==='ack'&&state.hold){
          if(state.acks.length>=32)throw new Error('Test ACK queue exceeded its bound');
          state.acks.push(message);return;
        }
      }
      send(data);
    };
    state.release=count=>{for(const ack of state.acks.splice(0,count??state.acks.length))state.send(ack);};
    state.resume=()=>{state.hold=false;state.release();};
    state.pause=async()=>{
      await client.chain;
      const gate=new Promise(resolve=>state.openGate=resolve);
      client.chain=client.chain.then(()=>gate);
    };
    // These fixture dimensions make every pseudo-text rectangle fall inside the
    // video region, where the exact demo renderer overwrites it. Recover tick
    // modulo256 from a known source pixel, then verify the complete RGBA plane.
    state.validatePixels=(pixels,width,height,id)=>{
      if(width!==320||height!==240)return `unexpected fixture size ${width}x${height}`;
      const sample=(100*width+100)*4,tick=((pixels[sample]-100+256)*205)&255;
      const checksum=(id&255)^((id>>>8)&255)^((id>>>16)&255)^0xa5;
      const bits=((id&0xffffff)|(checksum<<24))>>>0;
      for(let y=0;y<height;y++)for(let x=0;x<width;x++){
        let r=34,g=48,b=61;
        if(x>=20&&x<width-20&&y>=50&&y<height-30){r=242;g=244;b=246;}
        if(x>=20&&x<width-20&&y>=50&&y<75){r=80;g=97;b=116;}
        if(x>=30&&x<width-30&&y>=80&&y<height-30){r=(x+tick*5)&255;g=(y+tick*3)&255;b=((x^y)+tick*11)&255;}
        if(x>=10&&x<18&&y>=100&&y<108){r=230;g=91;b=56;}
        if(x<24&&y<24){r=id&255;g=(id>>>8)&255;b=(id>>>16)&255;}
        if(x>=32&&x<96&&y<32){const bit=Math.floor((x-32)/8)+8*Math.floor(y/8);r=g=b=(bits&(1<<bit))?235:20;}
        const position=(y*width+x)*4;
        if(pixels[position]!==r||pixels[position+1]!==g||pixels[position+2]!==b||pixels[position+3]!==255)return `RGBA mismatch at ${x},${y}, tick${tick}, input${id}`;
      }
      return null;
    };
    client.socket.addEventListener('message',event=>{
      if(typeof event.data==='string')return;
      const view=new DataView(event.data),packet={sequence:view.getUint32(4,true),inputId:view.getUint32(8,true),width:view.getUint16(12,true),height:view.getUint16(14,true),keyframe:Boolean(view.getUint16(18,true)&1),bytes:event.data.byteLength};
      state.packets.push(packet);if(state.packets.length>128)state.packets.shift();
      if(state.fence!==null&&packet.keyframe&&packet.sequence>state.fence&&state.keyPackets.length<2)state.keyPackets.push({header:packet,buffer:event.data});
    });
    const original=client.onEvent;
    client.onEvent=event=>{
      original(event);
      if(event.type==='stream_stats'){state.stats.push(event);if(state.stats.length>8)state.stats.shift();return;}
      if(event.type!=='frame')return;
      const frame=event.frame;
      const observed={sequence:frame.sequence,inputId:frame.inputId,keyframe:frame.keyframe,width:frame.width,height:frame.height,probe:video?client.readProbe():null};
      if(video&&frame.width===320&&frame.height===240&&state.armed){
        if(observed.probe!==(frame.inputId&0xffffff))state.validationErrors.push(`Canvas probe mismatch at ${frame.sequence}`);
        if(codec!=='h264'){
          const error=state.validatePixels(client.ctx.getImageData(0,0,frame.width,frame.height).data,frame.width,frame.height,frame.inputId);
          if(error)state.validationErrors.push(error);
        }
      }
      state.frames.push(observed);if(state.frames.length>128)state.frames.shift();
    };
  },{codec,video});
}

async function fixture(t,options,{rfb=false}={}){
  let peer,server,browser,page;
  const consoleErrors=[],pageErrors=[];
  t.after(async()=>{
    t.diagnostic(JSON.stringify({console_errors:consoleErrors.slice(0,8),page_errors:pageErrors.slice(0,8)}));
    await browser?.close();await server?.stop();await peer?.stop();
  });
  if(rfb)peer=await startRfb();
  server=await startServer({...options,...(peer?{vnc:peer.address}:{workload:'video'})});
  browser=await launchBrowser();page=await browser.newPage();
  page.on('console',message=>{if(message.type()==='error')consoleErrors.push(message.text());});
  page.on('pageerror',error=>pageErrors.push(error.message));
  await loginPage(page,server);
  await observe(page,{codec:options.codec,video:!rfb});
  if(!rfb){
    await page.evaluate(()=>window.gliss.send({type:'resize',width:320,height:240}));
    await page.waitForFunction(()=>window.gliss.canvas.width===320&&window.gliss.canvas.height===240,null,{timeout:5000});
    const id=await page.evaluate(()=>window.gliss.send({type:'pointer',x:10,y:100,buttons:0}));
    await page.waitForFunction(id=>window.gliss.readProbe()===(id&0xffffff),id,{timeout:5000});
    await page.evaluate(()=>window.schedulerTest.armed=true);
  }
  return{peer,server,browser,page,pageErrors};
}

async function holdWindow(page,count){
  await page.evaluate(()=>window.schedulerTest.hold=true);
  await page.waitForFunction(count=>window.schedulerTest.acks.length===count,count,{timeout:5000});
  const before=await page.evaluate(()=>({packets:window.schedulerTest.packets.length,frames:window.schedulerTest.frames.length,acks:window.schedulerTest.acks.map(ack=>ack.sequence)}));
  // A quiet interval is the assertion's observation bound, not a latency sample.
  await page.waitForTimeout(180);
  const after=await page.evaluate(()=>({packets:window.schedulerTest.packets.length,frames:window.schedulerTest.frames.length,acks:window.schedulerTest.acks.map(ack=>ack.sequence)}));
  assert.deepEqual(after,before,'withheld ACKs must stop admission at the configured bound');
  return before;
}

async function assertHealthy(page,pageErrors,{native=false}={}){
  const result=await page.evaluate(()=>({errors:window.gliss.metrics.errors,validationErrors:window.schedulerTest.validationErrors,frames:window.schedulerTest.frames}));
  assert.deepEqual(result.validationErrors,[]);
  assert.deepEqual(pageErrors,[]);
  if(!native)assert.equal(result.errors,0);
  for(let index=1;index<result.frames.length;index++){
    const previous=result.frames[index-1],frame=result.frames[index];
    if(!frame.keyframe)assert.equal(frame.sequence,(previous.sequence+1)>>>0,'every dependent frame must follow the decoded reference');
  }
  return result;
}

for(const codec of ['adaptive','h264'])test(`scheduler ${codec}: delayed ACKs fill a bounded four-frame window with real pixels`,{timeout:20000},async t=>{
  const {page,pageErrors}=await fixture(t,{codec,inFlightFrames:4,streamStats:true});
  const full=await holdWindow(page,4);
  assert.equal(new Set(full.acks).size,4);
  await page.evaluate(()=>window.schedulerTest.release(1));
  await page.waitForFunction(last=>window.schedulerTest.acks.length===4&&window.schedulerTest.acks.at(-1).sequence>last,full.acks.at(-1),{timeout:5000});
  await page.waitForFunction(()=>window.schedulerTest.stats.some(stats=>stats.reserved_frames===4),null,{timeout:3000});
  const stats=await page.evaluate(()=>window.schedulerTest.stats.findLast(stats=>stats.reserved_frames===4));
  assert.equal(stats.in_flight_frames_limit,4);
  assert.equal(stats.counters.reserved_frames_high_water,4);
  assert.ok(stats.counters.observed_capture_replacements>0);
  assert.ok(stats.counters.encode_ms.count>0&&stats.counters.write_ms.count>0&&stats.counters.enqueue_to_ack_ms.count>0);
  const result=await assertHealthy(page,pageErrors);
  assert.ok(result.frames.filter(frame=>frame.width===320&&frame.height===240).length>=5);
  assert.equal(result.errors,0);
  await page.evaluate(()=>window.schedulerTest.resume());
});

test('scheduler: byte budget stops before the frame limit and retains one waiting encoded packet',{timeout:20000},async t=>{
  const cap=512*1024;
  const {page,pageErrors}=await fixture(t,{codec:'raw',inFlightFrames:4,inFlightBytes:cap,streamStats:true});
  const full=await holdWindow(page,2);
  await page.waitForFunction(()=>window.schedulerTest.stats.some(stats=>stats.reserved_frames===2&&stats.waiting_encoded_bytes>0),null,{timeout:3000});
  const evidence=await page.evaluate(()=>{
    const state=window.schedulerTest;
    return{stats:state.stats.findLast(stats=>stats.reserved_frames===2&&stats.waiting_encoded_bytes>0),packets:state.acks.map(ack=>state.packets.find(packet=>packet.sequence===ack.sequence))};
  });
  assert.ok(evidence.packets.every(packet=>packet.bytes<=cap));
  const reserved=evidence.packets.reduce((total,packet)=>total+packet.bytes,0);
  assert.equal(evidence.stats.reserved_bytes,reserved);
  assert.ok(reserved<=cap&&evidence.stats.reserved_frames<4);
  assert.equal(evidence.stats.encoder_busy,false);
  await page.evaluate(()=>window.schedulerTest.release(1));
  await page.waitForFunction(last=>window.schedulerTest.acks.length===2&&window.schedulerTest.acks.at(-1).sequence>last,full.acks.at(-1),{timeout:5000});
  await page.waitForFunction(()=>window.schedulerTest.stats.some(stats=>stats.counters.byte_capacity_wait_ms.count>0),null,{timeout:3000});
  await assertHealthy(page,pageErrors);
});

test('scheduler: one legal oversized packet progresses alone until its ACK',{timeout:20000},async t=>{
  const cap=1024;
  const {page,pageErrors}=await fixture(t,{codec:'raw',inFlightFrames:4,inFlightBytes:cap,streamStats:true});
  const full=await holdWindow(page,1);
  await page.waitForFunction(()=>window.schedulerTest.stats.some(stats=>stats.reserved_frames===1&&stats.reserved_bytes>1024),null,{timeout:3000});
  const stats=await page.evaluate(()=>window.schedulerTest.stats.findLast(stats=>stats.reserved_frames===1&&stats.reserved_bytes>1024));
  assert.equal(stats.in_flight_frames_limit,4);
  assert.equal(stats.counters.reserved_frames_high_water,1);
  assert.ok(stats.counters.oversized_packets>0);
  assert.equal(stats.waiting_encoded_bytes,0);
  await page.evaluate(()=>window.schedulerTest.release(1));
  await page.waitForFunction(last=>window.schedulerTest.acks.length===1&&window.schedulerTest.acks[0].sequence>last,full.acks[0],{timeout:5000});
  await assertHealthy(page,pageErrors);
});

for(const codec of ['adaptive','h264'])test(`scheduler ${codec}: resync retires old ACKs and queued media before an independent reference`,{timeout:20000},async t=>{
  const {page,pageErrors}=await fixture(t,{codec,inFlightFrames:4,streamStats:true});
  await holdWindow(page,4);
  await page.evaluate(()=>window.schedulerTest.pause());
  const before=await page.evaluate(()=>window.schedulerTest.packets.at(-1).sequence);
  await page.evaluate(()=>window.schedulerTest.release(1));
  await page.waitForFunction(before=>window.schedulerTest.packets.at(-1).sequence>before,before,{timeout:5000});
  const fence=await page.evaluate(()=>{
    const state=window.schedulerTest,client=window.gliss;
    state.fence=state.packets.at(-1).sequence;
    client.decoder.reset();state.send({type:'resync'});
    // Same WebSocket ordering guarantees these old ACKs follow resync on wire.
    state.release();state.openGate();return state.fence;
  });
  await page.waitForFunction(fence=>window.schedulerTest.frames.some(frame=>frame.sequence>fence&&frame.keyframe),fence,{timeout:5000});
  const independent=await page.evaluate(async()=>{
    const state=window.schedulerTest,candidate=state.keyPackets[0];
    if(!candidate)throw new Error('No independent packet after generation barrier');
    let decoder,frame;
    if(state.codec==='h264'){
      const {VideoDecoderAdapter}=await import('/video-decoder.js');decoder=new VideoDecoderAdapter();frame=await decoder.decode(candidate.buffer);
    }else{
      const {Decoder}=await import('/decoder.js');decoder=new Decoder();frame=await decoder.decode(candidate.buffer);
    }
    const surface=document.createElement('canvas');surface.width=frame.width;surface.height=frame.height;
    const ctx=surface.getContext('2d',{alpha:false});
    if(frame.videoFrame){try{ctx.drawImage(frame.videoFrame,0,0);}finally{frame.videoFrame.close();}}else ctx.putImageData(new ImageData(frame.pixels,frame.width,frame.height),0,0);
    const {decodeProbe}=await import('/probe.js');
    const probe=decodeProbe((x,y)=>ctx.getImageData(x,y,1,1).data);
    const exactError=state.codec==='h264'?null:state.validatePixels(ctx.getImageData(0,0,frame.width,frame.height).data,frame.width,frame.height,frame.inputId);
    decoder.close?.();return{sequence:frame.sequence,keyframe:frame.keyframe,inputId:frame.inputId,probe,exactError};
  });
  assert.ok(independent.sequence>fence&&independent.keyframe);
  assert.equal(independent.probe,independent.inputId&0xffffff);
  assert.equal(independent.exactError,null);
  await page.waitForFunction(()=>window.schedulerTest.stats.some(stats=>stats.counters.ignored_retired_acks>=3),null,{timeout:3000});
  const state=await page.evaluate(()=>({errors:window.gliss.metrics.errors,last:window.schedulerTest.frames.at(-1).sequence}));
  await page.evaluate(()=>window.schedulerTest.resume());
  await page.waitForFunction(last=>window.schedulerTest.frames.at(-1).sequence>=last+3,state.last,{timeout:5000});
  const healthy=await assertHealthy(page,pageErrors,{native:true});
  assert.equal(healthy.errors,state.errors,'recovery must stop generating decoder errors after the new reference');
});

test('scheduler: upstream input and server key/button release continue while media ACK is withheld',{timeout:20000},async t=>{
  const {page,peer,pageErrors}=await fixture(t,{codec:'raw',inFlightFrames:1,streamStats:true},{rfb:true});
  await page.evaluate(()=>{window.schedulerTest.hold=true;window.gliss.send({type:'key',keysym:98,down:true});});
  await peer.waitFor(inputs=>inputs.some(input=>input.type==='key'&&input.keysym===98&&input.down));
  await page.waitForFunction(()=>window.schedulerTest.acks.length===1,null,{timeout:5000});
  const before=await page.evaluate(()=>window.gliss.metrics.frames);
  await page.evaluate(()=>{window.gliss.send({type:'key',keysym:97,down:true});window.gliss.send({type:'pointer',x:10,y:11,buttons:1});});
  await peer.waitFor(inputs=>inputs.some(input=>input.type==='key'&&input.keysym===97&&input.down)&&inputs.some(input=>input.type==='pointer'&&input.x===10&&input.y===11&&input.buttons===1),2000);
  assert.equal(await page.evaluate(()=>window.gliss.metrics.frames),before,'upstream input must not depend on media ACK credit');
  const pixels=await page.evaluate(()=>Array.from(window.gliss.ctx.getImageData(0,0,96,64).data));
  for(let position=0;position<pixels.length;position+=4)assert.deepEqual(pixels.slice(position,position+4),[31,67,129,255]);
  // Skip GlissClient.release(): only server source teardown can send these ups.
  await page.evaluate(()=>window.gliss.socket.close());
  await peer.waitFor(inputs=>[97,98].every(keysym=>inputs.some(input=>input.type==='key'&&input.keysym===keysym&&!input.down))&&inputs.some(input=>input.type==='pointer'&&input.x===10&&input.y===11&&input.buttons===0),3000);
  assert.deepEqual(pageErrors,[]);
});
