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

// A client whose video decode can be held, as a hidden tab holds a frame that waits to be painted.
async function stalledClient(t) {
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
  const events = [], statuses = [], canvas = {width:32,height:32,addEventListener() {},getContext:() => ({drawImage() {}})};
  const client = new GlissClient(canvas,status => statuses.push(status),event => events.push(event));
  let release;
  t.after(async () => {
    release?.({width:32,height:32,sequence:1,inputId:0,bytes:24,videoFrame:{close() {}}});
    await client.chain;client.dispose();
    for (const [name,value] of saved) if (value) Object.defineProperty(globalThis,name,value);else delete globalThis[name];
  });
  await client.connect();
  const say = message => client.socket.onmessage({data:typeof message === 'string' ? message : JSON.stringify(message)});
  say({type:'hello',codec:'h264'});await client.chain;
  let entered;
  const ready = new Promise(resolve => {entered = resolve;});
  const held = new Promise(resolve => {release = resolve;});
  client.decoder.decode = () => {entered();return held;};
  client.socket.onmessage({data:new ArrayBuffer(24)});await ready;
  events.length = 0;
  return {client,events,statuses,say,finish: async () => {
    release({width:32,height:32,sequence:1,inputId:0,bytes:24,videoFrame:{close() {}}});release = null;
    await client.chain;
  }};
}

test('events that are not about the picture are delivered while a frame waits, however many arrive', async t => {
  const {client,events,say,finish} = await stalledClient(t);
  const kinds = [
    {type:'cursor',width:1,height:1,hotspot_x:0,hotspot_y:0,pixels:[1,2,3,255]},
    {type:'clipboard',text:'copied'},{type:'bell'},
    {type:'resize_result',width:640,height:480,status:0},
    {type:'input_error',message:'refused'},{type:'stream_stats',generation:0},
  ];
  const settled = client.chain;
  for (let round = 0; round < 500; round++) for (const message of kinds) say({...message,round});
  // Nothing was queued behind the frame: the chain is the one the frame is on, and every event is already out.
  assert.equal(client.chain,settled,'A message that needs no ordering must not extend the video chain');
  assert.equal(events.length,500 * kinds.length);
  assert.deepEqual(events.slice(-kinds.length),kinds.map(message => ({...message,round:499})));
  assert.equal(client.metrics.frames,0);
  await finish();
  assert.equal(events.at(-1).type,'frame');
});

test('hello and error keep their place among the frames around them', async t => {
  const {client,events,say,finish} = await stalledClient(t);
  say({type:'error',message:'The picture is paused.'});
  say({type:'hello',codec:'next'});
  say({type:'bell'});
  await new Promise(setImmediate);
  assert.deepEqual(events.map(event => event.type),['bell']);
  assert.equal(client.codec,'h264','hello applies to the frames after it, not to the one being decoded');
  await finish();
  assert.deepEqual(events.map(event => event.type),['bell','frame','error','hello']);
  assert.equal(client.codec,'next');
});

test('a malformed or rejected message is reported at once and does not stop later ones', async t => {
  const {client,events,statuses,say} = await stalledClient(t);
  say('{not json');
  say('null');
  client.onEvent = event => {if (event.type === 'bell') throw new Error('handler failed');events.push(event);};
  say({type:'bell'});
  say({type:'clipboard',text:'after'});
  assert.equal(client.metrics.errors,3);
  assert.equal(statuses.length,3);
  assert.ok(statuses.at(-1).includes('handler failed'));
  assert.deepEqual(events,[{type:'clipboard',text:'after'}]);
});
