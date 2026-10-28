import test from 'node:test';
import assert from 'node:assert/strict';
import {createServer, createConnection} from 'node:net';
import {once} from 'node:events';
import {startLimitedProxy} from '../test-support/browser/limited-proxy.mjs';

test('rate proxy preserves bytes and cancels pending deliveries before returning from stop', {timeout: 5000}, async t => {
  const peers = new Set();
  const server = createServer(socket => {peers.add(socket);socket.once('close', () => peers.delete(socket));socket.on('error', () => {});socket.on('data', data => socket.write(data));});
  server.listen(0, '127.0.0.1');await once(server, 'listening');
  t.after(async () => {for (const socket of peers) socket.destroy();await new Promise(resolve => server.close(resolve));});
  const proxy = await startLimitedProxy({targetPort: server.address().port, rttMs: 10, mbps: 1});
  t.after(() => proxy.stop());
  const socket = createConnection(Number(new URL(proxy.url).port), '127.0.0.1');socket.on('error', () => {});
  await once(socket, 'connect');
  const received = once(socket, 'data');socket.write(Buffer.from('bounded echo'));
  assert.equal((await received)[0].toString(), 'bounded echo');
  const before = proxy.stats.receivedBytes;
  socket.write(Buffer.alloc(65536));
  const deadline = performance.now() + 500;
  while (proxy.stats.receivedBytes === before && performance.now() < deadline)
    await new Promise(resolve => setTimeout(resolve, 5));
  assert.ok(proxy.stats.receivedBytes > before, 'The proxy must receive the queued payload before cancellation');
  assert.ok(proxy.stats.pendingTimers > 0, 'Cancellation must exercise an active delivery timer');
  // Cancelling a queued stream can reset its peer; close is still required.
  const closed = new Promise(resolve => socket.once('close', resolve));
  await proxy.stop();
  await closed;
  assert.equal(proxy.stats.pendingTimers, 0);assert.equal(socket.destroyed || socket.readableEnded, true);
  assert.equal(proxy.stats.overflows, 0);
});

test('rate proxy denies invalid bounds before opening a listener', async () => {
  for (const config of [{targetPort: 0}, {mbps: 0}, {rttMs: -1}, {maxQueueBytes: 1048577}])
    await assert.rejects(startLimitedProxy({targetPort: 5900, mbps: 1, rttMs: 0, ...config}));
});
