import {AudioPlayer} from '/web/audio-player.js';

const state = {outputs: [], schedules: [], sends: [], errors: []};
const RealDecoder = AudioDecoder;
window.AudioDecoder = new Proxy(RealDecoder, {construct(Target, [callbacks]) {
  return new Target({...callbacks, output(audio) {
    if (state.outputs.length >= 32) throw new Error('Output trace cap');
    state.outputs.push({timestamp: audio.timestamp, frames: audio.numberOfFrames,
      channels: audio.numberOfChannels, sample_rate: audio.sampleRate});
    callbacks.output(audio);
  }});
}});
const player = new AudioPlayer(message => {if (message.message) state.errors.push(message.message);});
player.status({available: true, codecs: ['opus', 'pcm']});
const schedule = player.schedule.bind(player);
player.schedule = (frames, samples) => {state.schedules.push(frames);return schedule(frames, samples);};
document.querySelector('#sound').onclick = () => {window.soundReady = player.enable();};
const until = async end => {while (performance.now() < end) await new Promise(resolve => setTimeout(resolve, Math.min(20, end - performance.now())));};
window.runPackets = async packets => {
  if (player.codec !== 'opus') throw new Error('WebCodecs Opus required');
  const start = performance.now();let due = start;
  for (const bytes of packets) {
    const buffer = new Uint8Array(bytes).buffer, header = new DataView(buffer), frames = header.getUint32(16, true);
    await until(due);
    state.sends.push({sequence: header.getUint32(12, true), frames,
      timestamp: Math.round(player.timestamp * 1000000 / 48000), late_ms: performance.now() - due});
    player.push(buffer);due += frames * 1000 / 48000;
  }
  await until(due + 250);
  return {...state, elapsed_ms: performance.now() - start, snapshot: player.snapshot()};
};
window.stopAudio = () => {player.dispose();return player.snapshot();};
