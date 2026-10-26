// GLH2 experiment. The immutable exact base never reads composited video pixels.
import {Decoder as BaseDecoder} from './decoder.js';
import {VideoDecoderAdapter, parseVideoPacket} from './video-decoder.js';

export const BASE_TILE_SIZE = 64;
export const MASK_GRIDS = Object.freeze([32,16]);
export const HYBRID_GRID_HEADER_BYTES = 64;
const MAX_PIXELS = 16777216, MAX_TILES = 8192;
const MAX_BASE_BYTES = MAX_PIXELS * 5 + 1048576, MAX_VIDEO_BYTES = 24 + 16 * 1024 * 1024;
export const MAX_HYBRID_GRID_PACKET_BYTES = 64 + MAX_TILES * 2 + MAX_BASE_BYTES + MAX_VIDEO_BYTES;
const fail = (condition, message) => {if (!condition) throw new Error(message);};
const sameMasks = (a, b) => a.length === b.length && a.every((value, index) => value === b[index]);
const sameRoi = (a, b) => a === b || a && b && ['x','y','width','height'].every(name => a[name] === b[name]);

function tileBounds(index, width, height, maskGrid) {
  const columns = Math.ceil(width / maskGrid), x = index % columns * maskGrid, y = Math.floor(index / columns) * maskGrid;
  return {x, y, width: Math.min(maskGrid, width - x), height: Math.min(maskGrid, height - y)};
}
export function hybridGridRoi(masks, width, height, maskGrid) {
  if (!masks.length || !width || !height || !MASK_GRIDS.includes(maskGrid)) return null;
  const count = Math.ceil(width / maskGrid) * Math.ceil(height / maskGrid);
  if (count > MAX_TILES) return null;
  let left = width, top = height, right = 0, bottom = 0;
  for (const index of masks) {
    if (!Number.isInteger(index) || index < 0 || index >= count) return null;
    const tile = tileBounds(index, width, height, maskGrid);
    left = Math.min(left, tile.x); top = Math.min(top, tile.y); right = Math.max(right, tile.x + tile.width); bottom = Math.max(bottom, tile.y + tile.height);
  }
  const axis = (start, end, limit) => {
    end = Math.ceil(end / 2) * 2;
    if (end > limit) return null;
    if (end - start < 16) {
      const extra = 16 - (end - start);
      if (end + extra <= limit) end += extra;
      else if (start >= extra) start -= extra;
      else return null;
    }
    return [start, end - start];
  };
  const horizontal = axis(left, right, width), vertical = axis(top, bottom, height);
  if (!horizontal || !vertical) return null;
  const [x, w] = horizontal, [y, h] = vertical;
  if (Math.max(w,h) > 3840 || Math.min(w,h) > 2160) return null;
  return {x, y, width: w, height: h};
}

