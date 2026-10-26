import test from 'node:test';
import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {readFile,stat} from 'node:fs/promises';
import {createConnection} from 'node:net';
import {fileURLToPath} from 'node:url';
import {launchBrowser,serverBinary} from './helpers.mjs';

const SCRIPT=fileURLToPath(new URL('../test-support/browser/mock-rfb-fd.py',import.meta.url));
const EXIT_GRACE_MS=4500;

function bounded(promise,ms,label){
  let timer;
  return Promise.race([promise,new Promise((_,reject)=>{timer=setTimeout(()=>reject(new Error(label)),ms);})])
    .finally(()=>clearTimeout(timer));
}

async function ownedIdentityAbsent(pid,startTicks){
  try{
    const value=await readFile(`/proc/${pid}/stat`,'utf8');
    return Number(value.slice(value.lastIndexOf(')')+2).trim().split(/\s+/)[19])!==startTicks;
  }catch(error){if(error.code==='ENOENT'||error.code==='ESRCH')return true;throw error;}
}

async function startMockWorker({leaseMs=60000,handshake='normal',sessionTtl=60}={}){
  const binary=serverBinary();
  assert.ok((await stat(binary)).isFile(),'FD tests require a freshly built executable server');
  const args=['-u',SCRIPT,'--binary',binary,'--lease-ms',String(leaseMs),'--handshake',handshake,
    '--session-ttl',String(sessionTtl)];
  const child=spawn('python3',args,{stdio:['pipe','pipe','pipe'],
    env:{PATH:process.env.PATH,LANG:'C.UTF-8',PYTHONUNBUFFERED:'1'}});
  const events=[],replies=new Map(),waiters=[];
  let input='',commandId=0,readyResolve,readyReject,exitResolve,ready,stopping=false,terminalError;
  const readyPromise=new Promise((resolve,reject)=>{readyResolve=resolve;readyReject=reject;});
  const exitPromise=new Promise(resolve=>{exitResolve=resolve;});
  const fail=error=>{
    terminalError??=error;readyReject(error);
    for(const entry of replies.values()){clearTimeout(entry.timer);entry.reject(error);}replies.clear();
    for(const waiter of waiters.splice(0)){clearTimeout(waiter.timer);waiter.reject(error);}
  };
  child.on('error',()=>fail(new Error('FD mock process could not start')));
  child.stdin.on('error',()=>{if(!stopping)fail(new Error('FD mock private command pipe closed'));});
  child.on('exit',()=>{
    if(!stopping)fail(new Error('FD mock exited before an owned cleanup acknowledgement'));
  });
  // close follows drained stdio, including failed spawn; exit can precede the
  // final cleanup reply and is absent altogether after a spawn error.
  child.once('close',(code,signal)=>exitResolve({code,signal}));
  // Drain private native diagnostics without publishing them or IPC ready/token.
  child.stderr.on('data',()=>{});
  child.stdout.on('data',chunk=>{
    input+=chunk.toString('utf8');
    if(input.length>131072){fail(new Error('FD mock private IPC size exceeded'));return;}
    for(;;){
      const end=input.indexOf('\n');if(end<0)break;
      const line=input.slice(0,end);input=input.slice(end+1);
      let value;
      try{value=JSON.parse(line);}catch{fail(new Error('FD mock private IPC malformed'));return;}
      if(value.type==='ready'){
        if(ready){fail(new Error('FD mock issued duplicate readiness'));return;}
        ready=value;readyResolve(value);
      }else if(value.type==='event'){
        if(events.length>=512){fail(new Error('FD mock event bound exceeded'));return;}
        if(!value.event||typeof value.event.kind!=='string'||value.event.sequence!==events.length+1){
          fail(new Error('FD mock event continuity/shape invalid'));return;
        }
        events.push(value.event);
        for(const waiter of [...waiters])if(waiter.predicate(events)){
          clearTimeout(waiter.timer);waiters.splice(waiters.indexOf(waiter),1);waiter.resolve(events.slice());
        }
      }else if(value.type==='reply'){
        const entry=replies.get(value.id);
        if(!entry){fail(new Error('FD mock unexpected command reply'));return;}
        clearTimeout(entry.timer);replies.delete(value.id);entry.resolve(value);
      }else fail(new Error('FD mock private IPC message outside contract'));
    }
  });
  const request=(command,timeout=8000)=>{
    if(terminalError)return Promise.reject(terminalError);
    const id=++commandId;
    return new Promise((resolve,reject)=>{
      const timer=setTimeout(()=>{replies.delete(id);reject(new Error(`FD mock ${command} acknowledgement timed out`));},timeout);
      replies.set(id,{resolve,reject,timer});
      child.stdin.write(JSON.stringify({id,command})+'\n',error=>{
        if(error){clearTimeout(timer);replies.delete(id);reject(new Error('FD mock private command pipe closed'));}
      });
    });
  };
  const stop=async()=>{
    stopping=true;
    let reply,error;
    try{reply=await request('stop',8000);}catch(caught){error=caught;child.kill('SIGTERM');}
    child.stdin.end();
    const exited=await bounded(exitPromise,8000,'FD mock cleanup did not exit/reap its native child');
    if(ready&&!await ownedIdentityAbsent(ready.worker_pid,ready.worker_start_ticks))
      throw new Error('FD mock cleanup left the exact owned native identity alive');
    if(error)throw error;
    assert.equal(exited.code,0,'owned mock supervisor must exit normally after reaping');
    assert.equal(reply.stopped,true);
    assert.equal(reply.snapshot.worker_identity_absent,true);
    assert.equal(reply.snapshot.worker_exit_code,0,'native worker cleanup must not need SIGKILL');
    return reply.snapshot;
  };
  try{await bounded(readyPromise,10000,'FD mock HTTP readiness timed out');}
  catch(error){
    stopping=true;child.kill('SIGTERM');child.stdin.end();
    await bounded(exitPromise,8000,'FD mock failed startup cleanup did not finish');throw error;
  }
  return {url:ready.url,token:ready.token,ready,events,stop,
    snapshot:async()=>((await request('snapshot')).snapshot),
    waitFor:(predicate,timeout=5000)=>{
      if(predicate(events))return Promise.resolve(events.slice());
      if(terminalError)return Promise.reject(terminalError);
      if(waiters.length>=16)return Promise.reject(new Error('FD mock waiter bound exceeded'));
      return new Promise((resolve,reject)=>{
        const waiter={predicate,resolve,reject,timer:setTimeout(()=>{
          waiters.splice(waiters.indexOf(waiter),1);reject(new Error('FD mock expected wire event not observed'));
        },timeout)};waiters.push(waiter);
      });
    }};
}

