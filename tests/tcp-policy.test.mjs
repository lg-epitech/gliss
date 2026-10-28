import test from 'node:test';
import assert from 'node:assert/strict';
import {startServer, launchBrowser, loginPage} from './helpers.mjs';
import {validateSocketPolicy} from '../test-support/browser/audio-policy-server.mjs';

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
  validateSocketPolicy(policy, true);assert.ok(policy.verified_readback_true > 0);
  await page.evaluate(() => window.gliss.dispose());await page.close();
});
