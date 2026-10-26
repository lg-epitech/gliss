// Actual HTTP/WS/native RFB/browser pipeline. No VM, Distribox or GPU.
import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { mkdtemp, readFile, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { launchBrowser, freePort, CPU_BROWSER_ARGS } from '../../../../tests/helpers.mjs';
import { startRfb } from '../../../../tests/mock-rfb.mjs';

const A = '00000000-0000-4000-8000-000000000001';
const B = '00000000-0000-4000-8000-000000000002';
const USER = '00000000-0000-4000-8000-000000000010';
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));

async function eventually(predicate, timeout=8000) {
  const deadline=Date.now()+timeout;
  while(Date.now()<deadline){if(await predicate())return;await sleep(20);}
  throw new Error('Gateway assertion deadline exceeded');
}

test('mock-scoped gateway paints native frames and isolates cookies/input/worker lifecycle',
  {timeout: 90000, skip: !process.env.GLISS_BINARY}, async t => {
    const binary=resolve(process.env.GLISS_BINARY);
    assert.equal(binary,process.env.GLISS_BINARY,'GLISS_BINARY must be absolute');
    const temporary=await mkdtemp(join(tmpdir(),'gliss-gateway-'));
    const a=await startRfb(), b=await startRfb();
    const port=await freePort(), origin=`http://127.0.0.1:${port}`;
    const control=randomBytes(24).toString('base64url');
    const fixture=join(temporary,'owned-mock.json');
    await writeFile(fixture,JSON.stringify({mode:'owned-mock',vms:[
      {vm_id:A,node:'master',port:Number(a.address.split(':').at(-1)),credential:'mock-secret-a'},
      {vm_id:B,node:'node_a',port:Number(b.address.split(':').at(-1)),credential:'mock-secret-b'}],
      users:[{id:USER,token:'mock-dashboard-token',policies:['vms:connect']}]}));
    const child=spawn(process.env.GLISS_PROTOTYPE_PYTHON||'python3',[
      '-m','integration.distribox.prototype.gateway','--worker-binary',binary,'--fixtures',fixture,
      '--port',String(port),'--session-ttl','25','--ready-ttl','8','--reauthorize-interval','0.2'],
      {cwd:resolve('.'),env:{PATH:process.env.PATH,LANG:process.env.LANG||'C.UTF-8',
        GLISS_PROTOTYPE_TEST_CONTROL:control},stdio:['ignore','pipe','pipe']});
    let logs='',spawnError;
    child.once('error',error=>spawnError=error);
    for(const stream of [child.stdout,child.stderr])stream.on('data',chunk=>logs=(logs+chunk.toString()).slice(-8192));
    const stop=async()=>{
      if(child.exitCode!==null||child.signalCode!==null||!child.pid)return;
      const exited=new Promise(resolve=>child.once('exit',resolve));
      child.kill('SIGINT');
      let timer;
      await Promise.race([exited,new Promise(resolve=>{timer=setTimeout(resolve,6000);})]);
      clearTimeout(timer);
      if(child.exitCode===null&&child.signalCode===null){child.kill('SIGKILL');await exited;}
    };
    let browser,peersStopped=false;
    t.after(async()=>{await browser?.close();await stop();if(!peersStopped){await a.stop();await b.stop();}await rm(temporary,{recursive:true,force:true});});
    await eventually(async()=>{
      if(spawnError||child.exitCode!==null)throw spawnError||new Error(`Gateway exited: ${logs}`);
      try{return(await fetch(`${origin}/health`)).ok;}catch{return false;}
    });
    const state=async()=>{
      const response=await fetch(`${origin}/__test__/state`,{headers:{'X-Gliss-Test-Control':control}});
      assert.equal(response.status,200);return response.json();
    };
    browser=await launchBrowser();
    const context=await browser.newContext();
    const issue=async(body,bearer)=>{
      const response=await context.request.post(`${origin}/gliss/session`,{data:body,
        headers:{Origin:origin,...(bearer?{Authorization:`Bearer ${bearer}`}:{})}});
      assert.equal(response.status(),200);return response.json();
    };
    const exchange=async offer=>{
      const response=await context.request.post(origin+offer.exchange_path,{data:{ticket:offer.ticket},headers:{Origin:origin}});
      assert.equal(response.status(),200);return response.json();
    };
    const offerA=await issue({credential:'mock-secret-a'});
    const scopeA=await exchange(offerA);
    const pageA=await context.newPage();
    await pageA.goto(origin+scopeA.view_path);
    await pageA.waitForFunction(()=>window.glissPrototype?.metrics.frames>=1);
    assert.deepEqual(await pageA.evaluate(()=>window.glissPrototype.pixel()),[12,34,56,255]);
    const cookies=await context.cookies(origin+scopeA.view_path);
    const cookieA=cookies.find(cookie=>cookie.name==='gliss_stream');
    assert.ok(cookieA.httpOnly);assert.equal(cookieA.sameSite,'Strict');assert.equal(cookieA.path,scopeA.view_path);
    assert.ok(!cookies.some(cookie=>cookie.name==='gliss_session'));
    // Existing fixture B has the same initial pixels; change only B and prove
    // A's pixels/inputs stay independent in one shared browser cookie context.
    const scopeB=await exchange(await issue({vm_id:B},'mock-dashboard-token'));
    const pageB=await context.newPage();
    await pageB.goto(origin+scopeB.view_path);
    await pageB.waitForFunction(()=>window.glissPrototype?.metrics.frames>=1);
    const aInputBefore=a.inputs.filter(input=>input.type==='key').length;
    await pageB.locator('#screen').focus();await pageB.keyboard.press('b');
    await pageB.waitForFunction(()=>window.glissPrototype.pixel().join(',')==='31,67,129,255');
    assert.deepEqual(await pageA.evaluate(()=>window.glissPrototype.pixel()),[12,34,56,255]);
    assert.equal(a.inputs.filter(input=>input.type==='key').length,aInputBefore);
    assert.ok(b.inputs.some(input=>input.type==='key'&&input.keysym===98&&input.down));
    const sharedCookies=await context.cookies();
    assert.equal(sharedCookies.filter(cookie=>cookie.name==='gliss_stream').length,2);
    assert.equal((await state()).peak_active,2);assert.equal((await state()).peak_launching,1);
    const replay=await context.request.post(origin+offerA.exchange_path,{data:{ticket:offerA.ticket},headers:{Origin:origin}});
    assert.equal(replay.status(),403);
    const injected=await context.request.post(`${origin}/gliss/session`,{data:{credential:'mock-secret-a',vm_id:B},headers:{Origin:origin}});
    assert.equal(injected.status(),403);
    const invalidOrigin=await context.request.post(`${origin}/gliss/session`,{data:{credential:'mock-secret-a'},headers:{Origin:'http://evil.invalid'}});
    assert.equal(invalidOrigin.status(),403);
    const duplicate=await pageA.evaluate(path=>new Promise(resolve=>{
      const probe=new WebSocket(`ws://${location.host}${path}`);
      probe.onopen=()=>{probe.close();resolve('accepted');};probe.onerror=()=>resolve('rejected');
    }),scopeA.ws_path);
    assert.equal(duplicate,'rejected');
    // No browser-visible response contains the native worker token or cookie;
    // inspect state and public offer shapes rather than treating random strings
    // as proof merely because they look different.
    assert.deepEqual(Object.keys(offerA).sort(),['exchange_path','expires_at','ticket']);
    assert.deepEqual(Object.keys(scopeA).sort(),['capabilities','expires_at','view_path','ws_path']);
    const diagnostics=await state();assert.equal(diagnostics.workers.length,2);
    assert.ok(diagnostics.workers.every(worker=>worker.running&&Number.isInteger(worker.pid)));
    const bInputsBefore=b.inputs.filter(input=>input.type==='key').length;
    await pageA.locator('#screen').focus();await pageA.keyboard.down('a');
    await pageA.waitForFunction(()=>window.glissPrototype.pixel().join(',')==='31,67,129,255');
    await a.waitFor(inputs=>inputs.some(input=>input.type==='key'&&input.keysym===97&&input.down));
    assert.equal(b.inputs.filter(input=>input.type==='key').length,bInputsBefore);
    const mutation=await fetch(`${origin}/__test__/state`,{method:'POST',headers:{
      'Content-Type':'application/json','X-Gliss-Test-Control':control},
      body:JSON.stringify({action:'revoke_credential',vm_id:A})});
    assert.equal(mutation.status,200);
    await pageA.waitForFunction(()=>window.glissPrototype.socket.readyState===WebSocket.CLOSED);
    await a.waitFor(inputs=>inputs.some(input=>input.type==='key'&&input.keysym===97&&!input.down));
    await pageB.locator('#disconnect').click();
    await eventually(async()=>!(await state()).active);
    assert.ok(diagnostics.workers.every(worker=>{
      try{process.kill(worker.pid,0);return false;}catch(error){return error.code==='ESRCH';}
    }),'native worker processes must be reaped');
    const source=createHash('sha256');
    for(const name of ['gateway.py','workers.py','mock_backend.py','viewer.js','viewer.html','requirements.txt']){
      source.update(name+'\n');source.update(await readFile(`integration/distribox/prototype/${name}`));
    }
    const proof={kind:'owned-mock-gateway-browser',binary_sha256:createHash('sha256').update(await readFile(binary)).digest('hex'),
      prototype_source_sha256:source.digest('hex'),workers_snapshot:diagnostics.workers,
      browser_version:browser.version(),cpu_browser_args:CPU_BROWSER_ARGS,framesA:await pageA.evaluate(()=>window.glissPrototype.metrics.frames),
      framesB:await pageB.evaluate(()=>window.glissPrototype.metrics.frames),peak_active:diagnostics.peak_active,
      peak_launching:diagnostics.peak_launching,exact_initial_pixel:[12,34,56,255],exact_input_pixel:[31,67,129,255],
      cross_vm_input_rejected:true,credential_revocation_closed:true,held_key_released:true,workers_reaped:true};
    await browser.close();browser=null;
    await stop();assert.equal(child.exitCode,0,'gateway must exit gracefully after reaping native workers');
    assert.throws(()=>process.kill(child.pid,0),{code:'ESRCH'});
    await a.stop();await b.stop();peersStopped=true;
    Object.assign(proof,{gateway_pid:child.pid,gateway_reaped:true,browser_closed:true,owned_rfb_peers_closed:true,
      native_cookie_exposed:false,public_response_shapes_verified:true});
    const serialized=JSON.stringify(proof,null,2)+'\n';
    for(const secret of [control,cookieA.value,'mock-secret-a','mock-secret-b','mock-dashboard-token']){
      assert.ok(!serialized.includes(secret),'report must contain no control, stream-cookie or access secrets');
    }
    if(process.env.GLISS_PROTOTYPE_REPORT)await writeFile(process.env.GLISS_PROTOTYPE_REPORT,serialized);
    t.diagnostic(JSON.stringify(proof));
  });
