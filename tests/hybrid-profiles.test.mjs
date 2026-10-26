import test from 'node:test';
import assert from 'node:assert/strict';
import {launchBrowser,startServer,loginPage} from './helpers.mjs';

for(const hybridBase of ['global','global-cached'])test(`live Hybrid ${hybridBase}/camera-scene-off: interframes, input, recovery and resize`,{timeout:30000},async t=>{
  const options={codec:'hybrid',hybridBase,hybridVideoProfile:'camera-scene-off',workload:'video',fps:15,h264Bitrate:3000000};
  const server=await startServer(options);let browser;
  t.after(async()=>{await browser?.close();await server.stop();});
  assert.equal((await fetch(`${server.url}/api/stats`)).status,401,'configuration metadata stays authenticated');
  browser=await launchBrowser();const page=await browser.newPage(),errors=[];let hello;
  page.on('pageerror',error=>errors.push(error.message));
  page.on('websocket',socket=>socket.on('framereceived',({payload})=>{
    if(typeof payload==='string'){
      const value=JSON.parse(payload);if(value.type==='hello')hello=value;
    }
  }));
  await loginPage(page,server);
  const expected={base_mode:hybridBase,video_profile:'camera-scene-off',video_usage:'camera-video-real-time',scene_change_detection:false};
  assert.equal(hello?.codec,'hybrid');assert.equal(hello?.protocol,'GLH1');assert.deepEqual(hello?.hybrid_config,expected);
  const stats=await page.evaluate(async()=>{const response=await fetch('/api/stats');if(!response.ok)throw new Error('Authenticated stats unavailable');return response.json();});
  assert.equal(stats.codec,'hybrid');assert.deepEqual(stats.hybrid_config,expected);
  // Observe actual completed VideoDecoder/Canvas composition, not only wire flags.
  await page.evaluate(()=>{
    window.__profileFrames=[];
    const decoder=window.gliss.decoder,decode=decoder.decode.bind(decoder);
    decoder.decode=async packet=>{
      const frame=await decode(packet);
      window.__profileFrames.push({sequence:frame.sequence,keyframe:frame.keyframe,videoKeyframe:frame.videoKeyframe,
        activeVideoTiles:frame.activeVideoTiles,videoBytes:frame.videoBytes,width:frame.width,height:frame.height});
      if(window.__profileFrames.length>200)window.__profileFrames.shift();return frame;
    };
  });
  await page.waitForFunction(()=>window.__profileFrames.some(frame=>frame.activeVideoTiles>0&&!frame.videoKeyframe&&frame.videoBytes>24),null,{timeout:10000});
  const id=await page.evaluate(()=>window.gliss.send({type:'key',keysym:97,down:true},{pixelEcho:true}));
  await page.waitForFunction(id=>window.gliss.readProbe()===(id&0xffffff),id,{timeout:5000});
  assert.deepEqual(await page.evaluate(()=>Array.from(window.gliss.ctx.getImageData(0,0,1,1).data)),[id&255,(id>>>8)&255,(id>>>16)&255,255]);
  await page.evaluate(()=>window.gliss.send({type:'key',keysym:97,down:false}));
  const recovery=await page.evaluate(()=>{
    const last=window.__profileFrames.at(-1).sequence;
    window.gliss.socket.send(JSON.stringify({type:'resync'}));return last;
  });
  await page.waitForFunction(last=>window.__profileFrames.some(frame=>frame.sequence>last&&frame.keyframe&&frame.activeVideoTiles>0&&frame.videoKeyframe),recovery,{timeout:5000});
  await page.evaluate(()=>window.gliss.send({type:'resize',width:642,height:360}));
  await page.waitForFunction(()=>window.__profileFrames.some(frame=>frame.width===642&&frame.height===360&&frame.keyframe),null,{timeout:5000});
  await page.waitForFunction(()=>window.__profileFrames.some(frame=>frame.width===642&&frame.height===360&&frame.activeVideoTiles>0&&!frame.videoKeyframe),null,{timeout:10000});
  const second=await page.evaluate(()=>window.gliss.send({type:'key',keysym:98,down:false},{pixelEcho:true}));
  await page.waitForFunction(id=>window.gliss.readProbe()===(id&0xffffff),second,{timeout:5000});
  assert.deepEqual(await page.evaluate(()=>Array.from(window.gliss.ctx.getImageData(0,0,1,1).data)),[second&255,(second>>>8)&255,(second>>>16)&255,255]);
  assert.deepEqual(errors,[]);
});