// All wrapper/nested header/mask/ROI declarations are checked before decoding.
export function parseHybridGridPacket(buffer) {
  fail(buffer instanceof ArrayBuffer && buffer.byteLength >= 64 && buffer.byteLength <= MAX_HYBRID_GRID_PACKET_BYTES, 'Invalid GLH2 size');
  const bytes = new Uint8Array(buffer), view = new DataView(buffer);
  fail(bytes[0] === 71 && bytes[1] === 76 && bytes[2] === 72 && bytes[3] === 50, 'Unknown hybrid version');
  const sequence = view.getUint32(4,true), inputId = view.getUint32(8,true), width = view.getUint16(12,true), height = view.getUint16(14,true);
  const flags = view.getUint16(18,true), maskCount = view.getUint16(20,true), baseLength = view.getUint32(32,true), videoLength = view.getUint32(36,true);
  const maskGrid = view.getUint16(16,true); fail(MASK_GRIDS.includes(maskGrid), 'GLH2 mask grid must be 16 or 32');
  const tiles = Math.ceil(width / maskGrid) * Math.ceil(height / maskGrid), area = width * height;
  fail(area > 0 && area <= MAX_PIXELS, 'Invalid hybrid geometry');
  fail(!(flags & ~7) && !view.getUint16(22,true) && bytes.subarray(48,64).every(value => value === 0), 'Invalid hybrid flags/reserved/tile size');
  fail(view.getUint32(40,true) === sequence, 'Nested base sequence declaration differs');
  fail(maskCount <= tiles && maskCount <= MAX_TILES && (!maskCount || tiles <= MAX_TILES) && baseLength >= 24 && baseLength <= MAX_BASE_BYTES && videoLength <= MAX_VIDEO_BYTES, 'Hybrid nested count/length exceeds limits');
  fail(64 + maskCount * 2 + baseLength + videoLength === bytes.length, 'Hybrid nested lengths differ');
  const masks = [];
  for (let index = 0; index < maskCount; index++) {
    const mask = view.getUint16(64 + index * 2,true);
    fail(mask < tiles && (!index || masks[index - 1] < mask), 'Hybrid masks must be sorted unique and in range'); masks.push(mask);
  }
  const keyframe = Boolean(flags & 1), hasVideo = Boolean(flags & 2), videoKeyframe = Boolean(flags & 4);
  const declared = {x:view.getUint16(24,true),y:view.getUint16(26,true),width:view.getUint16(28,true),height:view.getUint16(30,true)};
  let roi = null;
  if (hasVideo) {
    fail(maskCount > 0 && videoLength >= 24 && view.getUint32(44,true) === sequence, 'Invalid video activation declaration');
    roi = hybridGridRoi(masks,width,height,maskGrid); fail(roi && sameRoi(roi,declared), 'Hybrid ROI differs from canonical mask bounds');
  } else fail(!maskCount && !videoLength && !videoKeyframe && !view.getUint32(44,true) && Object.values(declared).every(value => value === 0), 'Video-free hybrid has masks/ROI/payload');
  const baseStart = 64 + maskCount * 2, baseBuffer = buffer.slice(baseStart,baseStart + baseLength);
  const base = new Uint8Array(baseBuffer), baseView = new DataView(baseBuffer);
  fail(base[0] === 71 && base[1] === 76 && base[2] === 66 && base[3] === 50 && baseView.getUint32(4,true) === sequence && baseView.getUint32(8,true) === inputId &&
       baseView.getUint16(12,true) === width && baseView.getUint16(14,true) === height && baseView.getUint16(18,true) === Number(keyframe), 'Nested Global declarations differ');
  const baseCount = baseView.getUint16(16,true), expanded = baseView.getUint32(20,true);
  fail(baseCount <= Math.ceil(width / 64) * Math.ceil(height / 64) && expanded >= baseCount * 16 && expanded <= area * 4 + baseCount * 16, 'Nested Global expansion exceeds bound');
  const videoBuffer = hasVideo ? buffer.slice(baseStart + baseLength) : null;
  const video = videoBuffer ? parseVideoPacket(videoBuffer) : null;
  if (video) fail(video.sequence === sequence && video.inputId === inputId && video.width === roi.width && video.height === roi.height && video.keyframe === videoKeyframe && (!keyframe || video.keyframe), 'Nested video declarations/keyframe differ');
  return {sequence,inputId,width,height,maskGrid,keyframe,videoKeyframe,masks,roi,baseBuffer,videoBuffer,baseBytes:baseLength,videoBytes:videoLength,bytes:bytes.length};
}

function softwareCanvas(width,height) {
  if (typeof globalThis.OffscreenCanvas === 'function') return new globalThis.OffscreenCanvas(width,height);
  if (typeof globalThis.document !== 'undefined') {
    const canvas = document.createElement('canvas'); canvas.width = width; canvas.height = height; return canvas;
  }
  throw new Error('Hybrid composition needs a CPU-readable 2D canvas');
}

