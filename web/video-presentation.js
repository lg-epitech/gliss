// Opt-in CPU Canvas path. The caller still owns an unconverted VideoFrame.
export class RgbaVideoPresentation {
  constructor() {this.pixels = null;this.unsupported = false;}
  reset() {this.pixels = null;this.unsupported = false;}
  async prepare(frame) {
    const video = frame.videoFrame, rect = video?.visibleRect;
    if (!video || this.unsupported || video.format !== 'I420' || typeof video.copyTo !== 'function' ||
        rect?.width !== frame.width || rect?.height !== frame.height ||
        !Number.isInteger(frame.width) || !Number.isInteger(frame.height) || frame.width < 16 || frame.height < 16 ||
        frame.width * frame.height > 3840 * 2160) return frame;
    const size = frame.width * frame.height * 4;
    if (this.pixels?.length !== size) this.pixels = new Uint8ClampedArray(size);
    // Keep the destination alive if disconnect/reset releases the reusable slot.
    const pixels = this.pixels;
    try {await video.copyTo(pixels, {format: 'RGBA', layout: [{offset: 0, stride: frame.width * 4}]});}
    catch {this.unsupported = true;this.pixels = null;return frame;}
    video.close();
    // VideoFrame drawing covers the whole picture, including damage-mode clients.
    return {...frame, videoFrame: undefined, pixels, damage: {x: 0, y: 0, width: frame.width, height: frame.height}};
  }
}
