import test from 'node:test';
import assert from 'node:assert/strict';
import {launchBrowser,startServer,loginPage} from './helpers.mjs';
import {startRfb} from './mock-rfb.mjs';

test('viewers without held buttons cannot move or release another controller pointer on disconnect', {timeout:20000}, async t=>{
  const peer=await startRfb();t.after(peer.stop);
  const server=await startServer({vnc:peer.address,maxClients:3});t.after(server.stop);
  const browser=await launchBrowser();t.after(()=>browser.close());
  const contexts=[];
  for(let index=0;index<3;index++){
    const context=await browser.newContext();contexts.push(context);
    const page=await context.newPage();await loginPage(page,server);
  }
  const [controller,graceful,abrupt]=contexts.map(context=>context.pages()[0]);
  // Bypass client bookkeeping deliberately: only the native controller owns
  // this held button, so observer focus events cannot release it locally.
  await controller.evaluate(()=>window.gliss.socket.send(JSON.stringify({type:'pointer',x:40,y:24,buttons:1,id:5000})));
  await peer.waitFor(inputs=>inputs.some(input=>input.type==='pointer'&&input.x===40&&input.y===24&&input.buttons===1));
  const before=peer.inputs.filter(input=>input.type==='pointer').length;
  await graceful.evaluate(()=>window.gliss.disconnect());
  await abrupt.evaluate(()=>window.gliss.socket.close());
  await controller.waitForFunction(async()=>{
    const response=await fetch('/api/stats');return response.ok&&(await response.json()).active_clients===1;
  });
  // Drain loopback observations after native session cleanup; no timing claim.
  await controller.waitForTimeout(100);
  assert.deepEqual(peer.inputs.filter(input=>input.type==='pointer').slice(before),[]);
  await controller.evaluate(()=>window.gliss.socket.close());
  await peer.waitFor(inputs=>inputs.some(input=>input.type==='pointer'&&input.x===40&&input.y===24&&input.buttons===0));
});

test('client wheel pulses clear transient bits and direct pointer release keeps the supplied coordinates', {timeout:15000}, async t=>{
  const peer=await startRfb();t.after(peer.stop);
  const server=await startServer({vnc:peer.address});t.after(server.stop);
  const browser=await launchBrowser();t.after(()=>browser.close());
  const page=await browser.newPage();await loginPage(page,server);
  await page.evaluate(()=>{
    const canvas=window.gliss.canvas,rect=canvas.getBoundingClientRect();
    canvas.dispatchEvent(new WheelEvent('wheel',{clientX:rect.left+10,clientY:rect.top+11,deltaY:1,bubbles:true,cancelable:true}));
  });
  await peer.waitFor(inputs=>{
    const pointers=inputs.filter(input=>input.type==='pointer');
    return pointers.length>=2&&pointers.at(-2).buttons===16&&pointers.at(-1).buttons===0;
  });
  assert.equal(await page.evaluate(()=>window.gliss.buttons),0);
  await page.evaluate(()=>{window.gliss.send({type:'pointer',x:12,y:13,buttons:1});window.gliss.release();});
  await peer.waitFor(inputs=>inputs.some(input=>input.type==='pointer'&&input.x===12&&input.y===13&&input.buttons===1)&&
    inputs.some(input=>input.type==='pointer'&&input.x===12&&input.y===13&&input.buttons===0));
});
