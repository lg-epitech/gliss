// IMA prediction, Gliss stereo packing: eight header bytes, then one frame per
// byte with left in the low nibble and right in the high nibble. No prior packet
// or codec API is needed. Header predictors are the exact first source samples.
const STEPS = [7,8,9,10,11,12,13,14,16,17,19,21,23,25,28,31,34,37,41,45,50,55,60,66,73,80,88,97,107,
  118,130,143,157,173,190,209,230,253,279,307,337,371,408,449,494,544,598,658,724,796,876,963,1060,
  1166,1282,1411,1552,1707,1878,2066,2272,2499,2749,3024,3327,3660,4026,4428,4871,5358,5894,6484,
  7132,7845,8630,9493,10442,11487,12635,13899,15289,16818,18500,20350,22385,24623,27086,29794,32767];
const INDEX = [-1,-1,-1,-1,2,4,6,8];

export function decodeAdpcm(bytes, frames) {
  if (!(bytes instanceof Uint8Array) || !Number.isInteger(frames) || frames < 1 || frames > 5760 || bytes.length !== frames + 7)
    throw new Error('Invalid ADPCM block length.');
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const output = [new Float32Array(frames), new Float32Array(frames)];
  for (let channel = 0; channel < 2; channel++) {
    let predictor = view.getInt16(channel * 4, true), index = bytes[channel * 4 + 2];
    if (index > 88 || bytes[channel * 4 + 3] !== 0) throw new Error('Invalid ADPCM state.');
    output[channel][0] = predictor / 32768;
    for (let frame = 1; frame < frames; frame++) {
      const nibble = (bytes[frame + 7] >> (channel * 4)) & 15, step = STEPS[index];
      const delta = (step >> 3) + ((nibble & 4) ? step : 0) + ((nibble & 2) ? step >> 1 : 0) + ((nibble & 1) ? step >> 2 : 0);
      predictor = Math.min(32767, Math.max(-32768, predictor + ((nibble & 8) ? -delta : delta)));
      index = Math.min(88, Math.max(0, index + INDEX[nibble & 7]));
      output[channel][frame] = predictor / 32768;
    }
  }
  return output;
}