async function fixture(t,options={}){
  let browser,server;
  const errors=[];
  t.after(async()=>{
    await browser?.close();
    if(server){
      const evidence=await server.stop();
      assert.deepEqual(evidence.errors,[]);
      t.diagnostic(JSON.stringify({fixture:'one-inherited-unix-stream',binary_sha256:server.ready.binary_sha256,
        native_pid:server.ready.worker_pid,native_start_ticks:server.ready.worker_start_ticks,
        counts:evidence.counts,rfb_closed:evidence.rfb_closed,native_exit:evidence.worker_exit_code,
        native_identity_absent:evidence.worker_identity_absent,parent_graphics_copy_closed:evidence.parent_graphics_copy_closed}));
    }
  });
  // Start Chromium first: short native lease tests measure expiry from worker
  // spawn rather than depending on browser process startup time.
  if(options.browser!==false)browser=await launchBrowser();
  server=await startMockWorker(options);
  assert.equal(server.ready.transport,'one-inherited-unix-stream');
  assert.equal(server.ready.passed_fds,1);
  assert.equal(server.ready.numeric_vnc_target,false);
  let page;
  if(browser){page=await browser.newPage();page.on('pageerror',error=>errors.push(error.message));
    await page.addInitScript(()=>{
      const NativeWebSocket=window.WebSocket;
      window.fdWire={sockets:[]};
      window.WebSocket=class extends NativeWebSocket{
        constructor(...args){
          super(...args);const entry={opened:false,closed:false,hellos:0,media:0};window.fdWire.sockets.push(entry);
          this.addEventListener('open',()=>entry.opened=true);
          this.addEventListener('close',()=>entry.closed=true);
          this.addEventListener('message',event=>{
            if(typeof event.data==='string'){if(JSON.parse(event.data).type==='hello')entry.hellos++;}
            else entry.media++;
          });
        }
      };
    });
    await page.goto(server.url);
  }
  return {server,browser,page,errors};
}

async function submitLogin(page,server){
  // Avoid locator.fill(token): failed Playwright call logs can include its
  // literal argument. Private token remains only an evaluate parameter here.
  await page.evaluate(token=>{document.getElementById('token').value=token;document.getElementById('login').requestSubmit();},server.token);
}

