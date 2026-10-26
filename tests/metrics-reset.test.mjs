import test from 'node:test';
import assert from 'node:assert/strict';
import {writeFile,readFile,copyFile,mkdtemp,rm} from 'node:fs/promises';
import {constants} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {createHash} from 'node:crypto';
import {launchBrowser,startServer,loginPage,freePort,CPU_BROWSER_ARGS} from './helpers.mjs';
import {startProxy,PROFILES} from '../test-support/browser/tcp-proxy.mjs';

test('resetting counters between real decode and paint preserves a multi-frame stream',{timeout:20000},async t=>{
  const port=await freePort();
  const proxy=await startProxy({targetPort:port,profile:PROFILES.wan});t.after(proxy.stop);
  const server=await startServer({port,origin:proxy.url,codec:'global',workload:'scroll',inFlightFrames:4});t.after(server.stop);
  const browser=await launchBrowser();t.after(()=>browser.close());
  const page=await browser.newPage();
  const pageErrors=[];page.on('pageerror',error=>pageErrors.push(error.message));
  let clientSource;
  if(process.env.GLISS_METRICS_RESET_CLIENT){
    const source=clientSource=await readFile(process.env.GLISS_METRICS_RESET_CLIENT,'utf8');
    await page.route('**/client.js',route=>route.fulfill({status:200,contentType:'text/javascript',body:source}));
  }
  await loginPage(page,server,proxy.url);
  await page.evaluate(reset=>{
    const client=window.gliss;
    window.metricsResetTrace={status:[],packets:[],packetCount:0,resetBoundary:null};
    const previousStatus=client.onStatus;
    client.onStatus=value=>{if(window.metricsResetTrace.status.length<32)window.metricsResetTrace.status.push(value);previousStatus(value);};
    client.socket.addEventListener('message',event=>{
      if(typeof event.data==='string')return;
      window.metricsResetTrace.packetCount++;
      if(window.metricsResetTrace.packets.length>=12)return;
      const view=new DataView(event.data);
      window.metricsResetTrace.packets.push({sequence:view.getUint32(4,true),keyframe:Boolean(view.getUint16(18,true)&1)});
    });
    // Force the real benchmark race at its exact seam: an in-progress frame
    // already recorded decode time, then the counters reset before UI notify.
    // The production decoder, Canvas paint, ACKs and media chain still run.
    if(reset){
      const paint=client.ctx.putImageData.bind(client.ctx);
      client.ctx.putImageData=(...args)=>{
        window.metricsResetTrace.resetBoundary={decode_samples_before:client.metrics.decodeMs.length,at_page_ms:performance.now()};
        client.metrics={frames:0,bytes:0,decodeMs:[],presentTimes:[],forwardedInputAgeMs:[],pixelEchoLatencyMs:[],pixelEchoEvents:[],errors:0};
        client.ctx.putImageData=paint;
        return paint(...args);
      };
    }
  },process.env.GLISS_METRICS_RESET_CONTROL!=='no-reset');
  await page.waitForTimeout(1500);
  const result=await page.evaluate(()=>({trace:window.metricsResetTrace,errors:window.gliss.metrics.errors,frames:window.gliss.metrics.frames,status:document.getElementById('status').textContent}));
  if(process.env.GLISS_METRICS_RESET_REPORT){
    const directory=await mkdtemp(join(tmpdir(),'gliss-metrics-reset-'));
    t.after(()=>rm(directory,{recursive:true,force:true}));
    const temporary=join(directory,'report.json');
    const hash=bytes=>createHash('sha256').update(bytes).digest('hex');
    const decoder=Buffer.from(await(await fetch(`${server.url}/decoder.js`)).arrayBuffer());
    const embeddedClient=Buffer.from(await(await fetch(`${server.url}/client.js`)).arrayBuffer());
    await writeFile(temporary,JSON.stringify({kind:'live-benchmark-metrics-reset-regression',created_at:new Date().toISOString(),
      browser:browser.version(),browser_args:CPU_BROWSER_ARGS,network_profile:PROFILES.wan,
      server_command:server.child.spawnargs,binary_sha256:hash(await readFile(server.child.spawnargs[0])),
      embedded_client_sha256:hash(embeddedClient),loaded_client_sha256:hash(clientSource??embeddedClient),embedded_decoder_sha256:hash(decoder),
      client_override:Boolean(clientSource),reset_counters:process.env.GLISS_METRICS_RESET_CONTROL!=='no-reset',
      result,pageErrors},null,2)+'\n',{flag:'wx'});
    await copyFile(temporary,process.env.GLISS_METRICS_RESET_REPORT,constants.COPYFILE_EXCL);
  }
  t.diagnostic(JSON.stringify(result));
  assert.deepEqual(pageErrors,[]);
  if(process.env.GLISS_METRICS_RESET_CONTROL!=='no-reset')assert.ok(result.trace.resetBoundary?.decode_samples_before>0);
  assert.equal(result.errors,0,'resetting measurement counters must not reset media references');
  assert.ok(result.frames>10);
});
