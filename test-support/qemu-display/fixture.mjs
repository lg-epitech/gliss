// Linux/KVM pieces of scripts/bench-distribox-path.mjs: a diskless virtio-gpu guest
// behind QEMU's own VNC server, a gliss-server started the way Distribox starts it,
// and the page instrumentation that reads the guest's marker strips (see init.c).
// Each start function returns a stop() for exactly the process it spawned; nothing
// else on the host is signalled. No libvirt, disk image or network is involved.
import {spawn, execFileSync} from 'node:child_process';
import {randomBytes} from 'node:crypto';
import {chmodSync, closeSync, existsSync, mkdirSync, openSync, readFileSync, readdirSync, readlinkSync, writeFileSync} from 'node:fs';
import {release} from 'node:os';
import {basename, join} from 'node:path';
import {fileURLToPath} from 'node:url';
import {freePort} from '../../tests/helpers.mjs';

// One wall clock for the harness, the page and QEMU's trace timestamps.
export const clock = () => performance.timeOrigin + performance.now();
// Period of the guest's animation clock (TICK_NS in init.c).
export const TICK_MS = 16.666667;
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));

async function stopChild(child, signal) {
  if (child.exitCode !== null || child.signalCode !== null || !child.pid) return;
  await new Promise(resolve => {
    const timer = setTimeout(() => child.kill('SIGKILL'), 2000);
    child.once('exit', () => {clearTimeout(timer);resolve();});child.kill(signal);
  });
}

// Host kernel + a static init + the two virtio drivers this kernel ships as modules.
export function buildGuest(directory) {
  const root = join(directory, 'root');
  mkdirSync(join(root, 'drivers'), {recursive: true});
  execFileSync('gcc', ['-O2', '-static', '-o', join(root, 'init'), fileURLToPath(new URL('./init.c', import.meta.url))]);
  const modules = [];
  for (const driver of ['virtio-gpu', 'virtio_input']) {
    for (const line of execFileSync('modprobe', ['--show-depends', driver], {encoding: 'utf8'}).split('\n')) {
      const source = /^insmod (\S+)/.exec(line)?.[1];
      if (!source) continue;
      const name = basename(source).replace(/\.(zst|xz|gz)$/, ''), tool = {zst: 'zstd', xz: 'xz', gz: 'gzip'}[source.split('.').pop()];
      if (modules.includes(`/drivers/${name}`)) continue;
      writeFileSync(join(root, 'drivers', name),
        tool ? execFileSync(tool, ['-dc', source], {maxBuffer: 1 << 28}) : readFileSync(source));
      modules.push(`/drivers/${name}`);
    }
  }
  writeFileSync(join(root, 'modules'), modules.join('\n') + '\n');
  const initramfs = join(directory, 'initramfs.cpio'), output = openSync(initramfs, 'w');
  try {
    execFileSync('cpio', ['--null', '-o', '--format=newc'], {cwd: root,
      input: execFileSync('find', ['.', '-print0'], {cwd: root}), stdio: ['pipe', output, 'ignore']});
  } finally {
    closeSync(output);
  }
  return initramfs;
}