export class HybridGridDecoder {
  #baseFrame = null;
  #roi = null;
  #masks = [];
  #maskGrid = null;
  get roi() {return this.#roi ? {...this.#roi} : null;}
  get masks() {return this.#masks.slice();}
  get maskGrid() {return this.#maskGrid;}
  constructor({videoFactory = () => new VideoDecoderAdapter(), canvasFactory = softwareCanvas} = {}) {
    if (typeof videoFactory !== 'function' || typeof canvasFactory !== 'function') throw new Error('Invalid hybrid decoder factories');
    this.videoFactory = videoFactory; this.canvasFactory = canvasFactory; this.generation = 0; this.reset();
  }
  reset() {
    this.generation++;
    if (this.pendingVideo && this.pendingVideo !== this.video) this.pendingVideo.close();
    this.video?.close(); this.video = null; this.pendingVideo = null; this.#baseFrame = null;
    this.sequence = null; this.inputId = 0; this.frame = null; this.#maskGrid = null; this.#roi = null; this.#masks = []; this.needsKeyframe = true; this.busy = false; this.closed = false;
  }
  close() {this.reset(); this.closed = true;}
  async decode(buffer) {
    const packet = parseHybridGridPacket(buffer);
    fail(!this.closed && !this.busy, 'Hybrid decoder is closed or busy');
    if (!packet.keyframe) fail(!this.needsKeyframe && this.#maskGrid === packet.maskGrid && this.#baseFrame && this.#baseFrame.width === packet.width && this.#baseFrame.height === packet.height && packet.sequence === ((this.sequence + 1) >>> 0), 'Hybrid delta requires ordered reference/recovery keyframe');
    const changed = this.#maskGrid !== packet.maskGrid || !sameRoi(this.#roi,packet.roi) || !sameMasks(this.#masks,packet.masks);
    fail(!packet.videoBuffer || packet.videoKeyframe || this.video && !changed, 'Video activation/mask/ROI change requires IDR');
    const generation = this.generation; this.busy = true;
    let candidate = null, decodedVideo = null;
    try {
      const staged = new BaseDecoder(); staged.frame = this.#baseFrame; staged.sequence = this.sequence;
      const base = await staged.decode(packet.baseBuffer);
      fail(base.pixels.length === packet.width * packet.height * 4, 'Hybrid base pixel length differs');
      for (let offset = 3; offset < base.pixels.length; offset += 4) fail(base.pixels[offset] === 255, 'Hybrid base is not opaque');
      for (const index of packet.masks) {
        const tile = tileBounds(index,packet.width,packet.height,packet.maskGrid);
        for (let row = tile.y; row < tile.y + tile.height; row++) for (let col = tile.x; col < tile.x + tile.width; col++) {
          const offset = (row * packet.width + col) * 4;
          fail(base.pixels[offset] === 0 && base.pixels[offset + 1] === 0 && base.pixels[offset + 2] === 0 && base.pixels[offset + 3] === 255, 'Active hybrid base tile is not canonical black');
        }
      }
      fail(generation === this.generation, 'Hybrid decoder reset during base staging');
      const pixels = base.pixels.slice();
      if (packet.videoBuffer) {
        candidate = packet.keyframe || changed || !this.video ? this.videoFactory() : this.video;
        this.pendingVideo = candidate;
        decodedVideo = await candidate.decode(packet.videoBuffer);
        fail(generation === this.generation, 'Hybrid decoder reset during video staging');
        fail(decodedVideo.width === packet.roi.width && decodedVideo.height === packet.roi.height && decodedVideo.sequence === packet.sequence && decodedVideo.inputId === packet.inputId, 'Decoded hybrid video declarations differ');
        const canvas = this.canvasFactory(packet.roi.width,packet.roi.height);
        const context = canvas.getContext('2d',{alpha:false,willReadFrequently:true}); fail(context, 'Hybrid CPU-readable canvas unavailable');
        context.drawImage(decodedVideo.videoFrame,0,0,packet.roi.width,packet.roi.height);
        const videoPixels = context.getImageData(0,0,packet.roi.width,packet.roi.height).data;
        fail(videoPixels.length === packet.roi.width * packet.roi.height * 4, 'Hybrid video readback length differs');
        for (const index of packet.masks) {
          const tile = tileBounds(index,packet.width,packet.height,packet.maskGrid);
          for (let row = tile.y; row < tile.y + tile.height; row++) for (let col = tile.x; col < tile.x + tile.width; col++) {
            const source = ((row - packet.roi.y) * packet.roi.width + col - packet.roi.x) * 4, destination = (row * packet.width + col) * 4;
            pixels[destination] = videoPixels[source]; pixels[destination + 1] = videoPixels[source + 1]; pixels[destination + 2] = videoPixels[source + 2]; pixels[destination + 3] = 255;
          }
        }
      }
      fail(generation === this.generation, 'Hybrid decoder reset before commit');
      const frame = {width:packet.width,height:packet.height,pixels,sequence:packet.sequence,inputId:packet.inputId,keyframe:packet.keyframe,
        bytes:packet.bytes,tiles:base.tiles + packet.masks.length,hybrid:true,hybridGrid:true,maskGrid:packet.maskGrid,activeVideoTiles:packet.masks.length,videoMasks:packet.masks.slice(),roi:packet.roi ? {...packet.roi} : null,
        videoKeyframe:packet.videoKeyframe,baseBytes:packet.baseBytes,videoBytes:packet.videoBytes,
        damage:packet.videoBuffer || this.#masks.length ? {x:0,y:0,width:packet.width,height:packet.height} : base.damage};
      if (this.video && this.video !== candidate) this.video.close();
      this.video = candidate; this.#baseFrame = {width:base.width,height:base.height,pixels:base.pixels};
      this.sequence = packet.sequence; this.inputId = packet.inputId; this.#maskGrid = packet.maskGrid; this.frame = frame; this.#roi = packet.roi; this.#masks = packet.masks; this.needsKeyframe = false;
      return frame;
    } catch (error) {
      if (generation === this.generation) {this.needsKeyframe = true; candidate?.close();}
      else if (candidate && candidate !== this.video) candidate.close();
      error.requiresHybridKeyframe = true; throw error;
    } finally {
      decodedVideo?.videoFrame?.close();
      if (generation === this.generation) {this.pendingVideo = null; this.busy = false;}
    }
  }
}

export {HybridGridDecoder as Decoder};
