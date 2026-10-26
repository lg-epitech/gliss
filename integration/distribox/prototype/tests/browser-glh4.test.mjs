// CPU-only mock RFB/native/gateway/browser GLH4 correctness proof. Not a benchmark.
import test from 'node:test';
import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {createHash,randomBytes} from 'node:crypto';
import {mkdtemp,readFile,writeFile,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join,resolve,isAbsolute} from 'node:path';
import {launchBrowser,freePort,CPU_BROWSER_ARGS} from '../../../../tests/helpers.mjs';
import {startRfb} from '../../../../tests/mock-rfb.mjs';
import {startMotionRfb} from './motion-rfb.mjs';
const A='00000000-0000-4000-8000-000000000001',B='00000000-0000-4000-8000-000000000002';
const sleep=ms=>new Promise(resolve=>setTimeout(resolve,ms));
const deferred=()=>{let resolve;const promise=new Promise(done=>resolve=done);return{promise,resolve};};
async function eventually(predicate,timeout=8000){const deadline=Date.now()+timeout;while(Date.now()<deadline){if(await predicate())return;await sleep(20);}throw new Error('GLH4 gateway assertion deadline exceeded');}

test('GLH4 gateway: actual software media, exact input, resync, private cookies and revoked delayed loading',{timeout:90000},async t=>{
  const binary=process.env.GLISS_BINARY;
  if(!binary||!isAbsolute(binary))throw new Error('Supply an explicit absolute GLISS_BINARY supporting GLH4; no skipped success');
  const temporary=await mkdtemp(join(tmpdir(),'gliss-gateway-glh4-'));
  let a,b,child,browser,context,gateRoute,pageA,pageB,getState;const releaseModule=deferred();
  let phase="startup";const partial={kind:"owned-mock-gateway-glh4-partial",scope:"Partial correctness/cleanup evidence; no completion or performance claim"};
  const stop=async()=>{
    if(!child?.pid||child.exitCode!==null||child.signalCode!==null)return;
    const exited=new Promise(resolve=>child.once('exit',resolve));child.kill('SIGINT');let timer;
    await Promise.race([exited,new Promise(resolve=>timer=setTimeout(resolve,6000))]);clearTimeout(timer);
    if(child.exitCode===null&&child.signalCode===null){child.kill('SIGKILL');await exited;}
  };
  t.after(async()=>{
    releaseModule.resolve();
    partial.phase=phase;
    if(pageA)try{partial.metrics=await pageA.evaluate(()=>({...window.glissPrototype?.metrics}));}catch{partial.metrics_unavailable=true;}
    if(getState)try{partial.workers_before_cleanup=(await getState()).workers;}catch{partial.workers_unavailable=true;}
    await browser?.close();await stop();
    partial.gateway_exit_code=child?.exitCode;partial.gateway_signal=child?.signalCode;
    if(partial.workers_before_cleanup)partial.worker_pid_absence=partial.workers_before_cleanup.map(worker=>{
      try{process.kill(worker.pid,0);return{pid:worker.pid,absent:false};}catch(error){return{pid:worker.pid,absent:error.code==='ESRCH'};}
    });
    await a?.stop();await b?.stop();partial.owned_rfb_peers_closed=true;
    await rm(temporary,{recursive:true,force:true});t.diagnostic(JSON.stringify(partial));
  });
  a=await startMotionRfb();b=await startRfb();
  const port=await freePort(),origin=`http://127.0.0.1:${port}`,control=randomBytes(24).toString('base64url');
  const fixture=join(temporary,'owned-mock.json');
  await writeFile(fixture,JSON.stringify({mode:'owned-mock',vms:[
    {vm_id:A,node:'master',port:Number(a.address.split(':').at(-1)),credential:'mock-glh4-a'},
    {vm_id:B,node:'node_a',port:Number(b.address.split(':').at(-1)),credential:'mock-glh4-b'}]}));
  child=spawn(process.env.GLISS_PROTOTYPE_PYTHON||'python3',['-B','-m','integration.distribox.prototype.gateway',
    '--worker-binary',binary,'--worker-codec','hybrid-continuity','--fixtures',fixture,'--port',String(port),
    '--session-ttl','30','--ready-ttl','8','--reauthorize-interval','0.2'],
    {cwd:resolve('.'),env:{PATH:process.env.PATH,LANG:'C.UTF-8',GLISS_PROTOTYPE_TEST_CONTROL:control},stdio:['ignore','pipe','pipe']});
  let spawnError;child.on('error',error=>spawnError=error);for(const stream of [child.stdout,child.stderr])stream.on('data',()=>{});
  await eventually(async()=>{if(spawnError||child.exitCode!==null)throw new Error('GLH4 gateway startup failed');try{return(await fetch(origin+'/health')).ok;}catch{return false;}});
  const state=getState=async()=>{const r=await fetch(origin+'/__test__/state',{headers:{'X-Gliss-Test-Control':control}});assert.equal(r.status,200);return r.json();};
  const mutate=async vm_id=>{const r=await fetch(origin+'/__test__/state',{method:'POST',headers:{'X-Gliss-Test-Control':control,'Content-Type':'application/json'},body:JSON.stringify({action:'revoke_credential',vm_id})});assert.equal(r.status,200);};
  browser=await launchBrowser();context=await browser.newContext();
  const issue=async credential=>{const r=await context.request.post(origin+'/gliss/session',{data:{credential},headers:{Origin:origin}});assert.equal(r.status(),200);return r.json();};
  const exchange=async offer=>{const r=await context.request.post(origin+offer.exchange_path,{data:{ticket:offer.ticket},headers:{Origin:origin}});assert.equal(r.status(),200);return r.json();};
  const offerA=await issue('mock-glh4-a'),scopeA=await exchange(offerA);pageA=await context.newPage();
  const errors=[];pageA.on('pageerror',error=>errors.push(error.message));
  await pageA.addInitScript(()=>{
    const Native=globalThis.VideoDecoder;
    window.gatewayVideo={created:0,submitted:0,outputs:0,closed:0,configurations:[]};
    if(Native)globalThis.VideoDecoder=class extends Native{
      constructor(options){super({...options,output:frame=>{window.gatewayVideo.outputs++;options.output(frame);}});window.gatewayVideo.created++;}
      configure(configuration){window.gatewayVideo.configurations.push({codec:configuration.codec,hardwareAcceleration:configuration.hardwareAcceleration});return super.configure(configuration);}
      decode(chunk){window.gatewayVideo.submitted++;return super.decode(chunk);}
      close(){window.gatewayVideo.closed++;return super.close();}
    };
  });
  await pageA.goto(origin+scopeA.view_path);
  await pageA.waitForFunction(()=>window.glissPrototype?.metrics.hybrid_video_frames>=3&&window.glissPrototype.metrics.hybrid_video_delta_frames>=1,null,{timeout:10000});
  assert.deepEqual(await pageA.evaluate(()=>window.glissPrototype.pixel()),[12,34,56,255]);
  const initial=await pageA.evaluate(()=>({metrics:{...window.glissPrototype.metrics},video:window.gatewayVideo}));
  phase="actual-media-passed";partial.initial_media=initial;
  assert.equal(initial.metrics.codec,'hybrid-continuity');assert.equal(initial.metrics.protocol,'GLH4');assert.equal(initial.metrics.mask_grid,32);
  assert.ok(initial.video.outputs>=3&&initial.video.submitted>=3);assert.ok(initial.video.configurations.every(c=>c.hardwareAcceleration==='prefer-software'));
  await pageA.locator('#screen').focus();await pageA.keyboard.press('b');
  await pageA.waitForFunction(()=>window.glissPrototype.pixel().join(',')==='31,67,129,255');
  assert.ok(a.inputs.some(value=>value.type==='key'&&value.keysym===98&&value.down));
  phase='input-passed';partial.exact_input_rgba=[31,67,129,255];
  const beforeRecovery=await pageA.evaluate(()=>({...window.glissPrototype.metrics}));
  phase='independent-key-recovery';partial.recovery_before=beforeRecovery;
  await pageA.evaluate(()=>window.glissPrototype.resync());
  await pageA.waitForFunction(before=>window.glissPrototype.metrics.recoveries>before.recoveries&&window.glissPrototype.metrics.frames>before.frames&&
    window.glissPrototype.metrics.hybrid_video_frames>=before.hybrid_video_frames+2,beforeRecovery,{timeout:8000});
  assert.deepEqual(await pageA.evaluate(()=>window.glissPrototype.pixel()),[31,67,129,255]);
  phase='recovery-passed';partial.recovery_after=await pageA.evaluate(()=>({...window.glissPrototype.metrics}));
  const cookiesA=await context.cookies(origin+scopeA.view_path),cookieA=cookiesA.find(value=>value.name==='gliss_stream');
  assert.deepEqual({httpOnly:cookieA?.httpOnly,sameSite:cookieA?.sameSite,path:cookieA?.path},{httpOnly:true,sameSite:'Strict',path:scopeA.view_path});
  assert.ok(!cookiesA.some(value=>value.name==='gliss_session'));
  const injected=await context.request.post(origin+'/gliss/session',{data:{credential:'mock-glh4-a',codec:'deflate'},headers:{Origin:origin}});assert.equal(injected.status(),403);
  const replay=await context.request.post(origin+offerA.exchange_path,{data:{ticket:offerA.ticket},headers:{Origin:origin}});assert.equal(replay.status(),403);
  const scopeB=await exchange(await issue('mock-glh4-b'));pageB=await context.newPage();
  pageB.on('pageerror',error=>errors.push(error.message));
  await pageB.route('**/gliss/assets/hybrid-continuity-decoder.js',async route=>{gateRoute=route;await releaseModule.promise;await route.continue().catch(()=>{});});
  await pageB.goto(origin+scopeB.view_path);await eventually(()=>Boolean(gateRoute));
  assert.equal(await pageB.evaluate(()=>window.glissPrototype.metrics.frames),0);
  assert.equal(await pageB.evaluate(()=>window.glissPrototype.socket.readyState),1);
  const allCookies=await context.cookies();assert.equal(allCookies.filter(value=>value.name==='gliss_stream').length,2);
  const diagnostics=await state();assert.equal(diagnostics.active,2);assert.equal(diagnostics.peak_launching,1);
  await pageA.locator('#screen').focus();await pageA.keyboard.down('Control');
  await eventually(()=>a.inputs.some(value=>value.type==='key'&&value.keysym===0xffe3&&value.down));
  phase='revocation';
  await mutate(A);await pageA.waitForFunction(()=>window.glissPrototype.socket.readyState===WebSocket.CLOSED);
  await eventually(()=>a.inputs.some(value=>value.type==='key'&&value.keysym===0xffe3&&!value.down));
  await mutate(B);await pageB.waitForFunction(()=>window.glissPrototype.socket.readyState===WebSocket.CLOSED);
  const frameCountA=await pageA.evaluate(()=>window.glissPrototype.metrics.frames);
  releaseModule.resolve();await sleep(200);
  assert.equal(await pageB.evaluate(()=>window.glissPrototype.metrics.frames),0,'Revoked late module completion cannot paint');
  assert.equal(await pageA.evaluate(()=>window.glissPrototype.metrics.frames),frameCountA);
  await eventually(async()=>!(await state()).active);
  const finalMetrics=await pageA.evaluate(()=>({...window.glissPrototype.metrics}));
  assert.equal(finalMetrics.errors,0);assert.deepEqual(errors,[]);assert.equal(a.connections,1);
  for(const worker of diagnostics.workers)assert.throws(()=>process.kill(worker.pid,0),{code:'ESRCH'});
  const sources={};
  for(const path of ['gateway.py','workers.py','mock_backend.py','viewer.js','viewer-media.js','viewer.html'])
    sources[path]=createHash('sha256').update(await readFile(`integration/distribox/prototype/${path}`)).digest('hex');
  for(const path of ['decoder.js','video-decoder.js','hybrid-continuity-decoder.js'])
    sources[`web/${path}`]=createHash('sha256').update(await readFile(`web/${path}`)).digest('hex');
  const proof={kind:'owned-mock-gateway-glh4-browser',scope:'Correctness only; no VM, performance or production-attestation claim',
    binary_sha256:createHash('sha256').update(await readFile(binary)).digest('hex'),source_hashes:sources,
    cpu_browser_args:CPU_BROWSER_ARGS,browser_version:browser.version(),actual_video:initial.video,metrics:finalMetrics,
    exact_unmasked_initial_rgba:[12,34,56,255],exact_input_rgba:[31,67,129,255],scoped_cookies:true,native_cookie_exposed:false,
    codec_request_injection_rejected:true,replay_rejected:true,resync_recovered:true,revocation_released_held_key:true,
    revoked_delayed_module_paints:0,workers_reaped:true};
  await browser.close();browser=null;await stop();assert.equal(child.exitCode,0);assert.throws(()=>process.kill(child.pid,0),{code:'ESRCH'});
  proof.gateway_reaped=true;proof.browser_closed=true;
  const serialized=JSON.stringify(proof,null,2)+'\n';
  for(const secret of [control,cookieA.value,'mock-glh4-a','mock-glh4-b'])assert.ok(!serialized.includes(secret));
  if(process.env.GLISS_PROTOTYPE_GLH4_REPORT)await writeFile(process.env.GLISS_PROTOTYPE_GLH4_REPORT,serialized,{flag:'wx'});
  phase='complete';t.diagnostic(JSON.stringify(proof));
});