// The devices of a Distribox domain (backend/app/core/xml_builder.py) without libvirt:
// KVM, virtio video, loopback VNC with a `none` audio backend, PS/2 keyboard, virtio
// tablet. `traceRefresh` logs every change of QEMU's display refresh interval.
export async function startDisplayVm({directory, width, height, traceRefresh = false}) {
  const kernel = process.env.GLISS_QEMU_KERNEL || [`/boot/vmlinuz-${release()}`, '/boot/vmlinuz-linux'].find(existsSync);
  if (!kernel) throw new Error('Set GLISS_QEMU_KERNEL to a readable kernel matching the host modules');
  const initramfs = buildGuest(directory), port = await freePort();
  if (port < 5900) throw new Error('No VNC display number for the free port');
  const qemu = spawn('qemu-system-x86_64', ['-machine', 'pc,accel=kvm', '-cpu', 'host', '-smp', '2', '-m', '1024',
    '-nodefaults', '-no-reboot', '-device', `virtio-vga,xres=${width},yres=${height}`,
    '-audiodev', 'none,id=a1', '-device', 'intel-hda', '-device', 'hda-output,audiodev=a1',
    '-vnc', `127.0.0.1:${port - 5900},audiodev=a1`, '-device', 'virtio-tablet-pci',
    '-kernel', kernel, '-initrd', initramfs, '-append', 'console=ttyS0 quiet loglevel=0 panic=-1',
    '-serial', 'stdio', '-monitor', 'none',
    ...(traceRefresh ? ['-trace', 'enable=console_refresh', '-msg', 'timestamp=on'] : [])],
  {stdio: ['ignore', 'pipe', 'pipe']});
  const vm = {vnc: `127.0.0.1:${port}`, pid: qemu.pid, kernel, ready: null, mode: null,
    ticks: [], scenes: [], refresh: [], log: '', stop: () => stopChild(qemu, 'SIGTERM'), kill: () => qemu.kill('SIGKILL')};
  let failure;
  qemu.on('error', error => {failure = error;});
  const lines = (stream, handle) => {
    let pending = '';
    stream.on('data', data => {
      const at = clock();
      vm.log = (vm.log + data).slice(-16384);
      pending += data.toString('latin1');
      for (let end; (end = pending.indexOf('\n')) >= 0; pending = pending.slice(end + 1)) handle(pending.slice(0, end).trim(), at);
    });
  };
  lines(qemu.stdout, (line, at) => {
    let match;
    if ((match = /^T (\d+) (-?\d+)$/.exec(line))) vm.ticks.push({at, tick: Number(match[1]), lateUs: Number(match[2])});
    else if (line.startsWith('S ')) vm.scenes.push({at, ...Object.fromEntries(line.slice(2).split(' ').map(pair => {
      const [key, value] = pair.split('=');return [key, Number(value)];
    }))});
    else if ((match = /^READY (\d+)x(\d+)$/.exec(line))) vm.ready = {width: Number(match[1]), height: Number(match[2])};
    else if (line.startsWith('MODE ')) vm.mode = line.slice(5);
    else if (line.startsWith('FAIL ')) failure = new Error(`Guest: ${line}`);
  });
  // "[timestamp ]console_refresh interval N ms"; the timestamp is QEMU's own when present.
  lines(qemu.stderr, (line, at) => {
    const match = /(?:(\d{4}-\d\d-\d\dT[\d:.]+Z)\s+)?(?:\d+@(\d+\.\d+):)?console_refresh interval (\d+) ms/.exec(line);
    if (match) vm.refresh.push({at: match[2] ? Number(match[2]) * 1000 : match[1] ? Date.parse(match[1]) : at, interval: Number(match[3])});
  });
  const deadline = Date.now() + 30000;
  while (!vm.ready && Date.now() < deadline && !failure && qemu.exitCode === null) await sleep(30);
  if (!vm.ready || failure) {
    await vm.stop();
    throw failure || new Error(`Guest did not start: ${vm.log}`);
  }
  if (vm.ready.width !== width || vm.ready.height !== height) {
    await vm.stop();
    throw new Error(`Guest display is ${vm.ready.width}x${vm.ready.height}, not ${width}x${height}`);
  }
  vm.exited = () => qemu.exitCode !== null || qemu.signalCode !== null;
  return vm;
}

// The exact command line and bare environment of Distribox's gliss_session()
// (backend/app/services/gliss.py), for an explicit binary.
// `origin` is where the browser loads the page from when that is not the server itself.
export async function startGliss({binary, vnc, origin}) {
  const port = await freePort(), token = randomBytes(24).toString('base64url'), url = `http://127.0.0.1:${port}`;
  const child = spawn(binary, ['--vnc', vnc, '--bind', `127.0.0.1:${port}`, '--max-clients', '1', '--codec', 'h264',
    '--h264-bitrate', '2000000', '--fps', '30', '--in-flight-frames', '8', ...(origin ? ['--origin', origin] : [])],
  {env: {GLISS_TOKEN: token}, stdio: ['ignore', 'ignore', 'pipe']});
  let output = '', failure;
  child.on('error', error => {failure = error;});
  child.stderr.on('data', data => {output = (output + data).slice(-8192);});
  const server = {url, port, token, pid: child.pid, output: () => output, stop: () => stopChild(child, 'SIGTERM'),
    kill: () => child.kill('SIGKILL'), exited: () => child.exitCode !== null || child.signalCode !== null};
  for (const deadline = Date.now() + 15000; Date.now() < deadline; await sleep(30)) {
    if (failure || server.exited()) break;
    try {if ((await fetch(`${url}/health`)).ok) return server;} catch {}
  }
  await server.stop();
  throw failure || new Error(`gliss-server did not start (${binary}): ${output}`);
}

