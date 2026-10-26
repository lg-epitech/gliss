import test from 'node:test';
import assert from 'node:assert/strict';
import {launchBrowser,startServer,loginPage} from './helpers.mjs';

function deferred(){
  let resolve;
  const promise=new Promise(done=>resolve=done);
  return {promise,resolve};
}

async function fixture(t,codec){
  const state={closing:false,releases:[],pageErrors:[]};
  let server,browser;
  t.after(async()=>{
    state.closing=true;
    for(const release of state.releases)release();
    await browser?.close();await server?.stop();
  });
  server=await startServer({codec,workload:'video',fps:15,inFlightFrames:4});
  browser=await launchBrowser();
  // Each document has an empty module map and a fresh HTTP cache. Otherwise a
  // cached optional module could conceal an unwanted bootstrap request.
  const context=await browser.newContext(),page=await context.newPage();
  page.on('pageerror',error=>state.pageErrors.push(error.message));
  await page.addInitScript(()=>{
    const NativeWebSocket=window.WebSocket;
    const trace=window.negotiationWire={nextSocket:0,events:[]};
    window.WebSocket=class extends NativeWebSocket{
      constructor(...args){
        super(...args);const socket=++trace.nextSocket;
        this.addEventListener('open',()=>trace.events.push({socket,type:'open'}));
        this.addEventListener('message',event=>{
          if(typeof event.data!=='string')return;
          const message=JSON.parse(event.data);
          if(message.type==='hello')trace.events.push({socket,type:'hello',codec:message.codec,source:message.source});
        });
      }
    };
  });
  return {server,page,state};
}

async function actualPixelEcho(page){
  // A key-up carries no held input into cleanup; it still updates the real
  // server's exact reserved UI probe outside Hybrid video masks.
  const id=await page.evaluate(()=>window.gliss.send({type:'key',keysym:97,down:false},{pixelEcho:true}));
  assert.ok(id>0,'input must reach an open native stream');
  await page.waitForFunction(id=>window.gliss.readProbe()===(id&0xffffff),id,{timeout:5000});
  const evidence=await page.evaluate(()=>({errors:window.gliss.metrics.errors,frames:window.gliss.metrics.frames,
    bytes:window.gliss.metrics.bytes,pixel:Array.from(window.gliss.ctx.getImageData(0,0,1,1).data)}));
  assert.equal(evidence.errors,0);
  assert.ok(evidence.frames>0&&evidence.bytes>0);
  assert.deepEqual(evidence.pixel,[id&255,(id>>>8)&255,(id>>>16)&255,255]);
}

for(const codec of ['deflate','global','global-cached'])test(`negotiation ${codec}: login and actual lossless stream never request Hybrid`,{timeout:30000},async t=>{
  const {server,page,state}=await fixture(t,codec),requests=[];
  await page.route('**/hybrid-decoder.js',async route=>{requests.push(route.request().url());await route.continue();});
  await loginPage(page,server);
  await actualPixelEcho(page);
  assert.equal(await page.evaluate(()=>window.gliss.codec),codec);
  assert.deepEqual(await page.evaluate(()=>window.negotiationWire.events.map(event=>[event.type,event.codec])),
    [['open',undefined],['hello',codec]]);
  assert.deepEqual(requests,[],'lossless bootstrap and streaming must leave the optional Hybrid module unfetched');
  assert.deepEqual(state.pageErrors,[]);
});

test('negotiation Hybrid: optional module request follows socket open and actual native hello',{timeout:30000},async t=>{
  const {server,page,state}=await fixture(t,'hybrid'),requests=[];
  await page.route('**/hybrid-decoder.js',async route=>{
    // Inspect events observed in the browser before allowing the HTTP response;
    // cross-process Playwright timestamps are not used to infer ordering.
    requests.push({url:route.request().url(),wire:await page.evaluate(()=>window.negotiationWire.events)});
    await route.continue();
  });
  await loginPage(page,server);
  await actualPixelEcho(page);
  assert.equal(requests.length,1,'fresh context must request exactly one optional Hybrid module');
  assert.deepEqual(requests[0].wire.map(event=>[event.socket,event.type,event.codec]),
    [[1,'open',undefined],[1,'hello','hybrid']]);
  assert.equal(await page.evaluate(()=>window.gliss.codec),'hybrid');
  assert.equal(await page.evaluate(()=>window.gliss.decoder.frame.hybrid),true,'real GLH1 frames must use the negotiated decoder');
  assert.deepEqual(state.pageErrors,[]);
});

