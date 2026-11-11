import test from 'node:test';
import assert from 'node:assert/strict';
import { launchBrowser,startServer,loginPage,assertColour,COLOUR_TOLERANCE } from './helpers.mjs';
import { startRfb } from './mock-rfb.mjs';

test('authorization: origin, secret, cookie and display upgrade boundaries',async t=>{
  const server=await startServer();t.after(server.stop);
  assert.equal((await fetch(`${server.url}/api/stats`)).status,401);
  const login=async(origin,token)=>fetch(`${server.url}/api/session`,{method:'POST',headers:{Origin:origin,'Content-Type':'application/json'},body:JSON.stringify({token})});
  assert.equal((await login('https://untrusted.invalid',server.token)).status,403);
  assert.equal((await login(server.url,'wrong')).status,401);
  const accepted=await login(server.url,server.token);assert.equal(accepted.status,200);
  const cookie=accepted.headers.get('set-cookie');assert.match(cookie,/HttpOnly/);assert.match(cookie,/SameSite=Strict/);
  const session=cookie.split(';')[0];
  assert.equal((await fetch(`${server.url}/api/stats`,{headers:{Cookie:session}})).status,200);
  assert.equal((await fetch(`${server.url}/api/stats`,{headers:{Cookie:session.slice(0,-4)+'ffff'}})).status,401);
  const browser=await launchBrowser();t.after(()=>browser.close());
  const page=await browser.newPage();await page.goto(server.url);
  const rejected=await page.evaluate(()=>new Promise(resolve=>{const ws=new WebSocket(`ws://${location.host}/ws`);ws.onopen=()=>resolve(false);ws.onerror=()=>resolve(true);}));
  assert.equal(rejected,true);
});

test('browser: pixel echo, keyboard release and resize',async t=>{
  const server=await startServer();t.after(server.stop);
  const browser=await launchBrowser();t.after(()=>browser.close());
  const page=await browser.newPage();const errors=[];page.on('pageerror',e=>errors.push(e.message));
  await loginPage(page,server);
  const id=await page.evaluate(()=>window.gliss.send({type:'key',keysym:97,down:true},{pixelEcho:true}));
  await page.waitForFunction(id=>window.gliss.readProbe()===(id&0xffffff),id);
  const result=await page.evaluate(()=>({metrics:window.gliss.metrics,pixel:Array.from(window.gliss.ctx.getImageData(0,0,1,1).data)}));
  assert.equal(result.metrics.errors,0);assert.equal(result.metrics.pixelEchoLatencyMs.length,1);assert.ok(result.metrics.pixelEchoLatencyMs[0]>0);assert.equal(result.pixel[3],255);
  await page.locator('#screen').focus();await page.keyboard.down('Control');
  assert.equal(await page.evaluate(()=>window.gliss.keys.size),1);
  await page.locator('#clipboard').focus();
  assert.equal(await page.evaluate(()=>window.gliss.keys.size),0);
  await page.evaluate(()=>window.gliss.send({type:'resize',width:642,height:360}));
  await page.waitForFunction(()=>document.getElementById('screen').width===642&&document.getElementById('screen').height===360);
  const echoAfterBlur=await page.evaluate(()=>{window.gliss.point={x:0,y:0};window.gliss.release();return window.gliss.send({type:'key',keysym:98,down:false},{pixelEcho:true});});
  await page.waitForFunction(id=>window.gliss.readProbe()===(id&0xffffff),echoAfterBlur);
  assert.deepEqual(errors,[]);
});

test('full RFB pipeline: captured pixels, input bytes, clipboard and abrupt-close key release',async t=>{
  const rfb=await startRfb();t.after(rfb.stop);
  const server=await startServer({vnc:rfb.address});t.after(server.stop);
  const browser=await launchBrowser();t.after(()=>browser.close());const page=await browser.newPage();
  await loginPage(page,server);
  assertColour(await page.evaluate(()=>Array.from(window.gliss.ctx.getImageData(10,10,1,1).data)),[12,34,56],'Captured colour');
  await page.locator('#screen').focus();await page.keyboard.down('a');
  await page.waitForFunction(tolerance=>{const p=window.gliss.ctx.getImageData(10,10,1,1).data;return [31,67,129].every((value,channel)=>Math.abs(p[channel]-value)<=tolerance);},COLOUR_TOLERANCE);
  await rfb.waitFor(inputs=>inputs.some(i=>i.type==='key'&&i.keysym===97&&i.down));
  await page.evaluate(()=>window.gliss.send({type:'clipboard',text:'caf\u00e9'}));
  await rfb.waitFor(inputs=>inputs.some(i=>i.type==='clipboard'&&i.text==='caf\u00e9'));
  await page.evaluate(()=>{
    const canvas=window.gliss.canvas;
    for(const [type,key,code,repeat] of [['keydown','Shift','ShiftLeft',false],['keydown','A','KeyA',true],['keyup','A','KeyA',false],['keyup','Shift','ShiftLeft',false]])canvas.dispatchEvent(new KeyboardEvent(type,{key,code,repeat,bubbles:true,cancelable:true}));
  });
  await rfb.waitFor(inputs=>inputs.some(i=>i.type==='key'&&i.keysym===97&&!i.down));
  await page.keyboard.down('a');
  const releases=rfb.inputs.filter(i=>i.type==='key'&&i.keysym===97&&!i.down).length;
  // Skip client release(): exercise the server cleanup on an abrupt socket close.
  await page.evaluate(()=>window.gliss.socket.close());
  await rfb.waitFor(inputs=>inputs.filter(i=>i.type==='key'&&i.keysym===97&&!i.down).length>releases);
});

