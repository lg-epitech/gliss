// Fixed eight-frame H.264 launcher for the owned audio/motion study.
// The earlier four-frame launcher remains unchanged.
import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {randomBytes} from 'node:crypto';
import {performance} from 'node:perf_hooks';
import {serverBinary} from '../../tests/helpers.mjs';

export function validateSocketPolicy(snapshot, requested) {
  assert.equal(typeof requested, 'boolean');
  assert.ok(snapshot && typeof snapshot === 'object');
  assert.equal(snapshot.requested_nodelay, requested);
  assert.match(snapshot.scope, /actual option verified before HTTP IO/);
  for (const name of ['accepted_sockets', 'verified_sockets', 'rejected_sockets', 'initial_true', 'initial_false',
    'initial_read_errors', 'set_errors', 'readback_errors', 'readback_mismatches', 'verified_readback_true', 'verified_readback_false'])
    assert.ok(Number.isSafeInteger(snapshot[name]) && snapshot[name] >= 0);
  assert.ok(snapshot.accepted_sockets > 0);
  assert.equal(snapshot.accepted_sockets, snapshot.verified_sockets);
  assert.equal(snapshot.initial_true + snapshot.initial_false, snapshot.accepted_sockets);
  assert.equal(snapshot.verified_readback_true, requested ? snapshot.accepted_sockets : 0);
  assert.equal(snapshot.verified_readback_false, requested ? 0 : snapshot.accepted_sockets);
  for (const name of ['rejected_sockets', 'initial_read_errors', 'set_errors', 'readback_errors', 'readback_mismatches'])
    assert.equal(snapshot[name], 0);
  return {...snapshot};
}

export async function startSkipServer({tcpNodelay, port, vnc, codec, fps, h264Bitrate, inFlightFrames, frameSkip, origin}) {
  assert.equal(typeof tcpNodelay, 'boolean');
  assert.ok(Number.isInteger(port) && port > 0 && port <= 65535);
  assert.match(vnc, /^127\.0\.0\.1:[0-9]+$/);
  assert.equal(codec, 'h264');assert.equal(fps, 30);
  assert.equal(h264Bitrate,500000);assert.equal(typeof frameSkip,'boolean');assert.equal(inFlightFrames, 8);
  const token = randomBytes(24).toString('base64url'), url = `http://127.0.0.1:${port}`;
  const args = ['--bind', `127.0.0.1:${port}`, '--codec', codec, '--workload', 'static', '--fps', String(fps),
    '--max-clients', '8', '--vnc', vnc, '--h264-bitrate', String(h264Bitrate),
    '--in-flight-frames', String(inFlightFrames), '--tcp-nodelay', String(tcpNodelay), '--video-frame-skip', String(frameSkip), '--stream-stats'];
  if (origin) args.push('--origin', origin);
  const child = spawn(serverBinary(), args, {env: {...process.env, GLISS_TOKEN: token}, stdio: ['ignore', 'pipe', 'pipe']});
  let failure, log = '', stopping;
  const terminal = new Promise(resolve => {
    child.once('exit', resolve);
    child.once('error', error => {failure = error;resolve();});
  });
  for (const stream of [child.stdout, child.stderr]) stream.on('data', bytes => {log = (log + bytes).slice(-8192);});
  const stop = () => stopping ??= (async () => {
    if (child.exitCode !== null || child.signalCode !== null || !child.pid) return;
    let kill, expired;
    try {
      kill = setTimeout(() => {
        if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
      }, 2000);
      child.kill('SIGINT');
      await Promise.race([terminal, new Promise((_, reject) => {
        expired = setTimeout(() => reject(new Error('Owned policy server did not exit')), 3000);
      })]);
      assert.ok(child.exitCode !== null || child.signalCode !== null || failure);
    } finally {clearTimeout(kill);clearTimeout(expired);}
  })();
  try {
    const due = performance.now() + 15000;
    while (performance.now() < due) {
      if (failure) throw failure;
      if (child.exitCode !== null || child.signalCode !== null) throw new Error('Policy server exited during startup');
      try {
        const response = await fetch(`${url}/health`, {signal: AbortSignal.timeout(1000)});
        await response.arrayBuffer();
        if (response.ok && performance.now() < due) return {url, port, token, child, stop, output: () => log};
      } catch { /* The fixed owned port may not be listening yet. */ }
      await new Promise(resolve => setTimeout(resolve, 30));
    }
    throw new Error('Policy server startup timed out');
  } catch (error) {await stop();throw error;}
}
