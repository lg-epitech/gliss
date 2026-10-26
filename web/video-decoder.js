// GLV1 conventional software-encoded H.264. The returned VideoFrame is owned by
// the caller: draw it, then close it. prefer-software remains a browser hint.
const HEADER_LEN = 24;
const MAX_PAYLOAD = 16 * 1024 * 1024;
const MAX_PENDING = 2;

function startCode(bytes, pos) {
  if (bytes[pos] !== 0 || bytes[pos + 1] !== 0) return 0;
  if (bytes[pos + 2] === 1) return 3;
  return bytes[pos + 2] === 0 && bytes[pos + 3] === 1 ? 4 : 0;
}

function* nalUnits(bytes) {
  let pos = 0, count = 0;
  while (pos < bytes.length) {
    const prefix = startCode(bytes, pos);
    if (!prefix || pos + prefix >= bytes.length || ++count > 4096) throw new Error('Invalid Annex B access unit');
    const begin = pos + prefix;
    let end = begin + 1;
    while (end < bytes.length && !startCode(bytes, end)) end++;
    const nal = bytes.subarray(begin, end);
    if (nal[0] & 128) throw new Error('Invalid H.264 NAL header');
    yield nal;
    pos = end;
  }
}

// Framing validation precedes native decoder submission. Full bitstream
// validation belongs to VideoDecoder; this parser validates advertised IDR/SPS.
export function parseVideoPacket(buffer) {
  if (!(buffer instanceof ArrayBuffer) || buffer.byteLength < HEADER_LEN ||
      buffer.byteLength > HEADER_LEN + MAX_PAYLOAD) throw new Error('Invalid GLV1 packet size');
  const bytes = new Uint8Array(buffer), view = new DataView(buffer);
  if (bytes[0] !== 71 || bytes[1] !== 76 || bytes[2] !== 86 || bytes[3] !== 49) throw new Error('Unknown video wire version');
  const sequence = view.getUint32(4, true), inputId = view.getUint32(8, true);
  const width = view.getUint16(12, true), height = view.getUint16(14, true);
  const flags = view.getUint16(18, true), length = view.getUint32(20, true);
  if (width < 16 || height < 16 || width % 2 || height % 2 ||
      Math.max(width, height) > 3840 || Math.min(width, height) > 2160 ||
      view.getUint16(16, true) !== 0 || flags > 1 ||
      length !== bytes.length - HEADER_LEN || !length) throw new Error('Invalid video frame header');
  const payload = bytes.subarray(HEADER_LEN);
  let idr = false, pps = false, slice = false, codec = null;
  for (const nal of nalUnits(payload)) {
    const type = nal[0] & 31;
    if (type === 5) { idr = true; slice = true; }
    else if (type === 1) slice = true;
    else if (type === 8) pps = true;
    else if (type === 7) {
      if (nal.length < 4 || nal[1] !== 66) throw new Error('GLV1 requires Baseline H.264 SPS');
      codec = `avc1.${[nal[1], nal[2], nal[3]].map(value => value.toString(16).padStart(2, '0')).join('')}`;
    }
  }
  const keyframe = flags === 1;
  if (!slice || idr !== keyframe || (keyframe && (!codec || !pps))) throw new Error('Invalid video IDR/parameter sets');
  return { sequence, inputId, width, height, keyframe, codec, payload, bytes: bytes.length };
}

export class VideoDecoderAdapter {
  constructor({ timeoutMs = 5000, configureTimeoutMs = null } = {}) {
    if (!Number.isFinite(timeoutMs) || timeoutMs <= 0) throw new Error('Invalid decoder timeout');
    if (configureTimeoutMs !== null && (!Number.isFinite(configureTimeoutMs) || configureTimeoutMs <= 0)) throw new Error('Invalid decoder configuration timeout');
    this.timeoutMs = timeoutMs;
    this.configureTimeoutMs = configureTimeoutMs;
    this.generation = 0;
    this.pending = new Map();
    this.reset();
  }

  reset() {
    this.generation++;
    for (const entry of this.pending.values()) {
      clearTimeout(entry.timer);
      entry.reject(new Error('Video decoder reset'));
    }
    this.pending.clear();
    if (this.decoder && this.decoder.state !== 'closed') this.decoder.close();
    this.decoder = null;
    this.sequence = null;
    this.width = null;
    this.height = null;
    this.codec = null;
    this.configuration = null;
    this.configuring = false;
    this.failed = false;
    this.closed = false;
  }