test('negotiation reconnect: delayed shared module import cannot install abandoned decoder or hello metadata',{timeout:30000},async t=>{
  const {server,page,state}=await fixture(t,'hybrid');
  const requested=deferred(),release=deferred();state.releases.push(release.resolve);
  let moduleRequests=0,connections=0;
  await page.route('**/hybrid-decoder.js',async route=>{
    moduleRequests++;requested.resolve();await release.promise;
    if(state.closing){await route.abort().catch(()=>{});return;}
    // Fetch and deliver the server's actual module, without replacing any
    // decoder export or media payload. Pending same-URL imports share one fetch.
    await route.fulfill({response:await route.fetch()});
  });
  await page.routeWebSocket('**/ws',socket=>{
    const connection=++connections,upstream=socket.connectToServer();
    upstream.onMessage(message=>{
      if(connection===1&&typeof message==='string'){
        const value=JSON.parse(message);
        if(value.type==='hello'){
          // Make a stale metadata callback observable even though both native
          // sessions negotiate the same codec. Every media/input/ACK is intact.
          socket.send(JSON.stringify({...value,source:'abandoned-session'}));return;
        }
      }
      socket.send(message);
    });
    // Default client-to-server forwarding and bidirectional close propagation
    // remain active: this route is connected to the real native worker.
  });
  await page.goto(server.url);
  await page.evaluate(()=>{
    const client=window.gliss;
    const trace=window.negotiationRace={hellos:[],decoderWrites:[]};
    const notify=client.onEvent;
    client.onEvent=event=>{if(event.type==='hello')trace.hellos.push({codec:event.codec,source:event.source});notify(event);};
    let decoder=client.decoder;
    // Record actual public decoder installations, not constructor names. A
    // stale transient install must fail even if the second hello later repairs it.
    Object.defineProperty(client,'decoder',{configurable:true,enumerable:true,get:()=>decoder,
      set:value=>{decoder=value;trace.decoderWrites.push(value);}});
  });
  await page.locator('#token').fill(server.token);
  await page.locator('#login button').click();
  await requested.promise;
  await page.waitForFunction(()=>window.negotiationWire.events.some(event=>event.socket===1&&event.type==='hello'),null,{timeout:5000});
  assert.equal(await page.evaluate(()=>window.gliss.metrics.frames),0,'held real module must prevent first-session decoding');
  await page.evaluate(()=>{
    const client=window.gliss,trace=window.negotiationRace;
    trace.abandonedChain=client.chain;trace.abandonedSocket=client.socket;
    client.connect();trace.replacementSocket=client.socket;trace.replacementBaseline=client.decoder;
    trace.baselineWrites=trace.decoderWrites.length;
  });
  await page.waitForFunction(()=>window.negotiationWire.events.some(event=>event.socket===2&&event.type==='hello'),null,{timeout:5000});
  assert.equal(moduleRequests,1,'replacement import must reuse the held request in this fresh document');
  assert.equal(await page.evaluate(()=>window.negotiationRace.hellos.length),0,'pending hello callbacks must remain gated');
  release.resolve();
  await page.waitForFunction(()=>window.gliss.metrics.frames>0,null,{timeout:15000});
  await page.evaluate(async()=>{await window.negotiationRace.abandonedChain;await window.gliss.chain;});
  await actualPixelEcho(page);
  const result=await page.evaluate(()=>{
    const trace=window.negotiationRace,client=window.gliss;
    return {hellos:trace.hellos,installsAfterReplacement:trace.decoderWrites.length-trace.baselineWrites,
      activeInstalledLast:trace.decoderWrites.at(-1)===client.decoder,
      replacedBaseline:client.decoder!==trace.replacementBaseline,hybridFrame:client.decoder.frame.hybrid,
      sameSocket:client.socket===trace.replacementSocket,source:window.glissSource(),codec:client.codec};
  });
  assert.deepEqual(result,{hellos:[{codec:'hybrid',source:'demo'}],installsAfterReplacement:1,
    activeInstalledLast:true,replacedBaseline:true,hybridFrame:true,sameSocket:true,source:'demo',codec:'hybrid'});
  assert.equal(moduleRequests,1);
  assert.equal(connections,2);
  assert.deepEqual(state.pageErrors,[]);
});

