// node scripts/bench-lossless-browser.mjs --output /tmp/gliss-bench.json
// Optional: --baseline REF --frames 180 --repetitions 4 --codec raw,palette,palette-zlib --workload sparse,motion,empty
import assert from 'node:assert/strict';
import {execFileSync} from 'node:child_process';
import {createHash} from 'node:crypto';
import {open, readFile, readdir} from 'node:fs/promises';
import {createServer} from 'node:http';
import {cpus, platform, release} from 'node:os';
import {resolve} from 'node:path';
import {fileURLToPath} from 'node:url';
import {launchBrowser, CPU_BROWSER_ARGS} from '../tests/helpers.mjs';

const root=new URL('../',import.meta.url), options={baseline:'25642b57',frames:'180',repetitions:'4',codec:'raw,palette',workload:'sparse,motion'};
for(let i=2;i<process.argv.length;i++){
  const match=/^--(output|baseline|frames|repetitions|codec|workload)(?:=(.*))?$/.exec(process.argv[i]);
  assert.ok(match,`Unknown argument: ${process.argv[i]}`);
  const value=match[2]??process.argv[++i];
  assert.ok(value&&!value.startsWith('--'),`Missing value for --${match[1]}`);
  options[match[1]]=value;
}
assert.ok(options.output,'--output PATH is required and must name a new file');
const frames=Number(options.frames), repetitions=Number(options.repetitions), codecs=options.codec.split(','),workloads=options.workload.split(',');
assert.ok(Number.isInteger(frames)&&frames>0&&frames<=1800,'--frames must be an integer from 1 to 1800');
assert.ok(Number.isInteger(repetitions)&&repetitions>0&&repetitions<=10,'--repetitions must be an integer from 1 to 10');
assert.ok(codecs.length&&new Set(codecs).size===codecs.length&&codecs.every(c=>['raw','palette','palette-zlib'].includes(c)),'Invalid --codec list');
assert.ok(workloads.length&&new Set(workloads).size===workloads.length&&workloads.every(w=>['sparse','motion','empty'].includes(w)),'Invalid --workload list');
assert.equal(platform(),'linux','CDP process identity checks require Linux /proc');
const output=await open(resolve(options.output),'wx'), hash=bytes=>createHash('sha256').update(bytes).digest('hex');