async function login(page,server){
  await submitLogin(page,server);
  await page.waitForFunction(()=>window.gliss.metrics.frames>0,null,{timeout:5000});
}

async function assertPixels(page,rgb){
  const evidence=await page.evaluate(rgb=>{
    const client=window.gliss,{width,height}=client.canvas,pixels=client.ctx.getImageData(0,0,width,height).data;
    let exact=width===96&&height===64;
    for(let i=0;i<pixels.length;i+=4)if(pixels[i]!==rgb[0]||pixels[i+1]!==rgb[1]||pixels[i+2]!==rgb[2]||pixels[i+3]!==255){exact=false;break;}
    return {exact,width,height,errors:client.metrics.errors};
  },rgb);
  assert.deepEqual(evidence,{exact:true,width:96,height:64,errors:0});
}

async function upgradeStatus(server,{origin=server.url,cookie}={}){
  const url=new URL(server.url),socket=createConnection({host:'127.0.0.1',port:Number(url.port)});
  const closed=new Promise(resolve=>socket.once('close',resolve));
  let buffer='';
  try{
    const response=new Promise((resolve,reject)=>{
      socket.on('error',()=>reject(new Error('owned HTTP upgrade socket failed')));
      socket.on('data',chunk=>{
        buffer+=chunk.toString('latin1');
        if(buffer.length>8192){reject(new Error('bounded upgrade response exceeded'));return;}
        if(buffer.includes('\r\n\r\n')){
          const match=/^HTTP\/1\.1 (\d{3}) /.exec(buffer);
          if(!match)reject(new Error('invalid native HTTP upgrade response'));else resolve(Number(match[1]));
        }
      });
      socket.once('connect',()=>socket.write(`GET /ws HTTP/1.1\r\nHost: ${url.host}\r\nOrigin: ${origin}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: AAECAwQFBgcICQoLDA0ODw==\r\nSec-WebSocket-Version: 13\r\n${cookie?`Cookie: ${cookie}\r\n`:''}\r\n`));
    });
    return await bounded(response,4000,'native HTTP upgrade response timed out');
  }finally{socket.destroy();await bounded(closed,1000,'owned upgrade socket did not close');}
}

async function browserDeniedUpgrade(page){
  return page.evaluate(()=>new Promise((resolve,reject)=>{
    const socket=new WebSocket(`ws://${location.host}/ws`);let opened=false;
    const timer=setTimeout(()=>{socket.close();reject(new Error('denied native WS did not close'));},3000);
    socket.onopen=()=>opened=true;socket.onerror=()=>{};
    socket.onclose=()=>{clearTimeout(timer);resolve({opened});};
  }));
}

async function cookieForPage(page){
  const cookies=await page.context().cookies();
  const selected=cookies.find(value=>value.name==='gliss_session');
  assert.equal(Boolean(selected),true,'native browser session cookie missing');
  return `gliss_session=${selected.value}`;
}

function closureEvent(events){return events.some(event=>event.kind==='rfb_closed');}
function nativeExitEvent(events){return events.some(event=>event.kind==='worker_exit');}
function expiryBudget(server){return Math.max(0,server.ready.deadline_unix_ms-Date.now())+EXIT_GRACE_MS;}

async function holdInput(page,server){
  await page.locator('#screen').focus();await page.keyboard.down('Control');
  await page.evaluate(()=>window.gliss.send({type:'pointer',x:23,y:17,buttons:1}));
  await server.waitFor(events=>events.some(e=>e.kind==='key'&&e.keysym===0xffe3&&e.down)&&
    events.some(e=>e.kind==='pointer'&&e.x===23&&e.y===17&&e.buttons===1));
}

function assertReleased(snapshot){
  assert.deepEqual(snapshot.held_keys,[]);
  assert.deepEqual(snapshot.pointer,{x:23,y:17,buttons:0});
  assert.equal(snapshot.rfb_closed,'eof');
  assert.deepEqual(snapshot.errors,[]);
  for(const [kind,predicate] of [['key',event=>event.keysym===0xffe3],['pointer',event=>event.x===23&&event.y===17]]){
    const events=snapshot.events.filter(event=>event.kind===kind&&predicate(event));
    assert.equal(events.length,2,`${kind} must have one held input and one wire release`);
    assert.equal(kind==='key'?events[0].down:events[0].buttons===1,true);
    assert.equal(kind==='key'?events[1].down:events[1].buttons!==0,false);
    assert.ok(events[1].sequence<snapshot.events.find(event=>event.kind==='rfb_closed').sequence);
  }
}

