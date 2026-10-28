import {AudioPlayer as Baseline} from '/web/audio-player.js';
import {AudioPlayer as Candidate} from '/test-support/audio-direct-fill/audio-player.js';

const until = async end => {while (performance.now() < end) await new Promise(resolve => setTimeout(resolve, Math.min(20, end - performance.now())));};
const assert = (value, message) => {if (!value) throw new Error(message);};
let player, codec, payloads, sequence = 0, capture = false, captures = [], errors = [], timings = [], windowStart = Infinity, windowEnd = -Infinity;

window.preparePool = async () => {
  const pcm = [], opus = [];
  const supported = await AudioEncoder.isConfigSupported({codec: 'opus', sampleRate: 48000, numberOfChannels: 2, bitrate: 96000});
  assert(supported.supported, 'Opus encoder unavailable');
  const encoder = new AudioEncoder({output(chunk) {
    assert(opus.length < 128, 'Encoded pool cap');
    const bytes = new Uint8Array(chunk.byteLength);chunk.copyTo(bytes);opus.push(Array.from(bytes));
  },error() {errors.push('encoder');}});
  encoder.configure({codec: 'opus', sampleRate: 48000, numberOfChannels: 2, bitrate: 96000});
  try {
    for (let index = 0; index < 64; index++) {
      const bytes = new Uint8Array(3840), view = new DataView(bytes.buffer), samples = new Float32Array(1920);
      for (let i = 0; i < 960; i++) for (let channel = 0; channel < 2; channel++) {
        const value = Math.round(Math.sin((index * 960 + i) * 2 * Math.PI * (channel ? 880 : 440) / 48000) * 12000);
        view.setInt16((i * 2 + channel) * 2, value, true);samples[channel * 960 + i] = value / 32768;
      }
      pcm.push(Array.from(bytes));
      const audio = new AudioData({format: 'f32-planar', sampleRate: 48000, numberOfFrames: 960,
        numberOfChannels: 2, timestamp: index * 20000, data: samples});
      try {encoder.encode(audio);} finally {audio.close();}
    }
    await encoder.flush();
  } finally {encoder.close();}
  assert(errors.length === 0 && opus.length >= 64 && opus.every(b => b.length > 0 && b.length <= 4096), 'Encoded pool incomplete');
  return {pcm, opus};
};

window.configure = (mode, requestedCodec, pool, withCapture) => {
  player?.dispose();codec = requestedCodec;sequence = 0;
  assert(['baseline', 'direct'].includes(mode) && ['pcm', 'opus'].includes(codec), 'Unknown position');
  payloads = pool.map(bytes => new Uint8Array(bytes));
  capture = withCapture;captures = [];errors = [];timings = [];windowStart = Infinity;windowEnd = -Infinity;
  const Player = mode === 'baseline' ? Baseline : Candidate;
  player = new Player(state => {if (state.message) {assert(errors.length < 16, 'Error cap');errors.push(state.message);}});
  player.status({available: true, codecs: codec === 'pcm' ? ['pcm'] : ['opus', 'pcm']});
  document.querySelector('#sound').onclick = () => {window.soundReady = player.enable().then(value => {
    assert(value === codec && player.enabled, 'Playback unavailable');
    const context = player.context;
    if (capture) {
      const original = context.createBufferSource.bind(context);
      context.createBufferSource = () => {
        const source = original(), start = source.start.bind(source);
        source.start = (...args) => {
          assert(captures.length < 8, 'Capture cap');
          const buffer = source.buffer;
          assert(buffer.numberOfChannels === 2 && buffer.length === 960, 'Unexpected output geometry');
          captures.push([0, 1].map(channel => {
            const samples = new Float32Array(960);buffer.copyFromChannel(samples, channel);
            return Array.from(new Uint32Array(samples.buffer));
          }));
          return start(...args);
        };
        return source;
      };
    }
    const originalSchedule = player.schedule.bind(player);
    player.schedule = (...args) => {
      const begin = performance.now();
      try {return originalSchedule(...args);} finally {
        if (begin >= windowStart && begin < windowEnd) {
          assert(timings.length < 512, 'Timing cap');timings.push({at_ms: begin - windowStart, duration_ms: performance.now() - begin});
        }
      }
    };
    return value;
  });};
};

function push() {
  const bytes = payloads[sequence % payloads.length], raw = new ArrayBuffer(20 + bytes.length), view = new DataView(raw);
  new Uint8Array(raw, 0, 4).set([71, 76, 65, 49]);view.setUint8(4, codec === 'pcm' ? 0 : 1);view.setUint8(5, 2);
  view.setUint32(8, 48000, true);view.setUint32(12, sequence++, true);view.setUint32(16, 960, true);
  new Uint8Array(raw, 20).set(bytes);player.push(raw);
}

window.verifySamples = async () => {
  const start = performance.now();
  for (let i = 0; i < 8; i++) {await until(start + i * 20);push();}
  const deadline = start + 1000;
  while (captures.length < 8 && performance.now() < deadline) await until(Math.min(deadline, performance.now() + 5));
  assert(captures.length === 8 && player.pendingDecodes.size === 0 && errors.length === 0, 'Incomplete sample proof');
  return {captures, snapshot: player.snapshot()};
};

window.warmup = async () => {
  const start = performance.now();
  for (let i = 0; i < 75; i++) {await until(start + i * 20);push();}
  await until(start + 1500);return player.snapshot();
};

window.measure = async () => {
  windowStart = performance.now();windowEnd = windowStart + 6000;
  const before = player.snapshot(), sends = [];
  for (let i = 0; i < 300; i++) {
    const planned = windowStart + i * 20;await until(planned);
    const at = performance.now();
    if (at >= windowEnd) break;
    push();sends.push({index: i, at_ms: at - windowStart, lateness_ms: at - planned});
  }
  await until(windowEnd);
  const after = player.snapshot();
  return {start_ms: windowStart, end_ms: windowEnd, before, after, sends,
    schedule_timings: timings, errors: errors.slice(), isolated: crossOriginIsolated,
    physical_speaker_latency_ms: null};
};
window.stopAudio = () => {player?.dispose();return player?.snapshot();};