// Serialized into a browser module, so packet construction and the scalar oracle
// stay outside both the renderer timer and the enclosing CDP CPU samples.
function installHarness(GlissClient){
  const ensure=(condition,message)=>{if(!condition)throw new Error(message);};
  class LocalSocket{
    static OPEN=1;
    constructor(){this.readyState=1;this.acks=0;this.sequence=-1;queueMicrotask(()=>this.onopen?.());}
    send(text){
      const ack=JSON.parse(text);
      if(ack.type==='ack'){
        ensure(ack.sequence===this.sequence+1,'ACK sequence mismatch');
        this.sequence=ack.sequence;this.acks++;
      }
    }
    close(){this.readyState=3;}
  }
  window.WebSocket=LocalSocket;
  const color=(index,phase)=>[(index*47+phase*17)&255,(index*83+phase*29)&255,(index*131+phase*43)&255,255];
  const compress=async bytes=>new Uint8Array(await new Response(new Blob([bytes]).stream().pipeThrough(new CompressionStream('deflate'))).arrayBuffer());
  window.prepare=async({width,height,workload,codec})=>{
    const rectangles=(phase,full=false)=>{
      // Each position changes twice per cycle; replay keeps changing pixels.
      if(!full&&workload==='empty')return [];
      if(!full&&workload==='sparse')return phase%4===0?[]:[{x:64*(1+phase%3),y:64,w:64,h:64,phase}];
      const tiles=[];
      for(let y=0;y<height;y+=64)for(let x=0;x<width;x+=64){
        tiles.push({x,y,w:Math.min(64,width-x),h:Math.min(64,height-y),phase});
      }
      return tiles;
    };
    const rgba=(r,x,y)=>codec==='raw'?[(x+r.x+r.phase*17)&255,(y+r.y+r.phase*29)&255,(x+y+r.phase*43)&255,255]:color(((x+r.x)*3+(y+r.y)*7+r.phase*11)%16,r.phase);
    const packet=async(tiles,keyframe,sequence)=>{
      const parts=[];
      for(const r of tiles){
        let body=new Uint8Array(codec==='raw'?r.w*r.h*4:66+r.w*r.h);
        if(codec!=='raw'){
          new DataView(body.buffer).setUint16(0,16,true);
          for(let i=0;i<16;i++)body.set(color(i,r.phase),2+i*4);
        }
        for(let y=0;y<r.h;y++)for(let x=0;x<r.w;x++){
          if(codec==='raw')body.set(rgba(r,x,y),(y*r.w+x)*4);
          else body[66+y*r.w+x]=((x+r.x)*3+(y+r.y)*7+r.phase*11)%16;
        }
        if(codec==='palette-zlib')body=await compress(body);
        const header=new Uint8Array(16),view=new DataView(header.buffer);
        for(const [offset,value]of [[0,r.x],[2,r.y],[4,r.w],[6,r.h]])view.setUint16(offset,value,true);
        header[8]=codec==='raw'?0:codec==='palette'?2:3;view.setUint32(12,body.length,true);parts.push(header,body);
      }
      const bytes=new Uint8Array(24+parts.reduce((n,part)=>n+part.length,0)),view=new DataView(bytes.buffer);
      bytes.set([71,76,83,49]);view.setUint32(4,sequence,true);view.setUint16(12,width,true);view.setUint16(14,height,true);
      view.setUint16(16,tiles.length,true);view.setUint16(18,keyframe?1:0,true);
      let offset=24;
      for(const part of parts){bytes.set(part,offset);offset+=part.length;}
      return bytes.buffer;
    };
    const keyTiles=rectangles(0,true),deltaTiles=Array.from({length:8},(_,i)=>rectangles(i+1));
    const key=await packet(keyTiles,true,0),deltas=[];
    for(let i=0;i<8;i++)deltas.push(await packet(deltaTiles[i],false,i+1));
    const packetHashes=[];
    for(const bytes of [key,...deltas]){
      const digest=new Uint8Array(await crypto.subtle.digest('SHA-256',bytes));
      packetHashes.push(Array.from(digest,v=>v.toString(16).padStart(2,'0')).join(''));
    }
    const canvas=document.createElement('canvas');canvas.tabIndex=0;document.body.append(canvas);
    const client=new GlissClient(canvas,()=>{},()=>{},{url:'local://controlled'});await client.connect();
    let sequence=0;
    const deliver=async bytes=>{
      new DataView(bytes).setUint32(4,sequence++,true);
      client.socket.onmessage({data:bytes});await client.chain;
    };
    const apply=(pixels,tiles)=>{
      for(const r of tiles)for(let y=0;y<r.h;y++)for(let x=0;x<r.w;x++){
        pixels.set(rgba(r,x,y),((r.y+y)*width+r.x+x)*4);
      }
    };
    const expected=new Uint8ClampedArray(width*height*4);
    apply(expected,keyTiles);for(const tiles of deltaTiles)apply(expected,tiles);
    await deliver(key);
    for(let i=0;i<24;i++)await deliver(deltas[i%8]);
    window.check=extra=>{
      // A complete eight-packet cycle overwrites every touched pixel, so its
      // final image is stable across cycles, including empty sparse deltas.
      const pixels=expected.slice();for(let i=0;i<extra%8;i++)apply(pixels,deltaTiles[i]);
      ensure(canvas.width===width&&canvas.height===height,'Canvas dimensions differ');
      const actual=client.ctx.getImageData(0,0,width,height).data;
      for(let i=0;i<pixels.length;i++)if(actual[i]!==pixels[i])throw new Error(`Canvas mismatch at byte ${i}`);
      ensure(client.metrics.errors===0,'Client error');
      ensure(client.metrics.frames===sequence&&client.socket.acks===sequence,'Frame/ACK count mismatch');
      return {wholeCanvasBytes:actual.length,frames:client.metrics.frames,acks:client.socket.acks,errors:client.metrics.errors};
    };
    window.run=async count=>{
      client.metrics.decodeMs=[];client.metrics.paintMs=[];
      const before=client.metrics.frames,began=performance.now();
      for(let i=0;i<count;i++)await deliver(deltas[i%8]);
      const wallMs=performance.now()-began;
      ensure(client.metrics.frames-before===count&&client.socket.acks===sequence&&client.metrics.errors===0,'Timed frame/ACK/error mismatch');
      ensure(client.metrics.decodeMs.length===count&&client.metrics.paintMs.length===count,'Timed sample count mismatch');
      ensure([...client.metrics.decodeMs,...client.metrics.paintMs].every(n=>Number.isFinite(n)&&n>=0),'Invalid timing sample');
      return {wallMs,frames:count,acks:count,errors:client.metrics.errors,decodeMs:client.metrics.decodeMs,paintMs:client.metrics.paintMs};
    };
    return {packetHashes,packetBytes:[key,...deltas].map(p=>p.byteLength),warmupFrames:24};
  };
}

