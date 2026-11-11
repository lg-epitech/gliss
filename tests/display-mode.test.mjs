import test from 'node:test';
import assert from 'node:assert/strict';
import { launchBrowser,startServer,loginPage } from './helpers.mjs';
import { startRfb } from './mock-rfb.mjs';

test('display mode: a size H.264 cannot encode pauses the picture, keeps input working and resumes by itself',{timeout:60000},async t=>{
  const rfb=await startRfb();t.after(rfb.stop);
  const server=await startServer({vnc:rfb.address});t.after(server.stop);
  const browser=await launchBrowser();t.after(()=>browser.close());
  const page=await browser.newPage();const errors=[];page.on('pageerror',error=>errors.push(error.message));
  await loginPage(page,server);
  await page.evaluate(()=>{window.events=[];const original=window.gliss.onEvent;window.gliss.onEvent=event=>{if(event.type==='error'||event.type==='close')window.events.push(event);original(event);};});
  // An odd width: nothing to crop or pad, so the picture stops and the page says why.
  rfb.resize(97,64);
  await page.waitForFunction(()=>document.getElementById('status').textContent.startsWith('The VM display is 97x64.'),null,{timeout:15000});
  // The session is open and input reaches the VM, which is how a user would change the mode.
  await page.evaluate(()=>{window.gliss.send({type:'key',keysym:98,down:true});window.gliss.send({type:'pointer',x:5,y:6,buttons:0});});
  await rfb.waitFor(inputs=>inputs.some(input=>input.type==='key'&&input.keysym===98&&input.down)&&inputs.some(input=>input.type==='pointer'&&input.x===5&&input.y===6));
  const paused=await page.evaluate(()=>({open:window.gliss.socket.readyState===WebSocket.OPEN,width:window.gliss.canvas.width,events:window.events}));
  assert.equal(paused.open,true);assert.equal(paused.width,96,'the last picture stays until a new one can be shown');
  assert.equal(paused.events.length,1,'one event for one mode change');assert.equal(paused.events[0].type,'error');
  // A size that fits: the stream continues without the page doing anything.
  rfb.resize(98,66);
  await page.waitForFunction(()=>window.gliss.canvas.width===98&&window.gliss.canvas.height===66,null,{timeout:15000});
  await page.waitForFunction(()=>document.getElementById('status').textContent==='Connected',null,{timeout:15000});
  assert.equal(await page.evaluate(()=>window.events.length),1);
  assert.deepEqual(errors,[]);
});
