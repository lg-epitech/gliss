// Disposable, serial-driven guest source. It never attaches to an existing VM.
import assert from 'node:assert/strict';
import {spawn, execFileSync} from 'node:child_process';
import {mkdtemp, rm, access, readFile} from 'node:fs/promises';
import {existsSync} from 'node:fs';
import {tmpdir, release} from 'node:os';
import {join} from 'node:path';
import {fileURLToPath} from 'node:url';
import {performance} from 'node:perf_hooks';
import {createHash} from 'node:crypto';
import {freePort} from '../../tests/helpers.mjs';

export async function startAudioMotionVm(t, {cpu = 19, mixFrequency = 48000} = {}) {
  assert.ok(Number.isSafeInteger(cpu) && cpu > 0);
  assert.ok(mixFrequency === undefined || [44100, 48000].includes(mixFrequency));
  const directory = await mkdtemp(join(tmpdir(), 'gliss-audio-motion-'));
  t.after(() => rm(directory, {recursive: true, force: true}));
  const kernel = process.env.GLISS_AUDIO_KERNEL ||
    [`/boot/vmlinuz-${release()}`, '/boot/vmlinuz-linux'].find(existsSync);
  assert.ok(kernel);await access(kernel);
  execFileSync('python3', [fileURLToPath(new URL('./build.py', import.meta.url)), directory],
    {timeout: 30000, maxBuffer: 1048576});
  const digest = bytes => createHash('sha256').update(bytes).digest('hex');
  const buildIdentity = {kernel_sha256:digest(await readFile(kernel)),initramfs_sha256:digest(await readFile(join(directory,'initramfs.cpio'))),
    init_binary_sha256:digest(await readFile(join(directory,'root/init'))),qemu_version:execFileSync('qemu-system-x86_64',['--version'],{encoding:'utf8'}).split('\n')[0]};
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
  const frames = [], modes = [];
  let log = '', pending = '', failure, stopped;
  const notes = new Map();
  qemu.on('error', error => {failure = error;});qemu.stdin.on('error', error => {failure = error;});
  qemu.stderr.on('data', bytes => {log = (log + bytes).slice(-8192);});
  qemu.stdout.on('data', bytes => {
    log = (log + bytes).slice(-8192);pending = (pending + bytes).slice(-16384);
    let newline;
    while ((newline = pending.indexOf('\n')) >= 0) {
      const line = pending.slice(0, newline).replace(/\r$/, '');pending = pending.slice(newline + 1);
      const frame = /^GLISS_AUDIO_MOTION_FRAME ([0-9]+) ([0-9]+)$/.exec(line);
      const mode = /^GLISS_AUDIO_MOTION_MODE ([01]) ([0-9]+)$/.exec(line);
      if (frame) {if (frames.length >= 4096) failure = new Error('Motion frame cap exceeded');else frames.push({frame: Number(frame[1]), guest_ns: Number(frame[2]), host_received_ms: performance.now()});}
      if (mode) {if (modes.length >= 64) failure = new Error('Motion mode cap exceeded');else modes.push({motion: Number(mode[1]), guest_sample: Number(mode[2]), host_received_ms: performance.now()});}
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
  while ((!log.includes('GLISS_AUDIO_MARKER_READY') || !log.includes('GLISS_AUDIO_MOTION_READY')) && performance.now() < deadline) {
    if (failure || qemu.exitCode !== null || qemu.signalCode !== null || /Kernel panic|GLISS_AUDIO_MARKER_ERROR|GLISS_AUDIO_MOTION_ERROR/.test(log))
      throw failure || new Error('Guest marker startup failed');
    await new Promise(resolve => setTimeout(resolve, 30));
  }
  assert.ok(log.includes('GLISS_AUDIO_MARKER_READY') && log.includes('GLISS_AUDIO_MOTION_READY'), 'Guest audio/motion startup timed out');
  return {buildIdentity, frames, modes, async setMotion(enabled) {
    assert.equal(typeof enabled, 'boolean');const count = modes.length;
    await new Promise((resolve, reject) => qemu.stdin.write(`M${enabled ? 1 : 0}\n`, error => error ? reject(error) : resolve()));
    const due = performance.now() + 1500;
    while (modes.length === count && performance.now() < due) {if (failure) throw failure;await new Promise(resolve => setTimeout(resolve, 5));}
    assert.ok(modes.length > count, 'Guest motion acknowledgement timed out');
    assert.equal(modes.at(-1).motion, enabled ? 1 : 0);
  }, vnc: `127.0.0.1:${port}`, child: qemu, notes, audioDevice, log: () => log,
    async pulse(token, code) {
      assert.ok(Number.isInteger(token) && token > 0 && token <= 0xffffffff);
      assert.ok(Number.isInteger(code) && code >= 1 && code <= 48);
      if (failure) throw failure;
      const command = `P${token.toString(16).padStart(8, '0')}:${code.toString(16).padStart(2, '0')}\n`;
      await new Promise((resolve, reject) => qemu.stdin.write(command, error => error ? reject(error) : resolve()));
    },
  };
}