// Runs in the page. Marker geometry mirrors init.c: strips of 22 cells of 16 px at
// x=16 and y=16 (K), 48 (F), 80 (P): white, black, 16 value bits, 4 check bits.
function installRecorder() {
  const client = window.gliss, now = () => performance.timeOrigin + performance.now();
  const bench = window.glissBench = {frames: [], sends: [], wire: {bytes: 0, messages: 0}, now};
  const strip = y => {
    // Four rows through the middle of the cells; the centre 8 px of each cell.
    const data = client.ctx.getImageData(16, y + 6, 352, 4).data;
    const level = cell => {
      let sum = 0;
      for (let row = 0; row < 4; row++) for (let x = 4; x < 12; x++) sum += data[(row * 352 + cell * 16 + x) * 4 + 1];
      return sum / 32;
    };
    const white = level(0), black = level(1);
    if (white - black < 96) return -1;
    let bits = 0;
    for (let cell = 2; cell < 22; cell++) bits = bits * 2 + (level(cell) > (white + black) / 2 ? 1 : 0);
    const value = bits >>> 4;
    return (((value ^ value >> 4 ^ value >> 8 ^ value >> 12) & 15) ^ 5) === (bits & 15) ? value : -1;
  };
  bench.read = () => ({k: strip(16), f: strip(48), p: strip(80)});
  const forward = client.onEvent;
  client.onEvent = event => {
    forward(event);
    if (event.type !== 'frame') return;
    const at = now(), marks = bench.read();
    bench.frames.push([at, event.frame.sequence, event.frame.bytes, event.decodeMs, marks.k, marks.f, marks.p]);
  };
  bench.send = (message, tag) => {
    const at = now(), id = client.send(message);
    bench.sends.push({at, tag, sent: id !== 0, ...message});
  };
  // Every binary WebSocket message, painted or not (video and audio).
  bench.watchSocket = () => client.socket.addEventListener('message', event => {
    if (typeof event.data === 'string') return;
    bench.wire.bytes += event.data.byteLength;bench.wire.messages++;
  });
  bench.take = () => {
    const taken = {frames: bench.frames, sends: bench.sends, wire: bench.wire, errors: client.metrics.errors};
    bench.frames = [];bench.sends = [];bench.wire = {bytes: 0, messages: 0};
    return taken;
  };
  const until = async time => {
    const wait = time - performance.now();
    if (wait > 0) await new Promise(resolve => setTimeout(resolve, wait));
  };
  // events: [{at: ms from start, message, tag}] in time order. Keys leave the moment
  // their time comes, as a keydown handler would send them. Returns the window's
  // start, end and the media received in between.
  bench.play = async (events, duration) => {
    const start = performance.now();
    bench.wire = {bytes: 0, messages: 0};
    for (const event of events) {
      await until(start + event.at);
      bench.send(event.message, event.tag);
    }
    await until(start + duration);
    return [performance.timeOrigin + start, now(), bench.wire];
  };
  // Pointer motion leaves once per animation frame with the newest position, as the
  // client's own pointermove handler coalesces it. path: [[x, y]] at `stepMs` spacing.
  bench.drag = (path, stepMs) => new Promise(resolve => {
    const start = performance.now();
    let last = -1;
    bench.wire = {bytes: 0, messages: 0};
    const frame = () => {
      const step = Math.min(path.length - 1, Math.floor((performance.now() - start) / stepMs));
      if (step > last) bench.send({type: 'pointer', x: path[step][0], y: path[step][1], buttons: 1}, last = step);
      if (step < path.length - 1) requestAnimationFrame(frame);
      else resolve([performance.timeOrigin + start, now(), bench.wire]);
    };
    requestAnimationFrame(frame);
  });
}

// Offset of the page's clock from this process's, from the tightest of a few round trips.
export async function pageClockOffset(page) {
  let best = {roundTripMs: Infinity};
  for (let n = 0; n < 15; n++) {
    const sent = clock(), remote = await page.evaluate(() => performance.timeOrigin + performance.now()), received = clock();
    if (received - sent < best.roundTripMs) best = {roundTripMs: received - sent, offsetMs: remote - (sent + received) / 2};
  }
  return best;
}

// Logs in as tests/helpers.mjs loginPage does and returns once a frame is painted,
// with `window.glissBench` recording every painted frame from the first one.
export async function openViewer(browser, server, viewport = {width: 1920, height: 1080}) {
  const page = await browser.newPage({viewport}), errors = [];
  page.on('pageerror', error => errors.push(error.message));
  await page.goto(server.url);
  await page.evaluate(installRecorder);
  await page.locator('#token').fill(server.token);
  const login = clock();
  await page.locator('#login button').click();
  await page.waitForFunction(() => window.gliss?.metrics.frames > 0, null, {timeout: 30000, polling: 20});
  const firstFrameMs = clock() - login;
  await page.evaluate(() => window.glissBench.watchSocket());
  return {page, errors, firstFrameMs, clock: await pageClockOffset(page)};
}