const report={startedAt:new Date().toISOString(),scope:'Controlled GLS1 packets through real GlissClient, decoder, requestAnimationFrame, Canvas and ACK in CPU Chromium. Packet generation and whole-canvas readback excluded from timing. Process CPU sums CDP-reported processes present at both endpoints; exited intermediate processes are unobserved. Process CPU and renderer TaskDuration enclose the timed page evaluation and CDP boundary calls. No native encoder, network transport or physical-display claim.',
  options:{...options,frames,repetitions,codecs,workloads},browserArgs:CPU_BROWSER_ARGS,
  host:{platform:platform(),release:release(),cpu:cpus()[0]?.model,logicalCPUs:cpus().length,
    cpuAffinity:(await readFile('/proc/self/status','utf8')).match(/^Cpus_allowed_list:[ \t]+(.+)$/m)?.[1]},
  plannedRuns:codecs.length*2*workloads.length*repetitions*2,runs:[]};
let browser,server;
try{
  const git=(...args)=>execFileSync('git',args,{cwd:fileURLToPath(root),encoding:'utf8'});
  const baseline=git('rev-parse','--verify','--end-of-options',`${options.baseline}^{commit}`).trim();
  report.baselineCommit=baseline;report.candidateCommit=git('rev-parse','HEAD').trim();
  report.candidateDirty=!!git('status','--porcelain','--','web/client.js','web/decoder.js').trim();
  const shared=new Map();
  for(const name of (await readdir(new URL('web/',root),{recursive:true})).filter(n=>n.endsWith('.js')).sort()){
    shared.set(name,await readFile(new URL(`web/${name}`,root),'utf8'));
  }
  const variants={baseline:new Map(shared),candidate:new Map(shared)};
  for(const name of ['client.js','decoder.js'])variants.baseline.set(name,git('show',`${baseline}:web/${name}`));
  report.sourceHashes=Object.fromEntries(Object.entries(variants).map(([name,files])=>[name,Object.fromEntries([...files].map(([file,source])=>[file,hash(source)]))]));
  report.harnessSHA256=hash(await readFile(fileURLToPath(import.meta.url)));
  server=createServer((request,response)=>{
    const [,variant,...parts]=new URL(request.url,'http://localhost').pathname.split('/'),name=parts.join('/');
    if(!Object.hasOwn(variants,variant)){response.writeHead(404);response.end();return;}
    if(!name){response.setHeader('Content-Type','text/html');response.end('<!doctype html><script type="module" src="bench.js"></script>');return;}
    const source=name==='bench.js'?`import {GlissClient} from './client.js';(${installHarness.toString()})(GlissClient);`:variants[variant].get(name);
    response.writeHead(source===undefined?404:200,{'Content-Type':'text/javascript'});response.end(source);
  });
  await new Promise((resolve,reject)=>{server.once('error',reject);server.listen(0,'127.0.0.1',resolve);});
  browser=await launchBrowser();report.chromiumVersion=browser.version();
  const browserCDP=await browser.newBrowserCDPSession();
  const snapshot=async()=>{
    const began=performance.now(),{processInfo}=await browserCDP.send('SystemInfo.getProcessInfo');
    const processes=await Promise.all(processInfo.map(async row=>{
      const stat=await readFile(`/proc/${row.id}/stat`,'utf8'),startTicks=stat.slice(stat.lastIndexOf(')')+2).split(' ')[19];
      assert.match(startTicks,/^\d+$/,'Missing CPU process start time');
      return {...row,startTicks};
    }));
    return {began,ended:performance.now(),processes:processes.sort((a,b)=>a.id-b.id)};
  };
  const cpu=(before,after)=>{
    assert.deepEqual(before.processes.map(p=>[p.type,p.id,p.startTicks]),after.processes.map(p=>[p.type,p.id,p.startTicks]),'CPU process identities changed');
    let totalMs=0,rendererMs=0;
    for(let i=0;i<before.processes.length;i++){
      const elapsed=1000*(after.processes[i].cpuTime-before.processes[i].cpuTime);
      assert.ok(Number.isFinite(elapsed)&&elapsed>=0,'Invalid process CPU delta');
      totalMs+=elapsed;
      if(after.processes[i].type==='renderer')rendererMs+=elapsed;
    }
    assert.ok(before.processes.some(p=>p.type==='renderer'),'Renderer CPU process missing');
    const wallMs=(after.began+after.ended-before.began-before.ended)/2;
    assert.ok(after.began>before.ended,'CPU sample windows overlap');
    return {totalMs,rendererMs,wallMs,percentOneCore:100*totalMs/wallMs,rendererPercentOneCore:100*rendererMs/wallMs,
      wallBoundsMs:[after.began-before.ended,after.ended-before.began]};
  };
  const packetIdentities=new Map();
  for(const codec of codecs)for(const [width,height]of [[1280,800],[1920,1080]])for(const workload of workloads)for(let repetition=0;repetition<repetitions;repetition++){
    for(const variant of repetition%2?['candidate','baseline']:['baseline','candidate']){
      const row={codec,width,height,workload,repetition,variant};report.runs.push(row);
      let page;
      const errors=[];
      try{
        page=await browser.newPage({viewport:{width:1920,height:1080}});
        page.on('pageerror',error=>errors.push(error.message));
        page.on('requestfailed',request=>errors.push(`${request.url()}: ${request.failure()?.errorText}`));
        await page.goto(`http://127.0.0.1:${server.address().port}/${variant}/`);await page.waitForFunction(()=>window.prepare);
        row.packets=await page.evaluate(config=>window.prepare(config),{codec,width,height,workload});
        const key=JSON.stringify({codec,width,height,workload});
        if(packetIdentities.has(key))assert.deepEqual(row.packets,packetIdentities.get(key),'Variants received different packets');
        else packetIdentities.set(key,row.packets);
        row.before=await page.evaluate(()=>window.check(0));assert.deepEqual(errors,[]);
        const cdp=await page.context().newCDPSession(page);await cdp.send('Performance.enable');
        const task=async()=>{
          const {metrics}=await cdp.send('Performance.getMetrics');
          return metrics.find(m=>m.name==='TaskDuration').value;
        };
        const taskBefore=await task();row.cpuBefore=await snapshot();
        row.measurement=await page.evaluate(count=>window.run(count),frames);
        row.cpuAfter=await snapshot();row.rendererTaskMs=1000*((await task())-taskBefore);
        row.cpu=cpu(row.cpuBefore,row.cpuAfter);
        assert.ok(Number.isFinite(row.rendererTaskMs)&&row.rendererTaskMs>=0,'Invalid renderer task delta');
        row.after=await page.evaluate(count=>window.check(count),frames);assert.deepEqual(errors,[]);
        row.status='passed';console.log(JSON.stringify({codec,width,height,workload,repetition,variant,...row.measurement,cpu:row.cpu,rendererTaskMs:row.rendererTaskMs}));
      }catch(error){row.status='failed';row.failure=error.stack??String(error);row.browserErrors=errors;throw error;}
      finally{await page?.close();}
    }
  }
  assert.equal(report.runs.length,report.plannedRuns);report.status='passed';
}catch(error){report.status='failed';report.failure=error.stack??String(error);process.exitCode=1;console.error(error.message);}
finally{
  try{await browser?.close();}catch(error){report.status='failed';report.cleanupFailure=String(error);process.exitCode=1;}
  if(server)await new Promise(resolve=>server.close(resolve));
  report.finishedAt=new Date().toISOString();await output.writeFile(JSON.stringify(report,null,2)+'\n');await output.close();
}
