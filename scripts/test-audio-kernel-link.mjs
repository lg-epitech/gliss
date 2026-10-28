// Opt-in: run in a fresh user/net namespace with CAP_NET_ADMIN and host netns ID.
import test from 'node:test';
import assert from 'node:assert/strict';
import {createConnection, createServer} from 'node:net';
import {once} from 'node:events';
import {performance} from 'node:perf_hooks';
import {startKernelLink} from '../test-support/browser/audio-kernel-link.mjs';

test('kernel audio link delays only the declared port and removes its queues', {timeout: 10000}, async t => {
  const sockets = new Set(), servers = [];
  t.after(async () => {
    for (const socket of sockets) socket.destroy();
    for (const server of servers) await new Promise(resolve => server.close(resolve));
  });
  for (let i = 0; i < 2; i++) {
    const server = createServer(socket => {
      sockets.add(socket);socket.once('close', () => sockets.delete(socket));socket.on('error', () => {});
      socket.setNoDelay(true);socket.on('data', data => socket.write(data));
    });
    server.listen(0, '127.0.0.1');await once(server, 'listening');servers.push(server);
  }
  const link = await startKernelLink({targetPort: servers[0].address().port, rttMs: 80, mbps: 1});
  t.after(() => link.stop());
  const elapsed = [];
  for (const server of servers) {
    const socket = createConnection(server.address().port, '127.0.0.1');socket.setNoDelay(true);
    socket.on('error', () => {});sockets.add(socket);socket.once('close', () => sockets.delete(socket));
    await once(socket, 'connect');
    const begin = performance.now(), received = once(socket, 'data');
    const payload = Buffer.alloc(256, 93);socket.write(payload);
    assert.deepEqual((await received)[0], payload);elapsed.push(performance.now() - begin);
    socket.destroy();
  }
  assert.ok(elapsed[0] >= 75 && elapsed[0] < 500, 'Shaped echo must traverse both 40 ms queues');
  assert.ok(elapsed[1] < 60, 'Unrelated port must bypass the declared link');
  const measured = await link.snapshot();
  assert.ok(measured.queues.every(row => row.packets > 0 && row.drops === 0));
  await link.stop();assert.equal(link.cleanup.root_removed, true);
  t.diagnostic(JSON.stringify({rtt_ms: {shaped: elapsed[0], bypass: elapsed[1]}, measured, root_removed: true}));
});
