// Research fixture only: one authenticated native session, two browser sockets.
// Both experiment arms traverse this relay. It never fabricates a video ACK.
import assert from 'node:assert/strict';
import {createServer, request as httpRequest} from 'node:http';
import {createRequire} from 'node:module';
const require = createRequire(import.meta.url);
const {ws: WebSocket, wsServer: WebSocketServer} = require('playwright-core/lib/utilsBundle');
const MAX_BUFFER = 4 * 1024 * 1024;

export async function startIsolationRelay({port, targetPort, mode}) {
  assert.ok(['combined','split'].includes(mode));
  for (const value of [port,targetPort]) assert.ok(Number.isInteger(value) && value > 0 && value <= 65535);
  assert.notEqual(port,targetPort);
  const url = `http://127.0.0.1:${port}`, target = `http://127.0.0.1:${targetPort}`;
  const sockets = new Set(), peers = new Map(), identities = [], requests = new Set();
  const counters = {upstream_audio:0,upstream_video:0,main_audio:0,companion_audio:0,main_video:0,
    main_pongs:0,companion_pongs:0,overflows:0,protocol_errors:0};
  let upstream, cookie, stopping, closed = false, failure = null, upgrades = 0;
  const fail = code => {failure ??= code;closePeers();};
  function closePeers() {
    for (const peer of peers.values()) peer.terminate();
    upstream?.terminate();
  }
  function send(peer, data, binary) {
    if (!peer || peer.readyState !== WebSocket.OPEN) {fail('destination-not-open');return;}
    if (peer.bufferedAmount + data.length > MAX_BUFFER) {counters.overflows++;fail('bounded-buffer-overflow');return;}
    peer.send(data,{binary},error => {if (error && !closed) fail('send-error');});
  }
  const server = createServer((req,res) => {
    if (closed || requests.size >= 8) {res.writeHead(503).end();return;}
    // Fixed loopback destination; paths are forwarded, never interpreted as URLs.
    const outgoing = httpRequest(target + req.url, {agent:false,method:req.method,headers:{...req.headers,host:`127.0.0.1:${targetPort}`}}, incoming => {
      res.writeHead(incoming.statusCode,incoming.headers);incoming.pipe(res);
    });
    requests.add(outgoing);outgoing.setTimeout(5000,() => outgoing.destroy());
    outgoing.on('error',() => {if (!res.headersSent) res.writeHead(502);res.end();});
    outgoing.on('close',() => requests.delete(outgoing));
    req.on('aborted',() => outgoing.destroy());req.pipe(outgoing);
  });
  server.on('connection',socket => {
    if (closed || sockets.size >= 16) {socket.destroy();return;}
    sockets.add(socket);socket.setNoDelay(true);socket.on('error',() => {});
    socket.once('close',() => sockets.delete(socket));
  });
  const wss = new WebSocketServer({noServer:true,perMessageDeflate:false,maxPayload:65536});
  server.on('upgrade',(req,socket,head) => {
    const role = req.url === '/ws' ? 'main' : req.url === '/audio' ? 'companion' : null;
    if (closed || !role || req.headers.origin !== url || !req.headers.cookie || upgrades >= 2 || peers.has(role) ||
        (cookie && req.headers.cookie !== cookie)) {socket.destroy();return;}
    cookie ??= req.headers.cookie;upgrades++;
    wss.handleUpgrade(req,socket,head,peer => {
      peers.set(role,peer);peer.on('error',() => {if (!closed) fail('peer-error');});
      identities.push({role,local_port:socket.localPort,remote_port:socket.remotePort,
        loopback:socket.remoteAddress === '127.0.0.1',nodelay_requested:true});
      peer.once('close',() => {if (!closed) closePeers();});
      peer.on('message',(data,binary) => {
        if (binary || data.length > 65536) {counters.protocol_errors++;fail('unexpected-browser-message');return;}
        let value;try {value=JSON.parse(data);} catch {counters.protocol_errors++;fail('invalid-browser-json');return;}
        if (role === 'companion' && (mode !== 'split' || value.type !== 'audio')) {
          counters.protocol_errors++;fail('unexpected-companion-control');return;
        }
        if (role === 'main' && mode === 'split' && value.type === 'audio') {
          counters.protocol_errors++;fail('audio-control-on-main');return;
        }
        send(upstream,data,false);
      });
      if (peers.size !== 2) return;
      // Native cookie and Origin validation remain active on the actual upstream.
      upstream = new WebSocket(target.replace('http:','ws:')+'/ws', {headers:{Cookie:cookie,Origin:url},
        perMessageDeflate:false,maxPayload:16*1024*1024+24,handshakeTimeout:5000});
      upstream.on('error',() => {if (!closed) fail('upstream-error');});
      upstream.once('close',() => {if (!closed) closePeers();});
      upstream.on('message',(data,binary) => {
        let audio = binary && data.length >= 20 && data.readUInt32LE(0) === 0x31414c47;
        if (audio) counters.upstream_audio++;
        else if (binary) counters.upstream_video++;
        else {
          let value;try {value=JSON.parse(data);} catch {fail('invalid-native-json');return;}
          audio = value.type === 'audio_status';
          if (value.type === 'pong') counters.main_pongs++;
        }
        const destination = audio && mode === 'split' ? 'companion' : 'main';
        if (binary) {
          if (audio) counters[destination === 'main' ? 'main_audio' : 'companion_audio']++;
          else counters.main_video++;
        }
        send(peers.get(destination),data,binary);
      });
    });
  });
  await new Promise((resolve,reject) => {server.once('error',reject);server.listen(port,'127.0.0.1',resolve);});
  const snapshot = () => ({mode,failure,counters:{...counters},identities:identities.map(row => ({...row})),
    native_sessions:upstream ? 1 : 0,browser_sockets:peers.size,
    queued_main_bytes:peers.get('main')?.bufferedAmount ?? 0,queued_audio_bytes:peers.get('companion')?.bufferedAmount ?? 0});
  return {url,port,snapshot,stop:() => stopping ??= (async () => {
    closed=true;
    const all=[...sockets], ended=all.map(socket => new Promise(resolve => {
      if (socket.destroyed) resolve();else {socket.once('close',resolve);socket.destroy();}
    }));
    for (const request of requests) request.destroy();
    closePeers();
    const upstreamClosed = !upstream || upstream.readyState === WebSocket.CLOSED ? Promise.resolve() :
      new Promise(resolve => upstream.once('close',resolve));
    await Promise.all([...ended,upstreamClosed,new Promise(resolve => server.close(resolve)),new Promise(resolve => wss.close(resolve))]);
    assert.equal(sockets.size,0);assert.equal(requests.size,0);
  })()};
}
