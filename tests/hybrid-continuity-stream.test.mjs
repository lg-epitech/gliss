import test from 'node:test';
import assert from 'node:assert/strict';
import {hybridServerArgs,launchBrowser,startServer} from './helpers.mjs';

const options=grid=>({codec:'hybrid-continuity',hybridMaskGrid:grid,hybridBase:'global-cached',
  hybridVideoProfile:'camera-scene-off',workload:'video',fps:15,h264Bitrate:3000000});
const expectedConfig=grid=>({base_mode:'global-cached',video_profile:'camera-scene-off',
  video_usage:'camera-video-real-time',scene_change_detection:false,mask_grid:grid,base_tile:64});

async function observeFrames(page){
  await page.evaluate(()=>{
    window.__continuityFrames=[];window.__continuityDecoderIds=new WeakMap();window.__continuityNextDecoderId=0;
    const client=window.gliss,original=client.onEvent;
    client.onEvent=event=>{
      if(event.type==='frame'){
        const frame=event.frame,native=client.decoder.video?.decoder;let nativeId=null;
        if(native){if(!window.__continuityDecoderIds.has(native))window.__continuityDecoderIds.set(native,++window.__continuityNextDecoderId);nativeId=window.__continuityDecoderIds.get(native);}
        window.__continuityFrames.push({sequence:frame.sequence,inputId:frame.inputId,keyframe:frame.keyframe,
          videoKeyframe:frame.videoKeyframe,videoBytes:frame.videoBytes,activeVideoTiles:frame.activeVideoTiles,
          maskGrid:frame.maskGrid,width:frame.width,height:frame.height,nativeId});
        if(window.__continuityFrames.length>200)window.__continuityFrames.shift();
      }
      original(event);
    };
  });
}
async function awaitHeldRoute(read){
  const deadline=Date.now()+5000;
  while(!read()&&Date.now()<deadline)await new Promise(resolve=>setTimeout(resolve,10));
  const route=read();assert.ok(route,'negotiated hello must request GLH4 decoder');return route;
}
async function submitLogin(page,server){
  await page.locator('#token').fill(server.token);await page.locator('#login button').click();
}
async function exactInput(page,keysym,down){
  const id=await page.evaluate(({keysym,down})=>window.gliss.send({type:'key',keysym,down},{pixelEcho:true}),{keysym,down});
  assert.ok(id>0);await page.waitForFunction(id=>window.gliss.readProbe()===(id&0xffffff),id,{timeout:5000});
  assert.deepEqual(await page.evaluate(()=>Array.from(window.gliss.ctx.getImageData(0,0,1,1).data)),[id&255,(id>>>8)&255,(id>>>16)&255,255]);
  return id;
}

test('GLH4 helper admission preserves old arguments and requires explicit valid axes',()=>{
  assert.deepEqual(hybridServerArgs({codec:'hybrid-continuity'}),[]);
  for(const grid of [32,16])assert.deepEqual(hybridServerArgs(options(grid)),
    ['--hybrid-base','global-cached','--hybrid-video-profile','camera-scene-off','--hybrid-mask-grid',String(grid)]);
  for(const codec of ['deflate','global','sparse','h264','hybrid','hybrid-grid'])assert.deepEqual(hybridServerArgs({codec}),[]);
  for(const hybridMaskGrid of [0,17,64,'16',null])assert.throws(()=>hybridServerArgs({codec:'hybrid-continuity',hybridMaskGrid}));
  for(const codec of ['deflate','h264','hybrid'])assert.throws(()=>hybridServerArgs({codec,hybridMaskGrid:16}));
  assert.throws(()=>hybridServerArgs({codec:'hybrid-continuity',hybridBase:'adaptive'}));
  assert.throws(()=>hybridServerArgs({codec:'hybrid-continuity',hybridVideoProfile:'screen-scene-off'}));
});

