// Observation only: the production client's ACK, paint and audio paths stay intact.
import {readFile} from 'node:fs/promises';

export async function installMotionObserver(page) {
  const source = await readFile(new URL('../qemu-audio-motion/stamp.js', import.meta.url), 'utf8');
  await page.route('**/gliss-motion-probe/stamp.js', route => route.fulfill({contentType:'text/javascript',body:source}));
  await page.evaluate(async () => {window.readMotionStamp = (await import('/gliss-motion-probe/stamp.js')).readMotionStamp;});
}

export async function startMotionWindow(page, plan) {
  return page.evaluate(plan => {
    const client = window.gliss, start = performance.now(), end = start + plan.window_ms;
    const pings = Array.from({length:plan.control_probes_per_position}, (_,i) => ({id:i,planned_ms:i*plan.ping_interval_ms,
      sent_ms:null,wire_received_ms:null,dispatched_ms:null}));
    const videoSnapshot = () => ({frames:client.metrics.frames,bytes:client.metrics.bytes,errors:client.metrics.errors});
    const state = {start_ms:start,end_ms:end,wire_bytes:0,wire_frames:0,video_wire_bytes:0,samples:[],paints:[],arrivals:[],pings,skip_samples:[],invalid_skip_stats:0,invalid_video_headers:0,
      overflow:false,finished:false,audio_before:client.audio.snapshot(),video_before:videoSnapshot(),
      stamp_before:window.readMotionStamp(client.ctx)};
    const original = client.onEvent;
    const dispatch = value => {
      original(value);
      if (state.finished) return;
      const now = performance.now();
      if (now >= end) return;
      if (value.type === 'pong' && Number.isInteger(value.id) && pings[value.id] && pings[value.id].sent_ms !== null)
        pings[value.id].dispatched_ms ??= now-start;
      if (value.type === 'frame') {
        const at = client.presentationState?.presentedAtMs;
        if (at < start || at >= end) return;
        if (state.paints.length >= plan.paint_cap_per_position) {state.overflow = true;return;}
        const arrival = state.arrivals.find(row => row.sequence === value.frame.sequence);
        state.paints.push({at_ms:at-start,observed_ms:now-start,source_frame:window.readMotionStamp(client.ctx),
          sequence:value.frame.sequence,bytes:value.frame.bytes,decode_ms:value.decodeMs,
          wire_received_ms:arrival?.at_ms ?? null});
      }
    };
    client.onEvent = dispatch;
    const receive = event => {
      const at = performance.now();if (state.finished || at >= end) return;
      if (typeof event.data === 'string') {
        try {const value = JSON.parse(event.data);
          if (value.type === 'pong' && Number.isInteger(value.id) && pings[value.id] && pings[value.id].sent_ms !== null)
            pings[value.id].wire_received_ms ??= at-start;
          if (value.type === 'stream_stats') {
            const skipped = value.native_skipped_frames;
            if (plan.frame_skip[plan.current_mode]) {
              if (!Number.isSafeInteger(skipped) || skipped < 0 ||
                (state.skip_samples.length && skipped < state.skip_samples.at(-1).count)) state.invalid_skip_stats++;
              else if (state.skip_samples.length < 16) state.skip_samples.push({at_ms:at-start,count:skipped});
              else state.overflow = true;
            } else if (skipped !== undefined) state.invalid_skip_stats++;
          }
        } catch { /* The production parser retains protocol errors. */ }
        return;
      }
      if (!(event.data instanceof ArrayBuffer) || event.data.byteLength < 20) return;
      const view = new DataView(event.data);
      if (view.getUint32(0,true) === 0x31414c47) {state.wire_bytes += event.data.byteLength;state.wire_frames += view.getUint32(16,true);}
      else if (view.getUint32(0,true) === 0x31564c47) {
        if (event.data.byteLength < 25 || view.getUint32(20,true) !== event.data.byteLength-24 ||
            view.getUint16(16,true) !== 0 || (view.getUint16(18,true)&~1) !== 0) {state.invalid_video_headers++;return;}
        state.video_wire_bytes += event.data.byteLength;
        if (state.arrivals.length >= plan.paint_cap_per_position) state.overflow = true;
        else state.arrivals.push({sequence:view.getUint32(4,true),at_ms:at-start,bytes:event.data.byteLength,keyframe:(view.getUint16(18,true)&1)===1});
      }
    };
    // The constructor-installed hook is registered before the production
    // client's onmessage handler; EventTarget capture flags alone are insufficient.
    window.motionWireTap = receive;
    const sampler = setInterval(() => {if (performance.now() < end)
      state.samples.push({...client.audio.snapshot(),at_ms:performance.now()-start});},plan.sample_interval_ms);
    let timer, index = 0;
    function pulse() {
      if (state.finished || performance.now() >= end || index >= pings.length) return;
      const ping = pings[index++];ping.sent_ms = performance.now()-start;
      client.socket.send(JSON.stringify({type:'ping',id:ping.id}));
      if (index < pings.length) timer = setTimeout(pulse,Math.max(0,start+pings[index].planned_ms-performance.now()));
    }
    pulse();
    state.finish = () => {
      state.finished = true;clearInterval(sampler);clearTimeout(timer);
      if (window.motionWireTap === receive) window.motionWireTap = null;
      if (client.onEvent === dispatch) client.onEvent = original;
      const valid = state.paints.filter(row => row.source_frame !== null), codes = new Set(valid.map(row => row.source_frame));
      return {start_ms:start,end_ms:end,wire_bytes:state.wire_bytes,wire_frames:state.wire_frames,video_wire_bytes:state.video_wire_bytes,
        samples:state.samples,paints:state.paints,arrivals:state.arrivals,pings:state.pings,overflow:state.overflow,
        skip_samples:state.skip_samples,invalid_skip_stats:state.invalid_skip_stats,invalid_video_headers:state.invalid_video_headers,
        sequence_gaps:state.arrivals.slice(1).filter((row,i) => row.sequence !== ((state.arrivals[i].sequence+1)>>>0)).length,
        audio_before:state.audio_before,audio_after:client.audio.snapshot(),video_before:state.video_before,
        video_after:videoSnapshot(),video_after_collected_ms:performance.now()-start,video_errors_delta:client.metrics.errors-state.video_before.errors,
        stamp_before:state.stamp_before,stamp_after:window.readMotionStamp(client.ctx),
        unique_source_frames:codes.size,invalid_stamp_paints:state.paints.length-valid.length,
        video_marker_eligible:state.paints.length === valid.length,
        observer_failure:window.glissMarkerProbe.failure,max_analysis_gap_ms:window.glissMarkerProbe.max_poll_gap_ms,
        quality_windows_dropped:window.glissMarkerProbe.quality_windows_dropped};
    };
    window.motionStudy = state;
    return {start_ms:start,end_ms:end};
  },plan);
}