test('FD worker: denied admission preserves the source; real pixels/input, capacity, held release and one use',{timeout:40000},async t=>{
  const {server,page,errors}=await fixture(t);
  assert.equal((await fetch(`${server.url}/api/stats`)).status,401);
  assert.deepEqual(await browserDeniedUpgrade(page),{opened:false});
  assert.equal(await upgradeStatus(server),401);
  const loginRequest=(origin,token)=>fetch(`${server.url}/api/session`,{method:'POST',headers:{Origin:origin,'Content-Type':'application/json'},body:JSON.stringify({token})});
  assert.equal((await loginRequest('https://untrusted.invalid',server.token)).status,403);
  assert.equal((await loginRequest(server.url,'invalid-private-test-token')).status,401);
  const accepted=await loginRequest(server.url,server.token);assert.equal(accepted.status,200);
  const setCookie=accepted.headers.get('set-cookie');
  assert.equal(Boolean(setCookie?.includes('HttpOnly')&&setCookie.includes('SameSite=Strict')),true,'private native cookie flags missing');
  assert.equal(await upgradeStatus(server,{origin:'https://untrusted.invalid',cookie:setCookie.split(';')[0]}),401);
  assert.equal((await server.snapshot()).counts.client_versions,0,'HTTP/login/denied WS must not start RFB setup');
  await login(page,server);await assertPixels(page,[12,34,56]);
  await page.locator('#screen').focus();await page.keyboard.down('a');
  await page.waitForFunction(()=>{const p=window.gliss.ctx.getImageData(10,10,1,1).data;return p[0]===31&&p[1]===67&&p[2]===129&&p[3]===255;},null,{timeout:5000});
  await assertPixels(page,[31,67,129]);
  await server.waitFor(events=>events.some(event=>event.kind==='key'&&event.keysym===97&&event.down));
  await page.keyboard.up('a');
  // Read counters only after both independent initial and input-driven pixels.
  const stats=await page.evaluate(async()=>await (await fetch('/api/stats')).json());
  assert.ok(stats.frames>=2&&stats.bytes>0);assert.equal(stats.active_clients,1);
  assert.equal(await page.evaluate(()=>window.glissSource()),'rfb');
  const cookie=await cookieForPage(page);
  assert.deepEqual(await browserDeniedUpgrade(page),{opened:false});
  assert.equal(await upgradeStatus(server,{cookie}),503);
  assert.equal((await server.snapshot()).counts.client_versions,1,'capacity rejection must not claim another graphics source');
  await holdInput(page,server);
  await page.evaluate(()=>window.gliss.socket.close()); // No client release(): native cleanup is under test.
  await server.waitFor(closureEvent);assertReleased(await server.snapshot());
  await page.waitForFunction(async()=>{const response=await fetch('/api/stats');return response.ok&&(await response.json()).active_clients===0;},null,{timeout:5000});
  await page.evaluate(()=>{window.fdSecond=window.fdWire.sockets.length;window.gliss.connect();});
  await page.waitForFunction(()=>window.fdWire.sockets[window.fdSecond]?.closed,null,{timeout:5000});
  assert.deepEqual(await page.evaluate(()=>({hello:window.fdWire.sockets[window.fdSecond].hellos,media:window.fdWire.sockets[window.fdSecond].media})),{hello:0,media:0});
  const final=await server.snapshot();
  assert.equal(final.counts.client_versions,1);assert.equal(final.counts.client_inits,1);
  assertReleased(final);assert.deepEqual(errors,[]);
});

test('FD absolute expiry: unused source closes and the native worker exits before any login',{timeout:25000},async t=>{
  const {server,page,errors}=await fixture(t,{leaseMs:2000});
  assert.equal(await page.locator('#login').isVisible(),true);
  await server.waitFor(nativeExitEvent,expiryBudget(server));await server.waitFor(closureEvent,1000);
  const snapshot=await server.snapshot();
  assert.equal(snapshot.counts.client_versions,0);assert.equal(snapshot.counts.client_inits,0);
  assert.equal(snapshot.worker_exit_code,0);assert.equal(snapshot.worker_identity_absent,true);
  assert.ok(['eof','reset-unread-data','broken-pipe'].includes(snapshot.rfb_closed),'unused greeting may be unread at descriptor close');
  assert.deepEqual(snapshot.held_keys,[]);assert.deepEqual(snapshot.errors,[]);
  assert.equal(await page.evaluate(()=>window.gliss.metrics.frames),0);assert.deepEqual(errors,[]);
});