test('negotiation closed session: delayed import cannot install a decoder, deliver hello or revive display status',{timeout:30000},async t=>{
  const {server,page,state}=await fixture(t,'hybrid');
  const requested=deferred(),release=deferred();state.releases.push(release.resolve);
  let moduleRequests=0;
  await page.route('**/hybrid-decoder.js',async route=>{
    moduleRequests++;requested.resolve();await release.promise;
    if(state.closing){await route.abort().catch(()=>{});return;}
    await route.fulfill({response:await route.fetch()});
  });
  // No WebSocket route: close the actual browser/native connection while only
  // its optional HTTP module is held. Frames and protocol messages stay intact.
  await page.goto(server.url);
  await page.evaluate(()=>{
    const client=window.gliss;
    const trace=window.negotiationClose={hellos:[],decoderWrites:[],domStatuses:[]};
    const notify=client.onEvent;
    client.onEvent=event=>{if(event.type==='hello')trace.hellos.push({codec:event.codec,source:event.source});notify(event);};
    let decoder=client.decoder;
    Object.defineProperty(client,'decoder',{configurable:true,enumerable:true,get:()=>decoder,
      set:value=>{decoder=value;trace.decoderWrites.push(value);}});
    const status=document.getElementById('status');
    trace.observer=new MutationObserver(()=>trace.domStatuses.push(status.textContent));
    trace.observer.observe(status,{childList:true,characterData:true,subtree:true});
  });
  await page.locator('#token').fill(server.token);
  await page.locator('#login button').click();
  await requested.promise;
  await page.waitForFunction(()=>window.negotiationWire.events.some(event=>event.type==='hello'),null,{timeout:5000});
  assert.equal(await page.evaluate(()=>window.gliss.metrics.frames),0);
  await page.evaluate(()=>{
    const trace=window.negotiationClose;
    trace.abandonedChain=window.gliss.chain;trace.closedSocket=window.gliss.socket;
    trace.closedSocket.close();
  });
  await page.waitForFunction(()=>window.negotiationClose.closedSocket.readyState===WebSocket.CLOSED&&
    document.getElementById('status').textContent==='Desktop disconnected',null,{timeout:5000});
  await page.waitForFunction(async()=>{
    const response=await fetch('/api/stats');
    return response.ok&&(await response.json()).active_clients===0;
  },null,{timeout:5000,polling:100});
  await page.evaluate(()=>{
    const trace=window.negotiationClose;
    trace.afterCloseWrites=trace.decoderWrites.length;trace.afterCloseStatuses=trace.domStatuses.length;
    trace.decoderAfterClose=window.gliss.decoder;trace.closedChain=window.gliss.chain;
  });
  release.resolve();
  await page.evaluate(async()=>{
    await Promise.all([window.negotiationClose.abandonedChain,window.negotiationClose.closedChain]);
    await new Promise(resolve=>requestAnimationFrame(resolve));
  });
  const result=await page.evaluate(()=>{
    const trace=window.negotiationClose,client=window.gliss;
    trace.observer.disconnect();
    return {lateDecoderInstalls:trace.decoderWrites.length-trace.afterCloseWrites,
      sameDecoder:client.decoder===trace.decoderAfterClose,hellos:trace.hellos,
      status:document.getElementById('status').textContent,
      lateStatuses:trace.domStatuses.slice(trace.afterCloseStatuses),frames:client.metrics.frames,errors:client.metrics.errors,
      source:window.glissSource(),closed:trace.closedSocket.readyState===WebSocket.CLOSED};
  });
  assert.deepEqual(result,{lateDecoderInstalls:0,sameDecoder:true,hellos:[],status:'Desktop disconnected',
    lateStatuses:[],frames:0,errors:0,source:'',closed:true});
  assert.equal(moduleRequests,1);
  assert.deepEqual(state.pageErrors,[]);
});