// ---------------------------------------------------------------- link

// "rtt=100,down=1.5,up=10,queue=200,loss=0.5": the round trip in ms, split evenly
// between the directions; the rates towards the browser and back in Mbit/s, unlimited
// when absent; how many ms of packets the bottleneck holds before it drops; random
// loss towards the browser in percent. "down=4/0.8@3" alternates between two rates
// every 3 s, counted from each restart(). "idle=0" turns off the relay host's
// net.ipv4.tcp_slow_start_after_idle, which is on by default in Linux.
export function parseLink(text) {
  const link = {rtt: 0, down: null, up: null, queue: 200, loss: 0, period: 0, idle: 1};
  for (const part of text.split(',')) {
    const [name, value = ''] = part.split('='), [rates, period] = value.split('@');
    if (name === 'down' && period) Object.assign(link, {down: rates.split('/').map(Number), period: Number(period) * 1000});
    else if (name === 'down') link.down = [Number(value)];
    else if (name in link && name !== 'period') link[name] = Number(value);
    else throw new Error(`Unknown link setting: ${part}`);
  }
  const numbers = [link.rtt, link.queue, link.loss, link.period, link.up ?? 1, ...link.down ?? [1]];
  if (numbers.some(number => !(number >= 0 && Number.isFinite(number))) || [link.up, ...link.down ?? []].includes(0) || link.loss >= 100 || ![0, 1].includes(link.idle) ||
    (link.period ? link.down.length !== 2 : link.down?.length > 1)) throw new Error(`Invalid link: ${text}`);
  return link;
}

