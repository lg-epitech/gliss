import assert from 'node:assert/strict';
import {spawn, execFileSync} from 'node:child_process';
import {mkdtemp, rm, access} from 'node:fs/promises';
import {existsSync, openSync, closeSync} from 'node:fs';
import {tmpdir, release} from 'node:os';
import {join} from 'node:path';
import {fileURLToPath} from 'node:url';
import {freePort} from '../../tests/helpers.mjs';

// Linux-only, opt-in fixture: it creates and kills only its own diskless VM.
// `name` is QEMU's -name option; omit it to test the unnamed server.
export async function startClipboardVm(t, {name} = {}) {
  const directory = await mkdtemp(join(tmpdir(), 'gliss-clipboard-smoke-'));
  t.after(() => rm(directory, {recursive: true, force: true}));
  const kernel = process.env.GLISS_QEMU_KERNEL ||
    [`/boot/vmlinuz-${release()}`, '/boot/vmlinuz-linux'].find(existsSync);
  assert.ok(kernel, 'Set GLISS_QEMU_KERNEL to a readable kernel');
  await access(kernel);
  execFileSync('gcc', ['-O2', '-static', '-o', join(directory, 'init'),
    fileURLToPath(new URL('./init.c', import.meta.url))]);
  const initramfs = openSync(join(directory, 'initramfs.cpio'), 'w');
  try {
    execFileSync('cpio', ['-o', '--format=newc'], {cwd: directory, input: 'init\n',
      stdio: ['pipe', initramfs, 'ignore']});
  } finally {
    closeSync(initramfs);
  }
  const port = await freePort();
  assert.ok(port >= 5900);
  const tcg = process.env.GLISS_QEMU_ACCEL === 'tcg';
  const qemu = spawn('qemu-system-x86_64', ['-machine', `pc,accel=${tcg ? 'tcg' : 'kvm'}`,
    '-cpu', tcg ? 'max' : 'host', '-m', '256', '-nodefaults', '-no-reboot', ...(name ? ['-name', name] : []),
    '-device', 'VGA', '-device', 'virtio-serial-pci',
    '-chardev', 'qemu-vdagent,id=agent,name=vdagent,clipboard=on',
    '-device', 'virtserialport,chardev=agent,name=com.redhat.spice.0',
    '-vnc', `127.0.0.1:${port - 5900}`, '-kernel', kernel,
    '-initrd', join(directory, 'initramfs.cpio'), '-append', 'console=ttyS0 quiet loglevel=0 panic=0',
    '-serial', 'stdio', '-monitor', 'none'], {stdio: ['pipe', 'pipe', 'pipe']});
  let log = '', failure;
  const pastes = [], waiters = [];
  qemu.on('error', error => {failure = error;});
  qemu.stderr.on('data', data => {log = (log + data).slice(-8192);});
  let pending = '';
  qemu.stdout.on('data', data => {
    log = (log + data).slice(-8192);
    pending += data.toString('latin1');
    for (let end; (end = pending.indexOf('\n')) >= 0; pending = pending.slice(end + 1)) {
      const match = pending.slice(0, end).match(/GLISS_PASTE ([0-9a-f]*)\s*$/);
      if (!match) continue;
      pastes.push(Buffer.from(match[1], 'hex'));
      waiters.shift()?.();
    }
  });
  t.after(async () => {
    if (qemu.exitCode !== null || !qemu.pid) return;
    await new Promise(resolve => {
      const timer = setTimeout(() => qemu.kill('SIGKILL'), 2000);
      qemu.once('exit', () => {clearTimeout(timer);resolve();});qemu.kill('SIGTERM');
    });
  });
  const deadline = Date.now() + 30000;
  while (!log.includes('GLISS_CLIP_READY') && Date.now() < deadline) {
    if (failure || qemu.exitCode !== null || log.includes('GLISS_CLIP_FAILED') || log.includes('Kernel panic'))
      throw failure || new Error(log);
    await new Promise(resolve => setTimeout(resolve, 30));
  }
  assert.match(log, /GLISS_CLIP_READY/, log);
  return {
    vnc: `127.0.0.1:${port}`,
    // Bytes the guest agent received for the next host paste, exactly as QEMU sent them.
    nextPaste: () => new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`No guest paste: ${log}`)), 5000);
      const take = () => {clearTimeout(timer);resolve(pastes.shift());};
      if (pastes.length) take();else waiters.push(take);
    }),
    copy: bytes => qemu.stdin.write(`copy ${Buffer.from(bytes).toString('hex')}\n`),
  };
}
