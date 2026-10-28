import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {performance} from 'node:perf_hooks';

export async function installMarkerObserver(page, onMarker) {
  const source = await readFile(new URL('detector.js', import.meta.url));
  await page.route('**/gliss-audio-probe/detector.js', route =>
    route.fulfill({status: 200, contentType: 'text/javascript', body: source}));
  await page.exposeFunction('glissMarkerObservation', message => {
    assert.ok(message && Number.isInteger(message.code) && message.code >= 1 && message.code <= 48);
    assert.ok(Number.isSafeInteger(message.analysis_context_frame) && message.analysis_context_frame >= 0);
    assert.equal(message.sample_rate, 48000);
    onMarker({...message, host_received_ms: performance.now()});
  });
  return page.evaluate(async () => {
    const {classifyMarker, MARKER_FRAMES} = await import('/gliss-audio-probe/detector.js');
    const context = window.gliss.audio.context;
    if (!context || context.state !== 'running' || context.sampleRate !== 48000)
      throw new Error('A running 48 kHz context is required');
    const split = context.createChannelSplitter(2), mute = context.createGain();
    mute.gain.value = 0;mute.connect(context.destination);
    const analysers = [context.createAnalyser(), context.createAnalyser()];
    for (let channel = 0; channel < 2; channel++) {
      analysers[channel].fftSize = 2048;analysers[channel].smoothingTimeConstant = 0;
      split.connect(analysers[channel], channel);analysers[channel].connect(mute);
    }
    const buffers = [new Float32Array(2048), new Float32Array(2048)];
    const windows = buffers.map(buffer => buffer.subarray(2048 - MARKER_FRAMES));
    const original = context.createBufferSource.bind(context);
    const create = () => {
      const node = original(), connect = node.connect.bind(node);
      node.connect = (...args) => {
        const result = connect(...args);
        if (args[0] === context.destination) connect(split);
        return result;
      };
      return node;
    };
    context.createBufferSource = create;
    let timer, previousContext = -Infinity, previousPoll = null, candidate = null, consecutive = 0, announced = null;
    const state = window.glissMarkerProbe = {received: 0, last: null, lastCode: null, failure: null, stopped: false,
      polls: 0, max_poll_gap_ms: 0, quality_windows: [], quality_windows_dropped: 0,
      stop() {
        if (state.stopped) return;state.stopped = true;clearInterval(timer);
        if (context.createBufferSource === create) context.createBufferSource = original;
        split.disconnect();for (const analyser of analysers) analyser.disconnect();mute.disconnect();
      }};
    const poll = () => {
      if (state.stopped || context.state !== 'running') return;
      const audioTime = context.currentTime;
      // Accepted windows are at least 20 ms apart in the audio clock.
      if (audioTime - previousContext < MARKER_FRAMES / context.sampleRate) return;
      const begin = performance.now();
      if (previousPoll !== null) state.max_poll_gap_ms = Math.max(state.max_poll_gap_ms, begin - previousPoll);
      previousPoll = begin;previousContext = audioTime;state.polls++;
      try {
        for (let channel = 0; channel < 2; channel++) analysers[channel].getFloatTimeDomainData(buffers[channel]);
        const detected = classifyMarker(windows, context.sampleRate);
        if (detected.code !== null) {
          const quality = {code: detected.code, context_time_seconds: audioTime, quality: detected.quality};
          if (state.quality_windows.length < 256) state.quality_windows.push(quality);
          else state.quality_windows_dropped++;
        }
        consecutive = detected.code !== null && detected.code === candidate ? consecutive + 1 : 1;
        candidate = state.lastCode = detected.code;
        if (detected.code === null) {announced = null;return;}
        if (consecutive < 2 || detected.code === announced) return;
        announced = detected.code;
        if (++state.received > 128) throw new Error('Observation cap');
        state.last = {code: detected.code, analysis_context_frame: Math.round(audioTime * context.sampleRate),
          context_time_seconds: audioTime, sample_rate: context.sampleRate, quality: detected.quality,
          analysis_begin_browser_ms: begin, analysis_end_browser_ms: performance.now()};
        void window.glissMarkerObservation(state.last).catch(() => {state.failure = 'observer-bridge-error';});
      } catch {state.failure = 'detector-error';state.stop();}
    };
    timer = setInterval(poll, 10);poll();
    return {sample_rate: context.sampleRate, base_latency_seconds: context.baseLatency,
      output_latency_seconds: context.outputLatency ?? null, analysis_frames: MARKER_FRAMES,
      poll_interval_ms: 10, minimum_confirmation_windows: 2,
      qualification: 'Web Audio analyser plus main-thread polling and controller IPC; not physical speaker output.'};
  });
}
