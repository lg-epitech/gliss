import test from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {launchBrowser,startServer} from './helpers.mjs';

// These six native frames contain a keyframe, empty/sparse/full deltas and resize.
// Decoder codec coverage lives in browser.test.mjs; this checks the client path.
test('lossless client paints damage before ACK and fences recovery, reconnect and close', {timeout:15000}, async t=>{
  const fixtures=JSON.parse(await readFile(new URL('../test-support/rust/codec/fixtures.json',import.meta.url),'utf8')).filter(row=>row.mode==='raw');
  const server=await startServer();t.after(server.stop);
  const browser=await launchBrowser();t.after(()=>browser.close());
  const page=await browser.newPage(),pageErrors=[];page.on('pageerror',error=>pageErrors.push(error.message));
  await page.goto(`${server.url}/health`);
  const result=await page.evaluate(async fixtures=>{
    const {GlissClient}=await import('/client.js');
    const check=(condition,message)=>{if(!condition)throw new Error(message);};
    const bytes=hex=>Uint8Array.from(hex.match(/../g)||[],value=>parseInt(value,16));
    const canvas=document.createElement('canvas');document.body.append(canvas);
    let expected,client,acks=0;
    const pixelsMatch=()=>{
      const actual=client.ctx.getImageData(0,0,canvas.width,canvas.height).data;
      check(actual.length===expected.length&&actual.every((value,i)=>value===expected[i]),'Whole Canvas differs from native pixels');
    };
    // Controlled delivery uses the actual client, decoder, rAF and Canvas.
    window.WebSocket=class{
      static OPEN=1;
      constructor(){this.readyState=1;this.sent=[];}
      send(text){const message=JSON.parse(text);if(message.type==='ack'){pixelsMatch();acks++;}this.sent.push(message);}
      close(){this.readyState=3;}
    };
    const events=[],paints=[];
    client=new GlissClient(canvas,()=>{},event=>events.push(event));await client.connect();
    const paint=client.ctx.putImageData.bind(client.ctx);
    client.ctx.putImageData=(...args)=>{
      paint(...args);
      paints.push({arguments:args.length,area:args.length===3?canvas.width*canvas.height:args[5]*args[6]});
    };
    const deliver=async(index,{restore=false,resizeCanvas=false}={})=>{
      const row=fixtures[index],before=paints.length,ackBefore=acks;
      expected=bytes(row.expected_pixels_hex);
      if(restore){
        // Model the cleared bitmap and restoration notification from Canvas2D.
        canvas.width=canvas.width;canvas.dispatchEvent(new Event('contextrestored'));
      }
      if(resizeCanvas)canvas.width++;
      client.socket.onmessage({data:bytes(row.packet_hex).buffer});await client.chain;
      check(acks===ackBefore+1,`Missing ACK for fixture ${index}`);
      const calls=paints.slice(before);
      if(restore||resizeCanvas)check(calls.length===1&&calls[0].arguments===3,'Invalidated Canvas was not repainted completely');
      else if(index===1)check(calls.length===0,'Empty delta submitted Canvas work');
      else{
        check(calls.length===1,'Expected one Canvas submission');
        if(index===2)check(calls[0].arguments===7&&calls[0].area<row.width*row.height,'Sparse delta submitted the whole frame');
        if(index===0||index===4||index===5)check(calls[0].arguments===3,'Full frame missed the ordinary Canvas path');
      }
    };
    await deliver(0);await deliver(1,{restore:true});await deliver(2,{restore:true});
    const beforeFailure={paints:paints.length,acks};
    client.socket.onmessage({data:bytes(fixtures[3].packet_hex).slice(0,-1).buffer});await client.chain;
    check(client.metrics.errors===1&&client.socket.sent.at(-1).type==='resync','Malformed delta did not request recovery');
    check(paints.length===beforeFailure.paints&&acks===beforeFailure.acks,'Failed decode painted or acknowledged');pixelsMatch();
    await deliver(0);for(let index=1;index<fixtures.length;index++)await deliver(index);
    check(canvas.width===19&&canvas.height===17,'Resize did not replace the Canvas');

    const old=client.socket;await client.connect();
    const beforeStale={paints:paints.length,acks};
    old.onmessage({data:bytes(fixtures[0].packet_hex).buffer});old.onclose({code:1000,reason:'stale'});await client.chain;
    check(paints.length===beforeStale.paints&&acks===beforeStale.acks&&!events.some(event=>event.type==='close'),'Stale socket affected the new connection');
    await deliver(0);await deliver(1,{resizeCanvas:true});

    // Close after decode but before rAF: the pending sparse frame cannot paint/ACK.
    const nativeRaf=window.requestAnimationFrame;
    let release,ready;
    const waiting=new Promise(resolve=>ready=resolve);
    window.requestAnimationFrame=callback=>{release=callback;ready();return 1;};
    const beforeClose={paints:paints.length,acks};
    client.socket.onmessage({data:bytes(fixtures[2].packet_hex).buffer});await waiting;
    client.socket.readyState=3;client.socket.onclose({code:1000,reason:'done'});
    window.requestAnimationFrame=nativeRaf;release(performance.now());await client.chain;
    check(paints.length===beforeClose.paints&&acks===beforeClose.acks,'Closed connection painted or acknowledged');pixelsMatch();
    check(events.filter(event=>event.type==='close').length===1,'Close event was not delivered exactly once');
    const result={acks,errors:client.metrics.errors,closed:events.find(event=>event.type==='close').code};
    client.dispose();canvas.dispatchEvent(new Event('contextrestored'));
    check(!client.needsFullPaint,'Disposal retained the restoration listener');return result;
  },fixtures);
  assert.deepEqual(result,{acks:11,errors:1,closed:1000});
  assert.deepEqual(pageErrors,[]);
});
