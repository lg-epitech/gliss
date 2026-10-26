import test from 'node:test';
import assert from 'node:assert/strict';
import { launchBrowser,startServer } from './helpers.mjs';

// Another web app imports client.js as a module, without the standalone page.
test('embedded client: module import, explicit URL, close event and dispose',async t=>{
  const server=await startServer({codec:'batch'});t.after(server.stop);
  const browser=await launchBrowser();t.after(()=>browser.close());
  const page=await browser.newPage();const errors=[];page.on('pageerror',e=>errors.push(e.message));
  await page.goto(`${server.url}/health`);
  const width=await page.evaluate(async token=>{
    const login=await fetch('/api/session',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({token})});
    if(!login.ok)throw new Error(`login ${login.status}`);
    const {GlissClient}=await import('/client.js');
    const canvas=document.createElement('canvas');document.body.append(canvas);
    window.events=[];
    window.embedded=new GlissClient(canvas,()=>{},event=>window.events.push(event),{url:`ws://${location.host}/ws`});
    window.embedded.connect();
    while(!window.events.some(event=>event.type==='frame'))await new Promise(resolve=>setTimeout(resolve,20));
    return canvas.width;
  },server.token);
  assert.ok(width>0);
  assert.equal(await page.evaluate(()=>window.events.find(event=>event.type==='hello')?.codec),'batch');
  await server.stop();
  await page.waitForFunction(()=>window.events.some(event=>event.type==='close'));
  const after=await page.evaluate(()=>{
    window.embedded.dispose();
    const sent=window.embedded.send({type:'key',keysym:97,down:true});
    window.dispatchEvent(new Event('blur'));
    return {sent,code:window.events.find(event=>event.type==='close').code};
  });
  assert.equal(after.sent,0);assert.equal(typeof after.code,'number');
  assert.deepEqual(errors,[]);
});