test('reconnect: stale socket callbacks cannot clear the active session keys',async t=>{
  const rfb=await startRfb();t.after(rfb.stop);const server=await startServer({vnc:rfb.address});t.after(server.stop);
  const browser=await launchBrowser();t.after(()=>browser.close());const page=await browser.newPage();await loginPage(page,server);
  await page.evaluate(()=>{window.previousSocket=window.gliss.socket;window.gliss.connect();});
  await page.waitForFunction(()=>window.gliss.socket.readyState===WebSocket.OPEN);
  await page.locator('#screen').focus();await page.keyboard.down('Control');
  await page.evaluate(()=>window.previousSocket.onclose(new Event('close')));
  assert.equal(await page.evaluate(()=>window.gliss.keys.size),1);
  await page.keyboard.up('Control');
  await rfb.waitFor(inputs=>inputs.some(i=>i.type==='key'&&i.keysym===0xffe3&&!i.down));
});

test('expired authorization closes an already connected display stream',async t=>{
  const server=await startServer({sessionTtl:2});t.after(server.stop);const browser=await launchBrowser();t.after(()=>browser.close());const page=await browser.newPage();await loginPage(page,server);
  await page.waitForFunction(()=>window.gliss.socket.readyState===WebSocket.CLOSED,null,{timeout:4000});
  assert.equal((await page.request.get(`${server.url}/api/stats`)).status(),401);
});

test('stream: actual display probe, rejected resize, independent resync and resize',async t=>{
  const server=await startServer({workload:'scroll'});t.after(server.stop);const browser=await launchBrowser();t.after(()=>browser.close());const page=await browser.newPage();const errors=[];page.on('pageerror',error=>errors.push(error.message));await loginPage(page,server);
  const id=await page.evaluate(()=>window.gliss.send({type:'key',keysym:98,down:true},{pixelEcho:true}));
  await page.waitForFunction(id=>window.gliss.readProbe()===(id&0xffffff),id,{timeout:5000});
  await page.evaluate(()=>window.gliss.send({type:'resize',width:4096,height:600}));
  await page.waitForFunction(()=>document.getElementById('status').textContent.includes('3840'));
  assert.equal(await page.evaluate(()=>window.gliss.canvas.width),960);
  await page.evaluate(()=>{window.gliss.decoder.reset();window.gliss.socket.send(JSON.stringify({type:'resync'}));window.gliss.send({type:'resize',width:642,height:360});});
  await page.waitForFunction(()=>window.gliss.canvas.width===642&&window.gliss.canvas.height===360,null,{timeout:5000});
  assert.ok(await page.evaluate(()=>window.gliss.metrics.pixelEchoLatencyMs.length>=1));
  assert.deepEqual(errors,[]);
});

for(const [title,name,expected] of [
  ['Standard','fixture','browser: Ελληνικά\r\n🚀\0'],
  ['QEMU','QEMU (fixture)','browser: Ελληνικά\n🚀'],
]){
  test(`${title} extended clipboard: guest events and browser input preserve Unicode`,async t=>{
    const rfb=await startRfb({extendedClipboard:true,name});t.after(rfb.stop);
    const server=await startServer({vnc:rfb.address});t.after(server.stop);
    const browser=await launchBrowser();t.after(()=>browser.close());
    const page=await browser.newPage();await loginPage(page,server);
    await rfb.waitFor(inputs=>inputs.some(i=>i.type==='clipboard-caps'));
    rfb.sendClipboard('guest: 日本語\r\n🙂');
    await page.waitForFunction(()=>document.getElementById('cliptext').value==='guest: 日本語\n🙂');
    await page.evaluate(()=>window.gliss.send({type:'clipboard',text:'browser: Ελληνικά\n🚀'}));
    await rfb.waitFor(inputs=>inputs.some(i=>i.type==='clipboard'&&i.text===expected));
  });
}
