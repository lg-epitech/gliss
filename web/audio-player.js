// Audio is independent of the video decode/paint queue. Keep at most 200 ms
// scheduled so a network burst or a background tab cannot build up old sound.
const HEADER_BYTES = 20;
const SAMPLE_RATE = 48000;
const CHANNELS = 2;
const MAX_FRAMES = 5760;
const MAX_BUFFER_SECONDS = 0.2;
const START_DELAY_SECONDS = 0.02;
const MAX_DECODE_QUEUE = 8;

export function isAudioPacket(data) {
  if (!(data instanceof ArrayBuffer) || data.byteLength < 4) return false;
  const magic = new Uint8Array(data, 0, 4);
  return magic[0] === 71 && magic[1] === 76 && magic[2] === 65 && magic[3] === 49;
}

export class AudioPlayer {
  constructor(onState = () => {}) {
    this.onState = onState;
    this.available = false;
    this.enabled = false;
    this.blocked = false;
    this.codec = null;
    this.context = null;
    this.decoder = null;
    this.sources = new Set();
    this.epoch = 0;
    this.decoderEpoch = 0;
    this.nextTime = 0;
    this.timestamp = 0;
    this.sequence = null;
    this.received = 0;
    this.scheduled = 0;
    this.dropped = 0;
  }

  state(message) {
    this.onState({type: 'audio_state', available: this.available,
      enabled: this.enabled, blocked: this.blocked, ...(message ? {message} : {})});
  }

  status(status) {
    this.available = status.available === true;
    // A delayed acknowledgement of an earlier mute must not cancel a new
    // user request. Explicit failure/unavailability still stops playback.
    if ((!this.available || (status.enabled === false && status.message)) && this.enabled) {
      this.stop();
    }
    this.state(status.message);
  }

  // Call directly from the click handler: context creation and resume must
  // happen before the first await while the browser's user activation is live.
  async enable() {
    const epoch = ++this.epoch;
    this.enabled = false;
    this.closeResources();
    this.blocked = false;
    try {
      const Context = globalThis.AudioContext || globalThis.webkitAudioContext;
      if (!Context) throw new Error('This browser does not support audio playback.');
      const context = this.context = new Context({latencyHint: 'interactive', sampleRate: SAMPLE_RATE});
      const resumed = context.resume();
      // resume() can remain pending indefinitely when autoplay is blocked.
      let timer;
      await Promise.race([resumed, new Promise(resolve => {timer = setTimeout(resolve, 300);})])
        .finally(() => clearTimeout(timer));
      if (epoch !== this.epoch) return null;
      if (context.state !== 'running') {
        this.blocked = true;
        throw new Error('Click Sound again to allow audio playback.');
      }
      let codec = 'pcm';
      if (globalThis.AudioDecoder && globalThis.EncodedAudioChunk) {
        try {
          const support = await AudioDecoder.isConfigSupported({codec: 'opus', sampleRate: SAMPLE_RATE, numberOfChannels: CHANNELS});
          if (support.supported) codec = 'opus';
        } catch { /* PCM uses Web Audio in browsers without an Opus decoder. */ }
      }
      if (epoch !== this.epoch) return null;
      this.codec = codec;
      this.enabled = true;
      if (codec === 'opus') this.createDecoder();
      context.onstatechange = () => {
        if (epoch !== this.epoch || !this.enabled || context.state === 'running') return;
        this.blocked = true;
        this.fail('Click Sound again to resume audio playback.');
      };
      this.state();
      return codec;
    } catch (error) {
      if (epoch === this.epoch) this.fail(error.message);
      return null;
    }
  }

  stop() {
    ++this.epoch;
    this.enabled = false;
    this.blocked = false;
    this.closeResources();
  }

  closeResources() {
    this.clearScheduled();
    ++this.decoderEpoch;
    if (this.decoder && this.decoder.state !== 'closed') this.decoder.close();
    this.decoder = null;
    const context = this.context;
    this.context = null;
    if (context) {
      context.onstatechange = null;
      void context.close().catch(() => {});
    }
    this.timestamp = 0;
    this.sequence = null;
    this.codec = null;
  }

  dispose() {
    this.stop();
    this.available = false;
    this.state();
  }

  fail(message) {
    const blocked = this.blocked;
    this.stop();
    this.blocked = blocked;
    this.state(message);
  }

  clearScheduled() {
    for (const source of this.sources) {
      source.onended = null;
      try { source.stop(); } catch {}
      source.disconnect();
    }
    this.sources.clear();
    this.nextTime = 0;
  }

