import test from 'node:test';
import assert from 'node:assert/strict';
import {startServer, launchBrowser, loginPage} from './helpers.mjs';

test('native HTTP/WebSocket default actually enables TCP_NODELAY before the first hello', {timeout: 20000}, async t => {
  // No tcp-nodelay argument: this covers the shipped default rather than an
  // operator override. The native hello contains actual accepted-socket readback.
  const server = await startServer({codec: 'h264', fps: 30});t.after(server.stop);
  const browser = await launchBrowser();t.after(() => browser.close());
  const page = await browser.newPage();
  await page.addInitScript(() => {
    window.helloSocketPolicy = null;
    window.WebSocket = new Proxy(WebSocket, {construct(Target, args) {
      const socket = new Target(...args);
      socket.addEventListener('message', event => {
        if (typeof event.data !== 'string') return;
        const message = JSON.parse(event.data);
        if (message.type === 'hello') window.helloSocketPolicy = message.tcp_listener;
      });
      return socket;
    }});
  });
  await loginPage(page, server);
  const policy = await page.evaluate(() => window.helloSocketPolicy);
  assert.equal(policy.requested_nodelay, true);
  assert.ok(policy.accepted_sockets > 0);
  assert.equal(policy.verified_sockets, policy.accepted_sockets);
  assert.equal(policy.verified_readback_true, policy.accepted_sockets);
  assert.equal(policy.verified_readback_false, 0);
  for (const name of ['rejected_sockets', 'initial_read_errors', 'set_errors',
    'readback_errors', 'readback_mismatches']) assert.equal(policy[name], 0, name);
  await page.evaluate(() => window.gliss.dispose());await page.close();
});
