import test from 'node:test';
import assert from 'node:assert/strict';
import {launchBrowser,startServer,loginPage} from './helpers.mjs';

for(const codec of ['hybrid','global-cached'])test(`experimental ${codec}: real server stream, input echo, recovery and odd resize`,{timeout:30000},async t=>{
  const server=await startServer({codec,workload:'video',fps:15,inFlightFrames:4});t.after(server.stop);
  const browser=await launchBrowser();t.after(()=>browser.close());
  const page=await browser.newPage();const errors=[];page.on('pageerror',error=>errors.push(error.message));
  await loginPage(page,server);
  assert.equal(await page.evaluate(()=>window.gliss.codec),codec);
  if(codec==='hybrid')await page.waitForFunction(()=>window.gliss.decoder.frame?.activeVideoTiles>0,null,{timeout:10000});
  const id=await page.evaluate(()=>window.gliss.send({type:'key',keysym:97,down:true},{pixelEcho:true}));
  await page.waitForFunction(id=>window.gliss.readProbe()===(id&0xffffff),id,{timeout:5000});
  await page.evaluate(()=>{
    window.gliss.send({type:'key',keysym:97,down:false});
    window.gliss.decoder.reset();
    window.gliss.socket.send(JSON.stringify({type:'resync'}));
    window.gliss.send({type:'resize',width:641,height:359});
  });
  await page.waitForFunction(()=>window.gliss.canvas.width===641&&window.gliss.canvas.height===359,null,{timeout:5000});
  const second=await page.evaluate(()=>window.gliss.send({type:'key',keysym:98,down:false},{pixelEcho:true}));
  await page.waitForFunction(id=>window.gliss.readProbe()===(id&0xffffff),second,{timeout:5000});
  // The reserved exact UI probe sits outside the generated moving imagery.
  assert.deepEqual(await page.evaluate(()=>Array.from(window.gliss.ctx.getImageData(0,0,1,1).data)),
    [second&255,(second>>>8)&255,(second>>>16)&255,255]);
  assert.deepEqual(errors,[]);
});
