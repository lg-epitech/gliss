// Source counters measure the marker region, not whole-frame image correctness.
export function decodeMotionStamp(bytes) {
  const width = 372, height = 20;
  if (!(bytes instanceof Uint8ClampedArray) || bytes.length !== width * height * 4) return null;
  const mean = x => {
    const channels = [0, 0, 0];
    for (let dy = -1; dy <= 1; dy++) for (let dx = -1; dx <= 1; dx++) {
      const offset = ((10 + dy) * width + x + dx) * 4;
      for (let channel = 0; channel < 3; channel++) channels[channel] += bytes[offset + channel] / 9;
    }
    return channels;
  };
  const start = mean(10), end = mean(362);
  if (!(start[0] > 140 && start[1] < 100 && start[2] > 140 && end[0] < 100 && end[1] > 140 && end[2] > 140)) return null;
  let value = 0;
  for (let bit = 0; bit < 32; bit++) {
    const color = mean(29 + bit * 10), light = (color[0] + color[1] + color[2]) / 3;
    if (light > 160) value |= 1 << bit;
    else if (light >= 96) return null;
  }
  value >>>= 0;
  const frame = value & 0xffffff;
  return (value >>> 24) === ((frame & 255) ^ ((frame >>> 8) & 255) ^ ((frame >>> 16) & 255) ^ 0x5a) ? frame : null;
}

export function readMotionStamp(context) {
  // The owned guest's RFB surface is 1280x800; fbdev's rendering area is
  // 1024x768. The raw RFB readiness check pins the surface independently.
  if (context.canvas.width !== 1280 || context.canvas.height !== 800) return null;
  return decodeMotionStamp(context.getImageData(8, 8, 372, 20).data);
}