for(const grid of [32,16])test(`GLH4 grid${grid}: lazy negotiated composition, exact input, whole-key recovery and odd resize`,{timeout:30000},async t=>{
  const server=await startServer(options(grid));let browser,heldRoute;
  t.after(async()=>{await heldRoute?.abort().catch(()=>{});await browser?.close();await server.stop();});
  assert.equal((await fetch(`${server.url}/api/stats`)).status,401,'codec metadata remains authenticated');
  browser=await launchBrowser();const page=await browser.newPage(),errors=[],requests=[];let hello;
  page.on('pageerror',error=>errors.push(error.message));page.on('request',request=>requests.push(new URL(request.url()).pathname));
  page.on('websocket',socket=>socket.on('framereceived',({payload})=>{if(typeof payload==='string'){const value=JSON.parse(payload);if(value.type==='hello')hello=value;}}));
  await page.route('**/hybrid-continuity-decoder.js',route=>{heldRoute=route;});
  await page.goto(server.url);
  assert.equal(requests.includes('/hybrid-continuity-decoder.js'),false,'optional decoder must not load before authentication/hello');
  await observeFrames(page);await submitLogin(page,server);
  await page.waitForFunction(()=>window.gliss.codec==='hybrid-continuity',null,{timeout:5000});
  await awaitHeldRoute(()=>heldRoute);
  assert.equal(await page.evaluate(()=>window.gliss.decoder.constructor.name),'Decoder','decoder stays uninstalled while import is pending');
  assert.equal(await page.evaluate(()=>window.gliss.metrics.frames),0);
  await heldRoute.continue();heldRoute=null;
  await page.waitForFunction(grid=>window.gliss.metrics.frames>0&&window.gliss.decoder.frame?.maskGrid===grid,grid,{timeout:15000});
  assert.equal(hello?.protocol,'GLH4');assert.equal(hello?.codec,'hybrid-continuity');assert.deepEqual(hello?.hybrid_config,expectedConfig(grid));
  assert.equal(await page.evaluate(()=>window.gliss.decoder.constructor.name),'HybridContinuityDecoder');
  assert.equal(requests.includes('/hybrid-grid-decoder.js'),false);assert.equal(requests.includes('/hybrid-decoder.js'),false);
  const stats=await page.evaluate(async()=>{const response=await fetch('/api/stats');if(!response.ok)throw new Error('Authenticated stats unavailable');return response.json();});
  assert.equal(stats.codec,'hybrid-continuity');assert.deepEqual(stats.hybrid_config,expectedConfig(grid));
  await page.waitForFunction(()=>window.__continuityFrames.some(frame=>frame.activeVideoTiles>0&&!frame.videoKeyframe&&frame.videoBytes>24&&frame.nativeId!==null),null,{timeout:10000});
  const id=await exactInput(page,97,true);assert.ok(await page.evaluate(id=>window.gliss.decoder.inputId>=id,id));
  await page.evaluate(()=>window.gliss.send({type:'key',keysym:97,down:false}));
  const before=await page.evaluate(async()=>{
    // Exercise resync during decoding through the transport. Resetting the
    // decoder directly would cancel this frame and create a recovery error.
    const decoder=window.gliss.decoder,decode=decoder.decode.bind(decoder);
    await new Promise(resolve=>{decoder.decode=buffer=>{decoder.decode=decode;const pending=decode(buffer);resolve();return pending;};});
    const previous=window.__continuityFrames.at(-1);window.gliss.socket.send(JSON.stringify({type:'resync'}));
    return {sequence:previous.sequence,nativeId:previous.nativeId};
  });
  await page.waitForFunction(before=>window.__continuityFrames.some(frame=>frame.sequence>before.sequence&&frame.keyframe&&frame.videoKeyframe&&frame.activeVideoTiles>0&&frame.nativeId!==before.nativeId),before,{timeout:5000});
  assert.equal(await page.evaluate(()=>window.gliss.decoder.needsKeyframe),false);
  await page.evaluate(()=>window.gliss.send({type:'resize',width:641,height:359}));
  await page.waitForFunction(()=>window.__continuityFrames.some(frame=>frame.width===641&&frame.height===359&&frame.keyframe)&&window.gliss.canvas.width===641&&window.gliss.canvas.height===359,null,{timeout:5000});
  await exactInput(page,98,false);assert.equal(await page.evaluate(()=>window.gliss.decoder.maskGrid),grid);
  assert.equal(await page.evaluate(()=>window.gliss.metrics.errors),0);assert.deepEqual(errors,[]);
});

test('GLH4 pending lazy import cannot install after disconnect; a fresh epoch reconnects', {timeout:25000},async t=>{
  const server=await startServer(options(32));let browser,heldRoute;
  t.after(async()=>{await heldRoute?.abort().catch(()=>{});await browser?.close();await server.stop();});
  browser=await launchBrowser();const page=await browser.newPage(),errors=[];
  page.on('pageerror',error=>errors.push(error.message));
  await page.route('**/hybrid-continuity-decoder.js',route=>{heldRoute=route;});
  await page.goto(server.url);await submitLogin(page,server);
  await page.waitForFunction(()=>window.gliss.codec==='hybrid-continuity',null,{timeout:5000});
  await awaitHeldRoute(()=>heldRoute);const epoch=await page.evaluate(()=>{const epoch=window.gliss.epoch;window.gliss.disconnect();return epoch;});
  await page.waitForFunction(epoch=>window.gliss.epoch>epoch&&window.gliss.socket.readyState===WebSocket.CLOSED,epoch,{timeout:5000});
  await heldRoute.continue();heldRoute=null;
  const state=await page.evaluate(async()=>{await window.gliss.chain;return {epoch:window.gliss.epoch,decoder:window.gliss.decoder.constructor.name,frames:window.gliss.metrics.frames,errors:window.gliss.metrics.errors};});
  assert.ok(state.epoch>epoch);assert.equal(state.decoder,'Decoder');assert.equal(state.frames,0);assert.equal(state.errors,0);
  await page.unroute('**/hybrid-continuity-decoder.js');
  await page.evaluate(()=>window.gliss.connect());
  await page.waitForFunction(()=>window.gliss.metrics.frames>0&&window.gliss.decoder.constructor.name==='HybridContinuityDecoder',null,{timeout:15000});
  await exactInput(page,99,false);assert.equal(await page.evaluate(()=>window.gliss.metrics.errors),0);assert.deepEqual(errors,[]);
});
