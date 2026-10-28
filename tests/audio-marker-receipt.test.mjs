import test from 'node:test';
import assert from 'node:assert/strict';
import {finishMarker} from '../test-support/qemu-audio-marker/receipt.mjs';

const pulse = {token: 1, code: 1, submitted_host_ms: 1000};
const note = {token: 1, code: 1};
const clock = {offset_min_ms: 499, offset_max_ms: 501};
const window = {start_ms: 400, end_ms: 6000};
const observation = {code: 1, analysis_begin_browser_ms: 600, analysis_end_browser_ms: 602, host_received_ms: 1110};

test('marker receipt credits only the exact source token/code and retains the clock interval', () => {
  const result = finishMarker(pulse, [observation], note, clock, window, 700);
  assert.equal(result.status, 'observed');assert.equal(result.trigger_to_observed_ms, 110);
  assert.deepEqual(result.analysis_host_interval_ms, [1101, 1103]);
  assert.equal(finishMarker(pulse, [observation], {...note, token: 2}, clock, window, 700).status, 'source-unconfirmed');
  assert.equal(finishMarker(pulse, [{...observation, code: 2}], note, clock, window, 700).status, 'unobserved');
});

test('marker receipt rejects preexisting analysis even if its IPC callback arrives after submission', () => {
  assert.equal(finishMarker(pulse, [{...observation, analysis_begin_browser_ms: 499, analysis_end_browser_ms: 500}],
    note, clock, window, 700).status, 'preexisting-or-ambiguous');
  assert.equal(finishMarker(pulse, [{...observation, analysis_begin_browser_ms: 500, analysis_end_browser_ms: 501}],
    note, clock, window, 700).status, 'preexisting-or-ambiguous');
});

test('marker receipt treats the exact deadline as late and preserves observations after that deadline', () => {
  const late = finishMarker(pulse, [{...observation, host_received_ms: 1700}], note, clock, window, 700);
  assert.equal(late.status, 'late');assert.equal(late.trigger_to_observed_ms, 700);
  assert.equal(finishMarker(pulse, [{...observation, host_received_ms: 1699.9}], note, clock, window, 700).status, 'observed');
});

test('marker receipt does not credit observations beyond the absolute browser cutoff or fabricate missing probes', () => {
  assert.equal(finishMarker(pulse, [{...observation, analysis_begin_browser_ms: 6000, analysis_end_browser_ms: 6002}],
    note, clock, window, 700).status, 'unobserved');
  assert.equal(finishMarker({...pulse, submitted_host_ms: null}, [], null, clock, window, 700).status, 'not-submitted');
  assert.equal(finishMarker(pulse, [{...observation, analysis_end_browser_ms: NaN}], note, clock, window, 700).status,
    'invalid-observation');
});