  close() {
    this.reset();
    this.closed = true;
  }

  _fail(error) {
    this.failed = true;
    for (const entry of this.pending.values()) {
      clearTimeout(entry.timer);
      entry.reject(error);
    }
    this.pending.clear();
    if (this.decoder && this.decoder.state !== 'closed') this.decoder.close();
  }

  async _configure(packet) {
    if (typeof globalThis.VideoDecoder !== 'function' || typeof globalThis.EncodedVideoChunk !== 'function') {
      throw new Error('This browser does not expose WebCodecs video decoding');
    }
    const generation = this.generation;
    this.configuring = true;
    try {
      const configuration = {
        codec: packet.codec, codedWidth: packet.width, codedHeight: packet.height,
        hardwareAcceleration: 'prefer-software', optimizeForLatency: true,
        // No description means H.264 Annex B rather than AVC length prefixes.
      };
      let supported;
      if (this.configureTimeoutMs === null) supported = await globalThis.VideoDecoder.isConfigSupported(configuration);
      else {
        let timer;
        try {
          supported = await Promise.race([globalThis.VideoDecoder.isConfigSupported(configuration),
            new Promise((_, reject) => { timer = setTimeout(() => reject(new Error('Video decoder configuration timed out')), this.configureTimeoutMs); })]);
        } finally { clearTimeout(timer); }
      }
      if (generation !== this.generation) throw new Error('Video decoder reset during configuration');
      if (!supported.supported) throw new Error(`Unsupported software-preferred H.264 configuration: ${packet.codec}`);
      if (this.decoder && this.decoder.state !== 'closed') this.decoder.close();
      this.decoder = new globalThis.VideoDecoder({
        output: frame => {
          if (generation !== this.generation) { frame.close(); return; }
          const entry = this.pending.get(frame.timestamp);
          if (!entry) { frame.close(); return; }
          this.pending.delete(frame.timestamp);
          clearTimeout(entry.timer);
          if (frame.displayWidth !== entry.packet.width || frame.displayHeight !== entry.packet.height) {
            frame.close();
            const error = new Error('Decoded video dimensions differ from GLV1 header');
            entry.reject(error);
            this._fail(error);
            return;
          }
          const { width, height, sequence, inputId, keyframe, bytes } = entry.packet;
          entry.resolve({ width, height, videoFrame: frame, sequence, inputId, keyframe, tiles: 1, bytes });
        },
        error: error => { if (generation === this.generation) this._fail(error); },
      });
      this.decoder.configure(configuration);
      this.configuration = supported.config;
      this.width = packet.width;
      this.height = packet.height;
      this.codec = packet.codec;
      this.sequence = null;
    } finally {
      if (generation === this.generation) this.configuring = false;
    }
  }

  async decode(buffer) {
    const packet = parseVideoPacket(buffer);
    if (this.closed || this.failed) throw new Error('Reset the video decoder before reuse');
    if (this.configuring) throw new Error('Video decoder is configuring');
    if (this.pending.size >= MAX_PENDING || (this.decoder?.decodeQueueSize ?? 0) >= MAX_PENDING) {
      throw new Error('Video decoder queue limit reached');
    }
    const changed = !this.decoder || this.width !== packet.width || this.height !== packet.height ||
                    (packet.codec && this.codec !== packet.codec);
    if (changed) {
      if (!packet.keyframe) throw new Error('Video configuration change requires an IDR frame');
      if (this.pending.size) throw new Error('Cannot reconfigure while video frames are pending');
      await this._configure(packet);
    }
    if (!packet.keyframe && (this.sequence === null || packet.sequence !== ((this.sequence + 1) >>> 0))) {
      throw new Error('Missing video reference frame');
    }
    if (this.pending.has(packet.sequence)) throw new Error('Duplicate pending video sequence');
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => this._fail(new Error('Video decoder output timed out')), this.timeoutMs);
      this.pending.set(packet.sequence, { packet, resolve, reject, timer });
      try {
        this.decoder.decode(new globalThis.EncodedVideoChunk({
          type: packet.keyframe ? 'key' : 'delta', timestamp: packet.sequence, data: packet.payload,
        }));
        this.sequence = packet.sequence;
      } catch (error) {
        this._fail(error);
      }
    });
  }
}
