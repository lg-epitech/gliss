// A bounded TCP byte-rate/delay model for owned loopback fixtures. No packet loss
// or kernel network configuration is involved. stop() cancels queued deliveries.
import {createServer, createConnection} from 'node:net';
import {performance} from 'node:perf_hooks';

export async function startLimitedProxy({targetPort, rttMs, mbps, maxQueueBytes = 1048576}) {
  if (!Number.isInteger(targetPort) || targetPort < 1 || targetPort > 65535 ||
      !Number.isFinite(rttMs) || rttMs < 0 || rttMs > 1000 ||
      !Number.isFinite(mbps) || mbps <= 0 || mbps > 1000 ||
      !Number.isInteger(maxQueueBytes) || maxQueueBytes < 1 || maxQueueBytes > 1048576)
    throw new Error('Invalid owned proxy configuration');
  const sockets = new Set(), directions = new Set();
  const stats = {receivedBytes: 0, forwardedBytes: 0, peakQueueBytes: 0, overflows: 0, pendingTimers: 0};
  let stopped = false;
  const cancel = direction => {
    if (direction.timer !== null) {clearTimeout(direction.timer);direction.timer = null;stats.pendingTimers--;}
    direction.queue.length = 0;direction.bytes = 0;directions.delete(direction);
  };
  const server = createServer(client => {
    if (stopped) {client.destroy();return;}
    const upstream = createConnection({host: '127.0.0.1', port: targetPort});
    const owned = [];
    const close = () => {
      for (const direction of owned) cancel(direction);
      client.destroy();upstream.destroy();
    };
    for (const socket of [client, upstream]) {
      sockets.add(socket);socket.setNoDelay(true);
      socket.once('close', () => {sockets.delete(socket);close();});socket.on('error', close);
    }
    for (const [from, to] of [[client, upstream], [upstream, client]]) {
      const direction = {queue: [], bytes: 0, timer: null, next: 0, blocked: false};
      owned.push(direction);directions.add(direction);
      const arm = () => {
        if (stopped || to.destroyed || direction.timer !== null || direction.blocked || !direction.queue.length) return;
        stats.pendingTimers++;
        direction.timer = setTimeout(() => {
          direction.timer = null;stats.pendingTimers--;
          if (stopped || to.destroyed) return;
          // Timers can wake early; never forward before the absolute due time.
          const head = direction.queue[0];
          if (performance.now() < head.due) {arm();return;}
          direction.queue.shift();direction.bytes -= head.data.length;
          stats.forwardedBytes += head.data.length;
          direction.blocked = !to.write(head.data);
          arm();
        }, Math.max(0, Math.ceil(direction.queue[0].due - performance.now())));
      };
      to.on('drain', () => {direction.blocked = false;arm();});
      from.on('data', data => {
        if (stopped || to.destroyed) return;
        stats.receivedBytes += data.length;
        if (direction.bytes + data.length > maxQueueBytes) {stats.overflows++;close();return;}
        direction.bytes += data.length;
        stats.peakQueueBytes = Math.max(stats.peakQueueBytes, direction.bytes);
        direction.next = Math.max(performance.now() + rttMs / 2, direction.next) + data.length * 8 / (mbps * 1000);
        direction.queue.push({data, due: direction.next});arm();
      });
    }
  });
  await new Promise((resolve, reject) => {server.once('error', reject);server.listen(0, '127.0.0.1', resolve);});
  return {url: `http://127.0.0.1:${server.address().port}`, stats,
    async stop() {
      if (stopped) return;
      stopped = true;
      for (const direction of [...directions]) cancel(direction);
      const closed = [...sockets].map(socket => new Promise(resolve => {socket.once('close', resolve);socket.destroy();}));
      await Promise.all([...closed, new Promise(resolve => server.close(resolve))]);
      if (stats.pendingTimers || sockets.size || directions.size) throw new Error('Owned proxy cleanup incomplete');
    },
  };
}
