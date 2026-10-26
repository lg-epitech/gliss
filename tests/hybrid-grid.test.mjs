import test from 'node:test';
import assert from 'node:assert/strict';
import {launchBrowser,startServer,loginPage} from './helpers.mjs';

for(const grid of [32,16])test(`GLH2 grid${grid}: negotiated composition, exact input, recovery and odd resize`,{timeout:30000},async t=>{
  const server=await startServer({codec:'hybrid-grid',hybridMaskGrid:grid,hybridBase:'global-cached',
    hybridVideoProfile:'camera-scene-off',workload:'video',fps:15,h264Bitrate:3000000});
  let browser;t.after(async()=>{await browser?.close();await server.stop();});
  browser=await launchBrowser();const page=await browser.newPage(),errors=[];let hello;
  page.on('pageerror',error=>errors.push(error.message));
  page.on('websocket',socket=>socket.on('framereceived',({payload})=>{
    if(typeof payload==='string'){const value=JSON.parse(payload);if(value.type==='hello')hello=value;}
  }));
  await loginPage(page,server);
  assert.equal(hello?.protocol,'GLH2');assert.equal(hello?.codec,'hybrid-grid');
  assert.deepEqual(hello?.hybrid_config,{base_mode:'global-cached',video_profile:'camera-scene-off',
    video_usage:'camera-video-real-time',scene_change_detection:false,mask_grid:grid,base_tile:64});
  await page.waitForFunction(grid=>window.gliss.decoder.frame?.maskGrid===grid&&window.gliss.decoder.frame?.activeVideoTiles>0,grid,{timeout:10000});
  const id=await page.evaluate(()=>window.gliss.send({type:'key',keysym:97,down:true},{pixelEcho:true}));
  await page.waitForFunction(id=>window.gliss.readProbe()===(id&0xffffff),id,{timeout:5000});
  assert.deepEqual(await page.evaluate(()=>Array.from(window.gliss.ctx.getImageData(0,0,1,1).data)),[id&255,(id>>>8)&255,(id>>>16)&255,255]);
  await page.evaluate(()=>window.gliss.send({type:'key',keysym:97,down:false}));
  const before=await page.evaluate(()=>{
    const previous=window.gliss.decoder.sequence;window.gliss.decoder.reset();
    window.gliss.socket.send(JSON.stringify({type:'resync'}));return previous;
  });
  await page.waitForFunction(previous=>window.gliss.decoder.sequence>previous&&!window.gliss.decoder.needsKeyframe,before,{timeout:5000});
  await page.evaluate(()=>window.gliss.send({type:'resize',width:641,height:359}));
  await page.waitForFunction(()=>window.gliss.canvas.width===641&&window.gliss.canvas.height===359,null,{timeout:5000});
  const second=await page.evaluate(()=>window.gliss.send({type:'key',keysym:98,down:false},{pixelEcho:true}));
  await page.waitForFunction(id=>window.gliss.readProbe()===(id&0xffffff),second,{timeout:5000});
  assert.equal(await page.evaluate(()=>window.gliss.decoder.maskGrid),grid);
  assert.deepEqual(await page.evaluate(()=>Array.from(window.gliss.ctx.getImageData(0,0,1,1).data)),[second&255,(second>>>8)&255,(second>>>16)&255,255]);
  assert.deepEqual(errors,[]);
});
