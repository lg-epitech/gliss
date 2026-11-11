import test from 'node:test';
import assert from 'node:assert/strict';
import {launchBrowser,startServer} from './helpers.mjs';

// Packets recorded from the real server are replayed through a controlled socket into
// the actual client, WebCodecs decoder, rAF and Canvas.
test('client paints each frame before its ACK and fences recovery, reconnect and close', {timeout:20000}, async t=>{
  const server=await startServer({workload:'scroll'});t.after(server.stop);
  const browser=await launchBrowser();t.after(()=>browser.close());
  const page=await browser.newPage(),pageErrors=[];page.on('pageerror',error=>pageErrors.push(error.message));
  await page.goto(`${server.url}/health`);
  const result=await page.evaluate(async token=>{
    const check=(condition,message)=>{if(!condition)throw new Error(message);};
    const login=await fetch('/api/session',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({token})});
    check(login.ok,'Login failed');
    // A keyframe and its deltas, then a resize, which starts a second keyframe.
    const recorded=await new Promise((resolve,reject)=>{
      const socket=new WebSocket(`ws://${location.host}/ws`),packets=[];let hello;
      socket.binaryType='arraybuffer';socket.onerror=()=>reject(new Error('Recording socket failed'));
      socket.onmessage=event=>{
        if(typeof event.data==='string'){if(JSON.parse(event.data).type==='hello')hello=event.data;return;}
        const view=new DataView(event.data),packet={data:event.data,sequence:view.getUint32(4,true),width:view.getUint16(12,true),keyframe:view.getUint16(18,true)===1};
        packets.push(packet);socket.send(JSON.stringify({type:'ack',sequence:packet.sequence}));
        if(packets.length===4)socket.send(JSON.stringify({type:'resize',width:642,height:360}));
        const resized=packets.filter(packet=>packet.width===642);
        if(resized.length===2){socket.onmessage=null;socket.close();resolve({hello,initial:packets.slice(0,4),resized});}
      };
    });
    const {initial,resized}=recorded;
    check(initial[0].keyframe&&initial.slice(1).every(packet=>!packet.keyframe&&packet.width===960)&&resized[0].keyframe&&!resized[1].keyframe,'Unexpected recorded stream');

    const {GlissClient}=await import('/client.js'),{VideoDecoderAdapter}=await import('/video-decoder.js');
    const canvas=document.createElement('canvas');document.body.append(canvas);
    // An independent decoder gets the same packets: at each ACK the Canvas must show its output.
    const reference=new VideoDecoderAdapter(),surface=document.createElement('canvas'),expected=surface.getContext('2d',{alpha:false,willReadFrequently:true});
    let client,acks=0,paints=0;
    const pixelsMatch=()=>{
      check(canvas.width===surface.width&&canvas.height===surface.height,'Canvas size differs from the decoded frame');
      const actual=client.ctx.getImageData(0,0,canvas.width,canvas.height).data,wanted=expected.getImageData(0,0,surface.width,surface.height).data;
      check(actual.every((value,index)=>value===wanted[index]),'Canvas differs from the independently decoded frame');
    };
    window.WebSocket=class{
      static OPEN=1;
      constructor(){this.readyState=1;this.sent=[];}
      send(text){const message=JSON.parse(text);if(message.type==='ack'){check(paints===acks+1,'ACK did not follow its paint');pixelsMatch();acks++;}this.sent.push(message);}
      close(){this.readyState=3;}
    };
    const events=[];
    client=new GlissClient(canvas,()=>{},event=>events.push(event));await client.connect();
    const draw=client.ctx.drawImage.bind(client.ctx);
    client.ctx.drawImage=(...args)=>{draw(...args);paints++;};
    const hello=async()=>{client.socket.onmessage({data:recorded.hello});await client.chain;};
    const deliver=async packet=>{
      const before={paints,acks},frame=await reference.decode(packet.data);
      if(surface.width!==frame.width||surface.height!==frame.height){surface.width=frame.width;surface.height=frame.height;}
      expected.drawImage(frame.videoFrame,0,0);frame.videoFrame.close();
      client.socket.onmessage({data:packet.data});await client.chain;
      check(paints===before.paints+1&&acks===before.acks+1,`Frame ${packet.sequence} was not painted and acknowledged once`);
      check(client.socket.sent.at(-1).sequence===packet.sequence,'ACK names another frame');
    };
    await hello();for(const packet of initial)await deliver(packet);

    const beforeFailure={paints,acks};
    client.socket.onmessage({data:initial[3].data.slice(0,-1)});await client.chain;
    check(client.metrics.errors===1&&client.socket.sent.at(-1).type==='resync','Malformed packet did not request recovery');
    check(paints===beforeFailure.paints&&acks===beforeFailure.acks,'Failed decode painted or acknowledged');pixelsMatch();
    // Recovery starts from a keyframe, as the server's answer to resync does.
    reference.reset();await deliver(initial[0]);await deliver(initial[1]);
    await deliver(resized[0]);await deliver(resized[1]);
    check(canvas.width===642&&canvas.height===360,'Resize did not replace the Canvas');

    const old=client.socket;await client.connect();
    const beforeStale={paints,acks,events:events.length};
    old.onmessage({data:recorded.hello});old.onmessage({data:initial[0].data});old.onclose({code:1000,reason:'stale'});await client.chain;
    check(paints===beforeStale.paints&&acks===beforeStale.acks&&events.length===beforeStale.events,'Stale socket affected the new connection');
    reference.reset();await hello();await deliver(initial[0]);await deliver(initial[1]);

    // Close after decode but before rAF: the pending frame cannot paint or ACK.
    const nativeRaf=window.requestAnimationFrame;
    let release,ready;
    const waiting=new Promise(resolve=>ready=resolve);
    window.requestAnimationFrame=callback=>{release=callback;ready();return 1;};
    const beforeClose={paints,acks};
    client.socket.onmessage({data:initial[2].data});await waiting;
    client.socket.readyState=3;client.socket.onclose({code:1000,reason:'done'});
    window.requestAnimationFrame=nativeRaf;release(performance.now());await client.chain;
    check(paints===beforeClose.paints&&acks===beforeClose.acks,'Closed connection painted or acknowledged');pixelsMatch();
    check(events.filter(event=>event.type==='close').length===1,'Close event was not delivered exactly once');
    const result={acks,errors:client.metrics.errors,closed:events.find(event=>event.type==='close').code,
      hellos:events.filter(event=>event.type==='hello').map(event=>event.codec)};
    client.dispose();reference.close();return result;
  },server.token);
  assert.deepEqual(result,{acks:10,errors:1,closed:1000,hellos:['h264','h264']});
  assert.deepEqual(pageErrors,[]);
});
