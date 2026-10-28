export const MARKER_FRAMES = 960;
export const MARKER_CODES = 48;

// Two independent channel frequencies encode each marker. A 20 ms window
// contains an integer number of cycles for every candidate frequency.
export function classifyMarker(channels, sampleRate = 48000) {
  if (sampleRate !== 48000 || channels.length !== 2 || channels.some(row => row.length !== MARKER_FRAMES))
    throw new Error('Marker detector requires 20 ms of 48 kHz stereo');
  const picked = [], quality = [];
  for (let channel = 0; channel < 2; channel++) {
    const samples = channels[channel];
    let energy = 0;
    for (const value of samples) {if (!Number.isFinite(value)) return {code: null};energy += value * value;}
    const powers = [];
    for (let code = 1; code <= MARKER_CODES; code++) {
      const frequency = channel === 0 ? 1000 + (code - 1) * 100 : 11000 - (code - 1) * 100;
      const coefficient = 2 * Math.cos(2 * Math.PI * frequency / sampleRate);
      let previous = 0, older = 0;
      for (const value of samples) {const next = value + coefficient * previous - older;older = previous;previous = next;}
      powers.push(Math.max(0, previous * previous + older * older - coefficient * previous * older));
    }
    const sorted = powers.map((power, i) => ({power, code: i + 1})).sort((a, b) => b.power - a.power);
    const peak = sorted[0].power, rival = sorted[1].power;
    const amplitude = 2 * Math.sqrt(peak) / MARKER_FRAMES;
    const concentration = energy ? 2 * peak / (MARKER_FRAMES * energy) : 0;
    if (amplitude < 0.04 || concentration < 0.65 || peak < rival * 8) return {code: null};
    picked.push(sorted[0].code);quality.push({amplitude, concentration});
  }
  return picked[0] === picked[1] ? {code: picked[0], quality} : {code: null};
}
