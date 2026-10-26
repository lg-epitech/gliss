import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { launchBrowser,startServer,loginPage } from './helpers.mjs';
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

test('browser: lossless pixel echo, keyboard release and bounded delta history',async t=>{
  const server=await startServer({codec:'adaptive'});t.after(server.stop);
  const browser=await launchBrowser();t.after(()=>browser.close());
  const page=await browser.newPage();const errors=[];page.on('pageerror',e=>errors.push(e.message));
  await loginPage(page,server);
  const id=await page.evaluate(()=>window.gliss.send({type:'key',keysym:97,down:true},{pixelEcho:true}));
  await page.waitForFunction(id=>{const p=document.getElementById('screen').getContext('2d').getImageData(0,0,1,1).data;return(p[0]|p[1]<<8|p[2]<<16)===id;},id);
  const result=await page.evaluate(()=>({metrics:window.gliss.metrics,pixel:Array.from(window.gliss.ctx.getImageData(0,0,1,1).data)}));
  assert.equal(result.metrics.errors,0);assert.equal(result.metrics.pixelEchoLatencyMs.length,1);assert.ok(result.metrics.pixelEchoLatencyMs[0]>0);assert.equal(result.pixel[3],255);
  await page.locator('#screen').focus();await page.keyboard.down('Control');
  assert.equal(await page.evaluate(()=>window.gliss.keys.size),1);
  await page.locator('#clipboard').focus();
  assert.equal(await page.evaluate(()=>window.gliss.keys.size),0);
  await page.evaluate(()=>window.gliss.send({type:'resize',width:641,height:359}));
  await page.waitForFunction(()=>document.getElementById('screen').width===641&&document.getElementById('screen').height===359);
  const echoAfterBlur=await page.evaluate(()=>{window.gliss.point={x:0,y:0};window.gliss.release();return window.gliss.send({type:'key',keysym:98,down:false},{pixelEcho:true});});
  await page.waitForFunction(id=>{const p=window.gliss.ctx.getImageData(0,0,1,1).data;return(p[0]|p[1]<<8|p[2]<<16)===id;},echoAfterBlur);
  assert.deepEqual(errors,[]);
});

test('codec: Rust packets decode to identical browser pixels, malformed delta is atomic',async t=>{
  const fixtures=JSON.parse(await readFile(new URL('../test-support/rust/codec/fixtures.json',import.meta.url),'utf8'));
  const server=await startServer();t.after(server.stop);
  const browser=await launchBrowser();t.after(()=>browser.close());
  const page=await browser.newPage();await page.goto(server.url);
  const result=await page.evaluate(async fixtures=>{
    const {Decoder}=await import('/decoder.js');
    const fromHex=hex=>Uint8Array.from(hex.match(/../g)||[],s=>parseInt(s,16));
    let count=0;const decoderByMode=new Map(),surfaceByMode=new Map();
    for(const record of fixtures){
      let decoder=decoderByMode.get(record.mode);if(!decoder){decoder=new Decoder();decoderByMode.set(record.mode,decoder);}
      const packet=fromHex(record.packet_hex);const frame=await decoder.decode(packet.buffer);
      const expected=fromHex(record.expected_pixels_hex);
      if(frame.width!==record.width||frame.height!==record.height||frame.pixels.length!==expected.length)throw new Error(`Shape mismatch ${record.mode}`);
      for(let i=0;i<expected.length;i++)if(frame.pixels[i]!==expected[i])throw new Error(`Pixel mismatch ${record.mode}/${record.sequence}/${i}`);
      let surface=surfaceByMode.get(record.mode);if(!surface){surface=document.createElement('canvas');surfaceByMode.set(record.mode,surface);}
      if(surface.width!==frame.width||surface.height!==frame.height){surface.width=frame.width;surface.height=frame.height;}
      const ctx=surface.getContext('2d',{alpha:false,willReadFrequently:true}),area=frame.damage;
      if(area)ctx.putImageData(new ImageData(frame.pixels,frame.width,frame.height),0,0,area.x,area.y,area.width,area.height);
      const painted=ctx.getImageData(0,0,frame.width,frame.height).data;
      for(let i=0;i<expected.length;i++)if(painted[i]!==expected[i])throw new Error(`Damage paint mismatch ${record.mode}/${record.sequence}/${i}`);
      count++;
    }
    const decoder=new Decoder();const good=fromHex(fixtures[0].packet_hex);await decoder.decode(good.buffer);const saved=decoder.frame;
    let rejected=0;
    for(const packet of [good.slice(0,10),good.slice(0,-1),new Uint8Array([...good,0])]){try{await decoder.decode(packet.buffer);}catch{rejected++;}if(decoder.frame!==saved)throw new Error('Failed decode mutated reference');}
    const flags=good.slice();flags[18]=255;try{await decoder.decode(flags.buffer);}catch{rejected++;}
    return{count,rejected};
  },fixtures);
  assert.equal(result.count,fixtures.length);assert.equal(result.rejected,4);
});

test('full RFB pipeline: captured pixels, input bytes, clipboard and abrupt-close key release',async t=>{
  const rfb=await startRfb();t.after(rfb.stop);
  const server=await startServer({vnc:rfb.address});t.after(server.stop);
  const browser=await launchBrowser();t.after(()=>browser.close());const page=await browser.newPage();
  await loginPage(page,server);
  assert.deepEqual(await page.evaluate(()=>Array.from(window.gliss.ctx.getImageData(10,10,1,1).data)),[12,34,56,255]);
  await page.locator('#screen').focus();await page.keyboard.down('a');
  await page.waitForFunction(()=>{const p=window.gliss.ctx.getImageData(10,10,1,1).data;return p[0]===31&&p[1]===67&&p[2]===129;});
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

for(const codec of ['batch','h264'])test(`stream ${codec}: actual display probe, independent resync and resize`,async t=>{
  const server=await startServer({codec,workload:'scroll'});t.after(server.stop);const browser=await launchBrowser();t.after(()=>browser.close());const page=await browser.newPage();const errors=[];page.on('pageerror',error=>errors.push(error.message));await loginPage(page,server);
  const id=await page.evaluate(()=>window.gliss.send({type:'key',keysym:98,down:true},{pixelEcho:true}));
  await page.waitForFunction(id=>window.gliss.readProbe()===(id&0xffffff),id,{timeout:5000});
  if(codec==='h264'){
    await page.evaluate(()=>window.gliss.send({type:'resize',width:4096,height:600}));
    await page.waitForFunction(()=>document.getElementById('status').textContent.includes('3840'));
    assert.equal(await page.evaluate(()=>window.gliss.canvas.width),960);
  }
  await page.evaluate(()=>{window.gliss.decoder.reset();window.gliss.socket.send(JSON.stringify({type:'resync'}));window.gliss.send({type:'resize',width:642,height:360});});
  await page.waitForFunction(()=>window.gliss.canvas.width===642&&window.gliss.canvas.height===360,null,{timeout:5000});
  assert.ok(await page.evaluate(()=>window.gliss.metrics.pixelEchoLatencyMs.length>=1));
  assert.deepEqual(errors,[]);
});