// A WAN between the server and the browser, for a harness that runs in a user and
// network namespace of its own (`unshare -Urn`, which needs no privilege). The browser
// gets a second network namespace and a veth pair joins the two. On each end netem
// holds every packet for half the round trip, then a second netem lets them out at
// the direction's rate and drops what its buffer cannot hold. The kernel's own TCP
// runs across it, so slow start, a restart after idle, loss recovery and the queue at
// the bottleneck are real. QEMU and gliss-server stay on this namespace's loopback.
//
// The browser reaches the server through relay.mjs at `origin`, started by attach().
// `browser` is a Chromium that runs in the far namespace and treats that plain-HTTP
// origin as secure, which WebCodecs requires.
export async function startLink({directory, link, chromium}) {
  const SERVER = '10.77.0.1', BROWSER = '10.77.0.2', port = 8077, origin = `http://${SERVER}:${port}`;
  const run = (...command) => execFileSync(command[0], command.slice(1), {stdio: ['ignore', 'ignore', 'pipe']});
  run('ip', 'link', 'set', 'lo', 'up');
  const far = spawn('unshare', ['-n', 'sleep', 'infinity'], {stdio: 'ignore'});
  const inFar = (...command) => run('nsenter', '-t', String(far.pid), '-n', ...command);
  try {
    const here = readlinkSync('/proc/self/ns/net');
    for (const deadline = Date.now() + 5000; readlinkSync(`/proc/${far.pid}/ns/net`) === here; await sleep(10)) {
      if (Date.now() > deadline || far.exitCode !== null) throw new Error('unshare -n did not create a network namespace');
    }
    // No IPv6: its link-local address would arrive a second after the interface comes
    // up, and Chromium aborts what it is loading when the set of addresses changes.
    const noIpv6 = ['sh', '-c', 'echo 1 > /proc/sys/net/ipv6/conf/all/disable_ipv6'];
    run(...noIpv6);
    inFar(...noIpv6);
    run('sh', '-c', `echo ${link.idle} > /proc/sys/net/ipv4/tcp_slow_start_after_idle`);
    run('ip', 'link', 'add', 'wan0', 'type', 'veth', 'peer', 'name', 'wan1', 'netns', String(far.pid));
    // One TCP segment per packet: netem would hold and count a segmentation-offload batch as one.
    run('ip', 'link', 'set', 'wan0', 'gso_max_segs', '1');
    run('ip', 'addr', 'add', `${SERVER}/24`, 'dev', 'wan0');
    run('ip', 'link', 'set', 'wan0', 'up');
    inFar('ip', 'link', 'set', 'lo', 'up');
    inFar('ip', 'link', 'set', 'wan1', 'gso_max_segs', '1');
    inFar('ip', 'addr', 'add', `${BROWSER}/24`, 'dev', 'wan1');
    inFar('ip', 'link', 'set', 'wan1', 'up');
    const shape = (tc, device, rate, loss, verb) => {
      if (verb === 'add') tc('tc', 'qdisc', 'add', 'dev', device, 'root', 'handle', '1:', 'netem', 'delay', `${link.rtt / 2}ms`, 'limit', '100000');
      // 1500-byte packets: the buffer holds `queue` ms of them at this rate, and at least four.
      const bottleneck = rate
        ? ['rate', `${Math.round(rate * 1000)}kbit`, 'limit', String(Math.max(4, Math.ceil(rate * link.queue / 12)))]
        : ['limit', '100000'];
      tc('tc', 'qdisc', verb, 'dev', device, 'parent', '1:', 'handle', '2:', 'netem', ...bottleneck, ...(loss ? ['loss', `${loss}%`] : []));
    };
    shape(run, 'wan0', link.down?.[0], link.loss, 'add');
    shape(inFar, 'wan1', link.up, 0, 'add');
    const browser = join(directory, 'chromium-behind-link');
    writeFileSync(browser, `#!/bin/sh\nexec nsenter -t ${far.pid} -n -- '${chromium}' --unsafely-treat-insecure-origin-as-secure=${origin} "$@"\n`);
    chmodSync(browser, 0o755);

    let relay, timer, failure;
    const detach = async () => {if (relay) await stopChild(relay, 'SIGTERM');relay = undefined;};
    return {origin, browser, link,
      // Why the link no longer is what was asked for, if it is not.
      failure: () => failure,
      // The link starts over at its first rate; an alternating one changes every period from now.
      restart() {
        clearInterval(timer);
        if (!link.period) return;
        let phase = 0;
        shape(run, 'wan0', link.down[0], link.loss, 'change');
        // A failure here must not escape the timer: it would end the process before its cleanup.
        timer = setInterval(() => {
          try {
            shape(run, 'wan0', link.down[++phase % 2], link.loss, 'change');
          } catch (error) {
            clearInterval(timer);
            failure ??= `The link stopped changing its rate: ${error.stderr?.toString().trim() || error.message}`;
          }
        }, link.period);
      },
      async attach(server) {
        await detach();
        relay = spawn(process.execPath, [fileURLToPath(new URL('relay.mjs', import.meta.url)), SERVER, String(port), String(server.port)],
          {stdio: ['ignore', 'ignore', 'inherit', 'ipc']});
        await new Promise((resolve, reject) => {relay.once('message', resolve);relay.once('exit', () => reject(new Error('The link relay did not start')));});
        return {...server, url: origin, relay: relay.pid};
      },
      detach,
      async stop() {clearInterval(timer);await detach();await stopChild(far, 'SIGKILL');}};
  } catch (error) {
    far.kill('SIGKILL');
    throw new Error(`Cannot build the link: ${error.stderr?.toString().trim() || error.message}`);
  }
}

const TICK = 1000 / Number(execFileSync('getconf', ['CLK_TCK'], {encoding: 'utf8'}));

// utime+stime of every live process, in ms, with its parent: {pid: [ppid, cpuMs, guestMs]}.
// QEMU's guestMs is the part of its CPU time spent running the vCPUs.
export function processTimes() {
  const table = new Map();
  for (const entry of readdirSync('/proc')) {
    if (!/^\d+$/.test(entry)) continue;
    try {
      const stat = readFileSync(`/proc/${entry}/stat`, 'latin1'), fields = stat.slice(stat.lastIndexOf(')') + 2).split(' ');
      table.set(Number(entry), [Number(fields[1]), (Number(fields[11]) + Number(fields[12])) * TICK, Number(fields[40]) * TICK]);
    } catch {}
  }
  return table;
}

// CPU ms spent between two processTimes() tables by `roots` and their descendants.
// A process that exited in between is not counted.
export function cpuBetween(before, after, roots, guest = false) {
  const members = new Set(roots);
  for (let grew = true; grew;) {
    grew = false;
    for (const [pid, [parent]] of after) if (members.has(parent) && !members.has(pid)) {members.add(pid);grew = true;}
  }
  let total = 0;
  for (const pid of members) if (after.has(pid)) total += after.get(pid)[guest ? 2 : 1] - (before.get(pid)?.[guest ? 2 : 1] ?? 0);
  return total;
}

export const childPids = () => [...processTimes()].filter(([, [parent]]) => parent === process.pid).map(([pid]) => pid);
