import test from 'node:test';
import assert from 'node:assert/strict';
import {createServer} from 'node:http';
import {once} from 'node:events';
import {createRequire} from 'node:module';
import {startIsolationRelay} from '../test-support/audio-isolation/relay.mjs';
import {freePort} from './helpers.mjs';
const require=createRequire(import.meta.url);
const {ws:WebSocket,wsServer:WebSocketServer}=require('playwright-core/lib/utilsBundle');

async function fixture(t,mode) {
  const native=createServer(),wss=new WebSocketServer({noServer:true,perMessageDeflate:false});
  let source;const inputs=[],main=[],companion=[];
  native.on('upgrade',(req,socket,head) => {
    assert.equal(req.headers.cookie,'fixture=owned');
    wss.handleUpgrade(req,socket,head,peer => {source=peer;peer.on('error',()=>{});
      peer.on('message',data => inputs.push(JSON.parse(data)));
      peer.send(JSON.stringify({type:'hello',codec:'h264'}));});
  });
  await new Promise(resolve=>native.listen(0,'127.0.0.1',resolve));
  const relay=await startIsolationRelay({port:await freePort(),targetPort:native.address().port,mode});
  const open=role => {const ws=new WebSocket(relay.url.replace('http:','ws:')+(role==='main'?'/ws':'/audio'),
    {headers:{Origin:relay.url,Cookie:'fixture=owned'},perMessageDeflate:false});
    ws.on('message',(data,binary)=>(role==='main'?main:companion).push({data:Buffer.from(data),binary}));ws.on('error',()=>{});return ws;};
  const a=open('main'),b=open('companion');
  t.after(async()=>{await relay.stop();for(const ws of [a,b,source])ws?.terminate();
    await Promise.all([new Promise(resolve=>native.close(resolve)),new Promise(resolve=>wss.close(resolve))]);});
  await Promise.all([once(a,'open'),once(b,'open')]);
  const until=async condition => {const end=performance.now()+2000;
    while(!condition()&&performance.now()<end)await new Promise(resolve=>setTimeout(resolve,5));assert.ok(condition());};
  await until(()=>source?.readyState===WebSocket.OPEN&&main.length===1);
  return {relay,source,inputs,main,companion,a,b,until};
}

for (const mode of ['combined','split']) test(`isolation prototype: ${mode} routes exact media and forwards only real client ACKs`,async t=>{
  const f=await fixture(t,mode),audio=Buffer.alloc(24),video=Buffer.alloc(24);
  audio.write('GLA1');video.write('GLV1');video.writeUInt32LE(17,4);
  f.source.send(audio);f.source.send(video);f.source.send(JSON.stringify({type:'audio_status',available:true}));
  await f.until(()=>f.main.length+f.companion.length===4);
  const sound=mode==='split'?f.companion:f.main;
  assert.ok(sound.some(row=>row.binary&&row.data.equals(audio)));
  assert.ok(sound.some(row=>!row.binary&&JSON.parse(row.data).type==='audio_status'));
  assert.ok(f.main.some(row=>row.binary&&row.data.equals(video)));
  assert.equal(f.inputs.length,0,'Relay must never fabricate display credit');
  f.a.send(JSON.stringify({type:'ack',sequence:17}));
  (mode==='split'?f.b:f.a).send(JSON.stringify({type:'audio',enabled:true,codec:'opus'}));
  await f.until(()=>f.inputs.length===2);
  assert.deepEqual(f.inputs,[{type:'ack',sequence:17},{type:'audio',enabled:true,codec:'opus'}]);
  const stats=f.relay.snapshot();assert.equal(stats.native_sessions,1);assert.equal(stats.browser_sockets,2);
  assert.notEqual(stats.identities[0].remote_port,stats.identities[1].remote_port);
  assert.equal(stats.counters.main_video,1);assert.equal(stats.counters[mode==='split'?'companion_audio':'main_audio'],1);
  assert.equal(stats.failure,null);
});

test('isolation prototype: closes both paths instead of retaining an oversized video queue',async t=>{
  const f=await fixture(t,'split');f.source.send(Buffer.alloc(4*1024*1024+1));
  await f.until(()=>f.relay.snapshot().failure==='bounded-buffer-overflow');
  assert.equal(f.relay.snapshot().counters.overflows,1);
  await f.until(()=>f.a.readyState===WebSocket.CLOSED&&f.b.readyState===WebSocket.CLOSED&&f.source.readyState===WebSocket.CLOSED);
});

test('isolation prototype: companion rejects display ACKs and tears down the single source',async t=>{
  const f=await fixture(t,'split');f.b.send(JSON.stringify({type:'ack',sequence:17}));
  await f.until(()=>f.relay.snapshot().failure==='unexpected-companion-control');
  assert.equal(f.inputs.length,0);
  await f.until(()=>f.a.readyState===WebSocket.CLOSED&&f.source.readyState===WebSocket.CLOSED);
});
