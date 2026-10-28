// Disposable, serial-driven guest source. It never attaches to an existing VM.
import assert from 'node:assert/strict';
import {spawn, execFileSync} from 'node:child_process';
import {mkdtemp, rm, access} from 'node:fs/promises';
import {existsSync} from 'node:fs';
import {tmpdir, release} from 'node:os';
import {join} from 'node:path';
import {fileURLToPath} from 'node:url';
import {performance} from 'node:perf_hooks';
import {freePort} from '../../tests/helpers.mjs';

export async function startAudioMarkerVm(t, {cpu = 19, mixFrequency} = {}) {
  assert.ok(Number.isSafeInteger(cpu) && cpu > 0);
  assert.ok(mixFrequency === undefined || [44100, 48000].includes(mixFrequency));
  const directory = await mkdtemp(join(tmpdir(), 'gliss-audio-marker-'));
  t.after(() => rm(directory, {recursive: true, force: true}));
  const kernel = process.env.GLISS_AUDIO_KERNEL ||
    [`/boot/vmlinuz-${release()}`, '/boot/vmlinuz-linux'].find(existsSync);
  assert.ok(kernel);await access(kernel);
  execFileSync('python3', [fileURLToPath(new URL('./build.py', import.meta.url)), directory],
    {timeout: 30000, maxBuffer: 1048576});
  const port = await freePort();assert.ok(port >= 5900);
  const audioDevice = 'driver=none,id=audio0' + (mixFrequency === undefined ? '' :
    `,out.frequency=${mixFrequency},out.channels=2,out.format=s16`);
  const qemu = spawn('taskset', ['-c', String(cpu), 'qemu-system-x86_64', '-machine',
    `pc,accel=${process.env.GLISS_QEMU_ACCEL || 'kvm'}`, '-cpu', process.env.GLISS_QEMU_ACCEL === 'tcg' ? 'max' : 'host',
    '-m', '256', '-nodefaults', '-no-reboot', '-device', 'VGA', '-device', 'intel-hda,id=hda',
    '-device', 'hda-output,audiodev=audio0', '-audiodev', audioDevice,
    '-vnc', `127.0.0.1:${port - 5900},audiodev=audio0`, '-kernel', kernel,
    '-initrd', join(directory, 'initramfs.cpio'), '-append', 'console=ttyS0 quiet panic=0',
    '-serial', 'stdio', '-monitor', 'none'], {stdio: ['pipe', 'pipe', 'pipe']});
  let log = '', pending = '', failure, stopped;
  const notes = new Map();
  qemu.on('error', error => {failure = error;});qemu.stdin.on('error', error => {failure = error;});
  qemu.stderr.on('data', bytes => {log = (log + bytes).slice(-8192);});
  qemu.stdout.on('data', bytes => {
    log = (log + bytes).slice(-8192);pending = (pending + bytes).slice(-16384);
    let newline;
    while ((newline = pending.indexOf('\n')) >= 0) {
      const line = pending.slice(0, newline).replace(/\r$/, '');pending = pending.slice(newline + 1);
      const match = /^GLISS_AUDIO_MARKER ([0-9]+) ([0-9]+) ([0-9]+)$/.exec(line);
      if (match) {
        if (notes.size >= 128) {failure = new Error('Bounded guest marker notes exceeded');continue;}
        const token = Number(match[1]);
        if (!Number.isSafeInteger(token) || token < 1 || token > 0xffffffff || notes.has(token)) {
          failure = new Error('Invalid guest marker token');continue;
        }
        notes.set(token, {token, code: Number(match[2]), guest_sample: Number(match[3]), host_received_ms: performance.now()});
      }
    }
  });
  const stop = () => stopped ??= (async () => {
    if (qemu.exitCode !== null || qemu.signalCode !== null || !qemu.pid) return;
    let kill, expired;
    const terminal = new Promise(resolve => qemu.once('exit', resolve));
    try {
      kill = setTimeout(() => {if (qemu.exitCode === null && qemu.signalCode === null) qemu.kill('SIGKILL');}, 2000);
      qemu.kill('SIGTERM');
      await Promise.race([terminal, new Promise((_, reject) => {expired = setTimeout(() => reject(new Error('Guest cleanup unconfirmed')), 3000);})]);
    } finally {clearTimeout(kill);clearTimeout(expired);}
  })();
  t.after(stop);
  const deadline = performance.now() + 30000;
  while (!log.includes('GLISS_AUDIO_MARKER_READY') && performance.now() < deadline) {
    if (failure || qemu.exitCode !== null || qemu.signalCode !== null || /Kernel panic|GLISS_AUDIO_MARKER_ERROR/.test(log))
      throw failure || new Error('Guest marker startup failed');
    await new Promise(resolve => setTimeout(resolve, 30));
  }
  assert.ok(log.includes('GLISS_AUDIO_MARKER_READY'), 'Guest marker startup timed out');
  return {vnc: `127.0.0.1:${port}`, child: qemu, notes, audioDevice, log: () => log,
    async pulse(token, code) {
      assert.ok(Number.isInteger(token) && token > 0 && token <= 0xffffffff);
      assert.ok(Number.isInteger(code) && code >= 1 && code <= 48);
      if (failure) throw failure;
      const command = `P${token.toString(16).padStart(8, '0')}:${code.toString(16).padStart(2, '0')}\n`;
      await new Promise((resolve, reject) => qemu.stdin.write(command, error => error ? reject(error) : resolve()));
    },
  };
}