  createDecoder() {
    ++this.decoderEpoch;
    if (this.decoder && this.decoder.state !== 'closed') this.decoder.close();
    const epoch = this.decoderEpoch;
    this.decoder = new AudioDecoder({
      output: audio => {
        try {
          if (epoch !== this.decoderEpoch || !this.enabled) return;
          if (audio.sampleRate !== SAMPLE_RATE || audio.numberOfChannels !== CHANNELS || audio.numberOfFrames > MAX_FRAMES)
            throw new Error('Invalid decoded audio format.');
          this.schedule(audio.numberOfFrames, channel => {
            const samples = new Float32Array(audio.numberOfFrames);
            audio.copyTo(samples, {planeIndex: channel, format: 'f32-planar'});
            return samples;
          });
        } catch (error) { this.fail(error.message); }
        finally { audio.close(); }
      },
      error: error => {if (epoch === this.decoderEpoch) this.fail(`Audio decoder failed: ${error.message}`);},
    });
    this.decoder.configure({codec: 'opus', sampleRate: SAMPLE_RATE, numberOfChannels: CHANNELS});
  }

  push(data) {
    if (!this.enabled || this.context?.state !== 'running') return;
    try {
      if (data.byteLength < HEADER_BYTES) throw new Error('Truncated audio packet.');
      const header = new DataView(data);
      const codec = header.getUint8(4), channels = header.getUint8(5);
      const rate = header.getUint32(8, true), sequence = header.getUint32(12, true), frames = header.getUint32(16, true);
      if (header.getUint16(6, true) !== 0 || channels !== CHANNELS || rate !== SAMPLE_RATE ||
          frames < 1 || frames > MAX_FRAMES || codec !== (this.codec === 'opus' ? 1 : 0))
        throw new Error('Invalid audio packet format.');
      const length = data.byteLength - HEADER_BYTES;
      if ((codec === 0 && length !== frames * channels * 2) || (codec === 1 && (length < 1 || length > 4096)))
        throw new Error('Invalid audio payload length.');
      this.received++;
      if ((this.sequence !== null && sequence !== ((this.sequence + 1) >>> 0)) ||
          (this.decoder?.decodeQueueSize ?? 0) >= MAX_DECODE_QUEUE) {
        this.dropped++;
        this.clearScheduled();
        if (codec === 1) this.createDecoder();
      }
      this.sequence = sequence;
      if (codec === 0) {
        this.schedule(frames, channel => {
          const samples = new Float32Array(frames);
          for (let frame = 0; frame < frames; frame++) samples[frame] = header.getInt16(HEADER_BYTES + (frame * CHANNELS + channel) * 2, true) / 32768;
          return samples;
        });
      } else {
        this.decoder.decode(new EncodedAudioChunk({type: 'key', timestamp: Math.round(this.timestamp * 1000000 / SAMPLE_RATE),
          duration: Math.round(frames * 1000000 / SAMPLE_RATE), data: new Uint8Array(data, HEADER_BYTES)}));
        this.timestamp += frames;
      }
    } catch (error) { this.fail(error.message); }
  }

  schedule(frames, samples) {
    const context = this.context;
    if (!context || context.state !== 'running') return;
    const duration = frames / SAMPLE_RATE;
    const now = context.currentTime;
    if (this.nextTime < now || this.nextTime + duration - now > MAX_BUFFER_SECONDS || this.sources.size >= 12) {
      if (this.sources.size) this.dropped++;
      this.clearScheduled();
      this.nextTime = now + START_DELAY_SECONDS;
    }
    const buffer = context.createBuffer(CHANNELS, frames, SAMPLE_RATE);
    for (let channel = 0; channel < CHANNELS; channel++) buffer.copyToChannel(samples(channel), channel);
    const source = context.createBufferSource();
    source.buffer = buffer;
    source.connect(context.destination);
    source.onended = () => {this.sources.delete(source);source.disconnect();};
    this.sources.add(source);
    source.start(this.nextTime);
    this.nextTime += duration;
    this.scheduled++;
  }

  snapshot() {
    return {available: this.available, enabled: this.enabled, codec: this.codec,
      contextState: this.context?.state ?? 'closed', sources: this.sources.size,
      queuedSeconds: this.context ? Math.max(0, this.nextTime - this.context.currentTime) : 0,
      decodeQueue: this.decoder?.decodeQueueSize ?? 0,
      received: this.received, scheduled: this.scheduled, dropped: this.dropped};
  }
}
