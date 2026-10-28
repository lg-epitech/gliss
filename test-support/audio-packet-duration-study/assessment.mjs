// Fixed20s eligibility; a CPU reduction cannot be earned by delivering less sound.
export function assessAudioWindow(m,packetMs) {
  const reasons=[],lossFields=['stale_raw_events','stale_raw_bytes','stale_pending_discards','stale_pending_bytes','reset_pending_discards','reset_pending_bytes','raw_lag_events','raw_lag_messages','encoder_init_errors','encoder_errors','writer_stale_packets','writer_stale_bytes','writer_lag_events','writer_lag_messages'];
  const samples=m.native_audio_samples,first=samples[0],last=samples.at(-1);
  const counter_deltas=first&&last?Object.fromEntries(Object.keys(first.counters).map(k=>[k,last.counters[k]-first.counters[k]])):null;
  const native_sample_span_ms=first&&last?last.at_ms-first.at_ms:null;
  if(![20,40].includes(packetMs)||m.invalid_audio_headers!==0||m.audio_sequence_gaps!==0)reasons.push('headers-or-sequence');
  if(m.wire_frames!==m.wire_packets*packetMs*48||m.wire_frames<940800||m.wire_frames>979200)reasons.push('delivered-duration');
  if(m.audio_after.dropped!==m.audio_before.dropped)reasons.push('browser-drops');
  if(Math.abs(m.audio_after.scheduled-m.audio_before.scheduled-m.wire_packets)>4)reasons.push('schedule-count');
  if(m.invalid_audio_stats!==0||samples.length<18||samples.length>21||native_sample_span_ms<17000||native_sample_span_ms>=20000)reasons.push('native-coverage');
  if(!counter_deltas||lossFields.some(k=>counter_deltas[k]!==0))reasons.push('native-discards-or-errors');
  return {valid:reasons.length===0,reasons,native_sample_span_ms,counter_deltas,
    scope:'Counter deltas cover only first/last received diagnostics; per-field relaxed values, not a coherent transaction or exhaustive20s loss proof.'};
}
