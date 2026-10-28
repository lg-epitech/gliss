// Independent crop of the owned guest's RFB surface, for readiness diagnostics.
import assert from 'node:assert/strict';
import {createConnection} from 'node:net';
import {once} from 'node:events';

export async function rawStamp(address) {
  assert.match(address, /^127\.0\.0\.1:[0-9]+$/);
  const socket = createConnection({host:'127.0.0.1',port:Number(address.split(':')[1])});
  let buffer = Buffer.alloc(0), waiting = null, failure;
  const flush = () => {
    if (!waiting) return;
    if (failure) {waiting.reject(failure);waiting = null;return;}
    if (buffer.length >= waiting.length) {
      const value = buffer.subarray(0,waiting.length);buffer = buffer.subarray(waiting.length);
      waiting.resolve(value);waiting = null;
    }
  };
  socket.on('data', value => {buffer = Buffer.concat([buffer,value]);if (buffer.length > 2097152) failure = new Error('Raw crop buffer cap');flush();});
  socket.on('error', error => {failure = error;flush();});
  socket.on('close', () => {failure ??= new Error('Raw crop closed');flush();});
  const take = length => {assert.equal(waiting,null);return new Promise((resolve,reject) => {waiting = {length,resolve,reject};flush();});};
  const timeout = setTimeout(() => socket.destroy(new Error('Raw crop timeout')),3000);
  try {
    await once(socket,'connect');socket.setNoDelay(true);
    assert.equal((await take(12)).toString(),'RFB 003.008\n');socket.write('RFB 003.008\n');
    const n = (await take(1))[0];assert.ok(n > 0 && n <= 32);assert.ok((await take(n)).includes(1));
    socket.write(Buffer.from([1]));assert.equal((await take(4)).readUInt32BE(),0);socket.write(Buffer.from([1]));
    const init = await take(24), width = init.readUInt16BE(0), height = init.readUInt16BE(2), nameLength = init.readUInt32BE(20);
    assert.ok(width >= 400 && height >= 40 && width <= 4096 && height <= 2160 && nameLength <= 4096);await take(nameLength);
    socket.write(Buffer.from([0,0,0,0,32,24,0,1,0,255,0,255,0,255,16,8,0,0,0,0]));
    socket.write(Buffer.from([2,0,0,1,0,0,0,0]));
    const request = Buffer.from([3,0,0,0,0,0,1,144,0,40]);socket.write(request);
    const update = await take(4);assert.equal(update[0],0);const count = update.readUInt16BE(2);assert.ok(count > 0 && count <= 16);
    const colors = [];let received = 0;
    for (let i = 0; i < count; i++) {
      const header = await take(12), x = header.readUInt16BE(0), y = header.readUInt16BE(2), w = header.readUInt16BE(4), h = header.readUInt16BE(6);
      assert.equal(header.readInt32BE(8),0);received += w*h*4;assert.ok(w > 0 && h > 0 && received <= 8388608);
      for (let row = 0; row < h; row++) {
        const pixels = await take(w*4);
        if (y+row !== 18) continue;
        for (const px of [18,37,277,370]) if (px >= x && px < x+w) {
          const at = (px-x)*4;colors.push({x:px,rgba:[pixels[at+2],pixels[at+1],pixels[at],255]});
        }
      }
    }
    return {width,height,colors};
  } finally {clearTimeout(timeout);if (!socket.closed) {const closed = new Promise(resolve => socket.once('close',resolve));socket.destroy();await closed;}}
}
