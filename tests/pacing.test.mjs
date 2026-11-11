// The frame-rate cap drops frames from a faster source; it must never drop the last one.
import test from 'node:test';
import assert from 'node:assert/strict';
import {createServer} from 'node:net';
import {once} from 'node:events';
import {startServer, launchBrowser, loginPage} from './helpers.mjs';

const WIDTH = 96, HEIGHT = 64;

// An RFB peer that answers each update request with the next queued solid colour and
// leaves the request pending once the queue is empty.
async function startColourSource() {
  const queue = [[10, 20, 30]];
  let pending = false, socket;
  const answer = () => {
    if (!pending || !queue.length || !socket) return;
    const [red, green, blue] = queue.shift(), update = Buffer.alloc(16 + WIDTH * HEIGHT * 4);
    update.writeUInt16BE(1, 2);update.writeUInt16BE(WIDTH, 8);update.writeUInt16BE(HEIGHT, 10);
    for (let offset = 16; offset < update.length; offset += 4) update.set([red, green, blue, 0], offset);
    pending = false;socket.write(update);
  };
  const server = createServer(async connection => {
    socket = connection;connection.setNoDelay(true);connection.on('error', () => {});
    let buffer = Buffer.alloc(0), wake;
    connection.on('data', data => {buffer = Buffer.concat([buffer, data]);wake?.();});
    const read = async count => {
      while (buffer.length < count) await new Promise(resolve => {wake = resolve;});
      const data = buffer.subarray(0, count);buffer = buffer.subarray(count);return data;
    };
    connection.write('RFB 003.008\n');await read(12);
    connection.write(Buffer.from([1, 1]));await read(1);connection.write(Buffer.alloc(4));await read(1);
    const init = Buffer.alloc(24 + 6);
    init.writeUInt16BE(WIDTH, 0);init.writeUInt16BE(HEIGHT, 2);
    init.set([32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0], 4);
    init.writeUInt32BE(6, 20);init.write('colour', 24);
    connection.write(init);
    for (;;) {
      const type = (await read(1))[0];
      if (type === 0) await read(19);
      else if (type === 2) await read((await read(3)).readUInt16BE(1) * 4);
      else if (type === 3) {await read(9);pending = true;answer();}
      else if (type === 4) await read(7);
      else if (type === 5) await read(5);
      else throw new Error(`Unexpected RFB message ${type}`);
    }
  });
  server.listen(0, '127.0.0.1');await once(server, 'listening');
  return {address: `127.0.0.1:${server.address().port}`, push: (...colours) => {queue.push(...colours);answer();},
    stop: () => new Promise(resolve => {socket?.destroy();server.close(resolve);})};
}

test('frame-rate cap: a burst faster than the cap loses a middle frame and ends on its last', {timeout: 30000}, async t => {
  const source = await startColourSource();t.after(source.stop);
  // One frame per second; captures are requested every 500 ms at most.
  const server = await startServer({vnc: source.address, fps: 1});t.after(server.stop);
  const browser = await launchBrowser();t.after(() => browser.close());
  const page = await browser.newPage();
  await loginPage(page, server);
  await page.evaluate(() => {
    window.painted = [];
    const forward = window.gliss.onEvent;
    window.gliss.onEvent = event => {
      forward(event);
      if (event.type === 'frame') window.painted.push(Array.from(window.gliss.ctx.getImageData(10, 10, 1, 1).data.subarray(0, 3)).join());
    };
  });
  // Let the cap's schedule go idle, then offer eight captures as fast as the server
  // asks: 3.5 s of captures, in which the cap admits five at most.
  await new Promise(resolve => setTimeout(resolve, 2500));
  const burst = [[200, 0, 0], [0, 200, 0], [0, 0, 200], [200, 200, 0], [0, 200, 200], [200, 0, 200], [90, 90, 90], [250, 120, 0]];
  source.push(...burst);
  const last = burst.at(-1).join();
  await page.waitForFunction(colour => window.painted.at(-1) === colour, last, {timeout: 15000});
  // Nothing follows the last frame, and the cap admitted fewer frames than were captured.
  await new Promise(resolve => setTimeout(resolve, 1500));
  const painted = await page.evaluate(() => window.painted);
  assert.equal(painted.at(-1), last);
  assert.equal(painted[0], burst[0].join(), 'the burst starts with its first frame');
  assert.ok(painted.length < burst.length, `the cap dropped nothing: ${painted}`);
  assert.equal(new Set(painted).size, painted.length, `a frame was painted twice: ${painted}`);
});
