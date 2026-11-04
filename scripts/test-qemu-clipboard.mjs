// Opt-in Linux/KVM smoke test. Boots its own diskless guest with a small SPICE
// agent; no existing VM, disk, libvirt definition or spice-vdagent is used.
// Run after cargo build: GLISS_BINARY="$PWD/target/release/gliss-server" \
//   node scripts/test-qemu-clipboard.mjs
// Prerequisites: QEMU with qemu-vdagent, a readable kernel with built-in virtio-pci
// and virtio-console, static glibc, cpio and Playwright.
// GLISS_QEMU_KERNEL and GLISS_QEMU_ACCEL=tcg override the kernel and accelerator.
import test from 'node:test';
import assert from 'node:assert/strict';
import {startServer, launchBrowser, loginPage} from '../tests/helpers.mjs';
import {startClipboardVm} from '../test-support/qemu-clipboard/fixture.mjs';

const pastes = [
  ['plain text', 'hello', 'hello'],
  ['LF line endings', 'one\ntwo\n', 'one\ntwo\n'],
  ['CRLF line endings', 'one\r\ntwo\r\n', 'one\ntwo\n'],
  ['Unicode', 'Ελληνικά 日本語 🚀\ncafé', 'Ελληνικά 日本語 🚀\ncafé'],
  ['several agent chunks', 'é\r\n'.repeat(8192), 'é\n'.repeat(8192)],
  ['empty text', '', ''],
];
const copies = [
  ['Unicode with CRLF', 'guest: 日本語\r\n🙂', 'guest: 日本語\n🙂'],
  ['LF', 'one\ntwo', 'one\ntwo'],
  ['a trailing NUL', 'text\0', 'text'],
];

for (const name of [undefined, 'clipboard-smoke']) {
  test(`real QEMU ${name ? `named ${name}` : 'without a name'} ↔ qemu-vdagent ↔ guest agent: exact bytes`, {timeout: 60000}, async t => {
    const vm = await startClipboardVm(t, {name});
    const server = await startServer({vnc: vm.vnc});t.after(server.stop);
    const browser = await launchBrowser();t.after(() => browser.close());
    const page = await browser.newPage();await loginPage(page, server);
    const paste = async text => {
      const received = vm.nextPaste();
      await page.evaluate(value => window.gliss.send({type: 'clipboard', text: value}), text);
      return received;
    };
    // Extended clipboard capabilities arrive just after login; before that Gliss uses base messages.
    await paste('capabilities');
    for (const [label, text, expected] of pastes) {
      assert.deepEqual(await paste(text), Buffer.from(expected), label);
    }
    for (const [label, text, expected] of copies) {
      vm.copy(Buffer.from(text));
      await page.waitForFunction(value => document.getElementById('cliptext').value === value, expected, {timeout: 5000})
        .catch(error => assert.fail(`${label}: ${error.message}`));
    }
  });
}