test('FD absolute expiry: canceled stalled RFB setup closes the socket and native child',{timeout:25000},async t=>{
  const {server,page,errors}=await fixture(t,{leaseMs:4000,handshake:'stall-security'});
  await submitLogin(page,server);
  await server.waitFor(events=>events.some(event=>event.kind==='client_version'));
  await server.waitFor(nativeExitEvent,expiryBudget(server));await server.waitFor(closureEvent,1000);
  const snapshot=await server.snapshot();
  assert.equal(snapshot.counts.client_versions,1);assert.equal(snapshot.counts.security_selections,0);
  assert.equal(snapshot.counts.client_inits,0);assert.equal(snapshot.counts.frames,0);
  assert.equal(snapshot.rfb_closed,'eof');assert.equal(snapshot.worker_exit_code,0);assert.equal(snapshot.worker_identity_absent,true);
  assert.equal(await page.evaluate(()=>window.gliss.metrics.frames),0);
  assert.equal(await page.evaluate(()=>window.fdWire.sockets.filter(value=>value.hellos).length),0);
  assert.deepEqual(snapshot.errors,[]);assert.deepEqual(errors,[]);
});

test('FD absolute expiry: active stream releases its held key/button before peer closure and reap',{timeout:25000},async t=>{
  const {server,page,errors}=await fixture(t,{leaseMs:5000});
  await login(page,server);await assertPixels(page,[12,34,56]);await holdInput(page,server);
  await server.waitFor(nativeExitEvent,expiryBudget(server));await server.waitFor(closureEvent,1000);
  const snapshot=await server.snapshot();assertReleased(snapshot);
  assert.equal(snapshot.counts.client_versions,1);assert.equal(snapshot.worker_exit_code,0);assert.equal(snapshot.worker_identity_absent,true);
  await page.waitForFunction(()=>window.gliss.socket.readyState===WebSocket.CLOSED,null,{timeout:1000});
  assert.deepEqual(errors,[]);
});

test('FD absolute expiry: incomplete HTTP body cannot retain the unused source or native process',{timeout:20000},async t=>{
  const {server}=await fixture(t,{leaseMs:2000,browser:false});
  const url=new URL(server.url),socket=createConnection({host:'127.0.0.1',port:Number(url.port)});
  const closed=new Promise(resolve=>socket.once('close',resolve));
  socket.on('error',()=>{});t.after(()=>socket.destroy());
  await bounded(new Promise((resolve,reject)=>{
    socket.once('error',()=>reject(new Error('owned partial HTTP connection failed')));
    socket.once('connect',()=>socket.write(`POST /api/session HTTP/1.1\r\nHost: ${url.host}\r\nOrigin: ${server.url}\r\nContent-Type: application/json\r\nContent-Length: 4096\r\nConnection: keep-alive\r\n\r\n{"token":"`,error=>error?reject(new Error('owned partial HTTP write failed')):resolve()));
  }),1000,'owned partial HTTP request did not connect');
  assert.equal((await server.snapshot()).counts.client_versions,0);
  await server.waitFor(nativeExitEvent,expiryBudget(server));await bounded(closed,1000,'expired worker left an HTTP connection alive');
  await server.waitFor(closureEvent,1000);
  const snapshot=await server.snapshot();assert.equal(snapshot.counts.client_versions,0);
  assert.equal(snapshot.worker_exit_code,0);assert.equal(snapshot.worker_identity_absent,true);assert.deepEqual(snapshot.errors,[]);
});

test('FD cookie expiry: earlier authorization deadline closes one source while its longer worker lease remains',{timeout:25000},async t=>{
  const {server,page,errors}=await fixture(t,{sessionTtl:3});
  await login(page,server);await assertPixels(page,[12,34,56]);await holdInput(page,server);
  await server.waitFor(closureEvent,5000);const snapshot=await server.snapshot();assertReleased(snapshot);
  assert.equal(snapshot.worker_exit_code,null,'source authorization expiry must precede the 60s native lease');
  assert.equal(snapshot.worker_identity_absent,false);
  assert.equal((await page.request.get(`${server.url}/api/stats`)).status(),401);
  assert.deepEqual(errors,[]);
});
