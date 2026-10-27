import test from 'node:test';
import assert from 'node:assert/strict';
import {AudioPlayer} from '../web/audio-player.js';

// Advance the playback clock and retire completed sources as Web Audio does.
// Packet arrivals include TCP catch-up after one delayed delivery.
for (const delayMs of [40, 50]) test(`audio: playback stays continuous through a ${delayMs} ms packet delay`, () => {
  const active = new Set(), starts = [];
  const context = {
    state: 'running', currentTime: 1, destination: {},
    createBuffer: () => ({copyToChannel() {}}),
    createBufferSource() {
      const source = {
        connect() {}, disconnect() {},
        start(time) {this.end = time + 0.02;active.add(this);starts.push(time);},
        stop() {active.delete(this);},
      };
      return source;
    },
  };
  const player = new AudioPlayer();player.context = context;
  let maxQueued = 0;
  for (let index = 0; index < 80; index++) {
    context.currentTime = Math.max(context.currentTime, 1 + index * 0.02 + (index === 25 ? delayMs / 1000 : 0));
    for (const source of active) if (source.end <= context.currentTime) {
      active.delete(source);source.onended?.();
    }
    player.schedule(960, () => new Float32Array(960));
    maxQueued = Math.max(maxQueued, player.snapshot().queuedSeconds);
  }
  const gaps = starts.slice(1).filter((time, index) => time > starts[index] + 0.02 + 1e-9);
  assert.equal(gaps.length, 0, 'A delayed packet must not leave a gap between scheduled audio buffers');
  assert.ok(maxQueued <= 0.2, 'Jitter buffering must stay within the playback bound');
  assert.equal(player.dropped, 0);
});
