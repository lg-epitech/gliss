// Execute the real client path with a deliberately stalled video decode.
import test from 'node:test';
import assert from 'node:assert/strict';
import {GlissClient} from '../web/client.js';

test('control reply dispatch does not wait for an unrelated video decoder', async t => {
  const names = ['window','document','location','WebSocket','requestAnimationFrame'];
  const saved = new Map(names.map(name => [name,Object.getOwnPropertyDescriptor(globalThis,name)]));
  class Socket {
    static OPEN = 1;readyState = 1;sent = [];
    send(value) {this.sent.push(JSON.parse(value));}
    close() {this.readyState = 3;}
  }
  globalThis.window = {addEventListener() {}};globalThis.document = {addEventListener() {}};
  globalThis.location = {pathname:'/',protocol:'http:',host:'127.0.0.1'};
  globalThis.WebSocket = Socket;globalThis.requestAnimationFrame = callback => {callback(0);return 1;};
  const events = [], canvas = {width:32,height:32,addEventListener() {},getContext:() => ({drawImage() {}})};
  const client = new GlissClient(canvas,() => {},event => events.push(event));
  let release;
  try {
    await client.connect();
    client.socket.onmessage({data:JSON.stringify({type:'hello',codec:'h264'})});await client.chain;
    let entered;
    const ready = new Promise(resolve => {entered = resolve;});
    const held = new Promise(resolve => {release = resolve;});
    client.decoder.decode = () => {entered();return held;};
    client.socket.onmessage({data:new ArrayBuffer(24)});await ready;
    client.socket.onmessage({data:JSON.stringify({type:'audio_status',available:false,enabled:false})});
    client.socket.onmessage({data:JSON.stringify({type:'pong',id:17})});
    await new Promise(setImmediate);
    assert.ok(events.some(event => event.type === 'audio_status'),'The current socket and direct control path must be active');
    assert.ok(events.some(event => event.type === 'pong' && event.id === 17),'A pong must be dispatched before the stalled video is released');
    assert.equal(client.metrics.frames,0);
  } finally {
    release?.({width:32,height:32,sequence:1,inputId:0,bytes:24,videoFrame:{close() {}}});
    await client.chain;client.dispose();
    for (const [name,value] of saved) if (value) Object.defineProperty(globalThis,name,value);else delete globalThis[name];
  }
});
