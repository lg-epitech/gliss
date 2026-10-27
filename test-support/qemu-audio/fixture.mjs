import assert from 'node:assert/strict';
import {spawn, execFileSync} from 'node:child_process';
import {mkdtemp, rm, access} from 'node:fs/promises';
import {existsSync} from 'node:fs';
import {tmpdir, release} from 'node:os';
import {join} from 'node:path';
import {fileURLToPath} from 'node:url';
import {freePort} from '../../tests/helpers.mjs';

// Linux-only, opt-in fixture: it creates and kills only its own diskless VM.
// No libvirt daemon, network, disk image or host audio service is involved.
export async function startAudioVm(t) {
  const directory = await mkdtemp(join(tmpdir(), 'gliss-audio-smoke-'));
  t.after(() => rm(directory, {recursive: true, force: true}));
  const kernel = process.env.GLISS_AUDIO_KERNEL ||
    [`/boot/vmlinuz-${release()}`, '/boot/vmlinuz-linux'].find(existsSync);
  assert.ok(kernel, 'Set GLISS_AUDIO_KERNEL to a readable kernel matching the host modules');
  await access(kernel);
  execFileSync('python3', [fileURLToPath(new URL('./build.py', import.meta.url)), directory]);
  const port = await freePort();
  assert.ok(port >= 5900);
  const qemu = spawn('qemu-system-x86_64', ['-machine', `pc,accel=${process.env.GLISS_QEMU_ACCEL || 'kvm'}`,
    '-cpu', process.env.GLISS_QEMU_ACCEL === 'tcg' ? 'max' : 'host', '-m', '256', '-nodefaults', '-no-reboot',
    '-device', 'VGA', '-device', 'intel-hda,id=hda', '-device', 'hda-output,audiodev=audio0',
    '-audiodev', 'driver=none,id=audio0', '-vnc', `127.0.0.1:${port - 5900},audiodev=audio0`,
    '-kernel', kernel, '-initrd', join(directory, 'initramfs.cpio'), '-append', 'console=ttyS0 quiet panic=0',
    '-serial', 'stdio', '-monitor', 'none'], {stdio: ['ignore', 'pipe', 'pipe']});
  let log = '', failure;
  qemu.on('error', error => {failure = error;});
  for (const stream of [qemu.stdout, qemu.stderr]) stream.on('data', data => {log = (log + data).slice(-8192);});
  t.after(async () => {
    if (qemu.exitCode !== null || !qemu.pid) return;
    await new Promise(resolve => {
      const timer = setTimeout(() => qemu.kill('SIGKILL'), 2000);
      qemu.once('exit', () => {clearTimeout(timer);resolve();});qemu.kill('SIGTERM');
    });
  });
  const deadline = Date.now() + 30000;
  while (!log.includes('GLISS_AUDIO_READY') && Date.now() < deadline) {
    if (failure || qemu.exitCode !== null || log.includes('Kernel panic')) throw failure || new Error(log);
    await new Promise(resolve => setTimeout(resolve, 30));
  }
  assert.match(log, /GLISS_AUDIO_READY/, log);
  return {vnc: `127.0.0.1:${port}`, port, log: () => log, directory};
}
