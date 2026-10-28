// Timing attribution used by the actual marker study, including its failures.
export function finishMarker(pulse, observations, sourceNote, clock, browserWindow, deadlineMs) {
  const sourceValid = sourceNote?.token === pulse.token && sourceNote?.code === pulse.code;
  const result = {...pulse, source_note: sourceNote ?? null, source_valid: sourceValid,
    observation: null, trigger_to_observed_ms: null, analysis_host_interval_ms: null};
  if (pulse.submitted_host_ms === null) return {...result, status: 'not-submitted'};
  const candidates = observations.filter(row => row.code === pulse.code && row.analysis_begin_browser_ms < browserWindow.end_ms)
    .toSorted((a, b) => a.host_received_ms - b.host_received_ms);
  if (!candidates.length) return {...result, status: sourceValid ? 'unobserved' : 'source-unconfirmed'};
  const observation = candidates[0];
  const values = [observation.analysis_begin_browser_ms, observation.analysis_end_browser_ms,
    observation.host_received_ms, clock.offset_min_ms, clock.offset_max_ms];
  if (!values.every(Number.isFinite) || observation.analysis_end_browser_ms < observation.analysis_begin_browser_ms)
    return {...result, status: 'invalid-observation'};
  result.observation = observation;
  result.trigger_to_observed_ms = observation.host_received_ms - pulse.submitted_host_ms;
  result.analysis_host_interval_ms = [observation.analysis_end_browser_ms + clock.offset_min_ms,
    observation.analysis_end_browser_ms + clock.offset_max_ms];
  if (observation.analysis_begin_browser_ms < browserWindow.start_ms ||
      observation.analysis_begin_browser_ms + clock.offset_min_ms < pulse.submitted_host_ms ||
      observation.host_received_ms < pulse.submitted_host_ms)
    return {...result, status: 'preexisting-or-ambiguous'};
  if (!sourceValid) return {...result, status: 'source-unconfirmed'};
  return {...result, status: observation.host_received_ms < pulse.submitted_host_ms + deadlineMs ? 'observed' : 'late'};
}
