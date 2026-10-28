import test from 'node:test';
import assert from 'node:assert/strict';
import {validateSocketPolicy} from '../test-support/browser/audio-policy-server.mjs';

const snapshot = requested => ({requested_nodelay: requested,
  scope: 'All accepted sockets; actual option verified before HTTP IO',
  accepted_sockets: 3, verified_sockets: 3, rejected_sockets: 0, initial_true: 0, initial_false: 3,
  initial_read_errors: 0, set_errors: 0, readback_errors: 0, readback_mismatches: 0,
  verified_readback_true: requested ? 3 : 0, verified_readback_false: requested ? 0 : 3});

test('audio transport study requires actual accepted-socket readback for either policy', () => {
  for (const requested of [false, true]) {
    const value = snapshot(requested);
    assert.deepEqual(validateSocketPolicy(value, requested), value);
    for (const edit of [{requested_nodelay: !requested}, {verified_sockets: 2}, {accepted_sockets: 0},
      {readback_mismatches: 1}, {initial_false: 2}, {verified_readback_true: '3'},
      {verified_readback_true: requested ? 2 : 1}, {verified_readback_false: requested ? 1 : 2}])
      assert.throws(() => validateSocketPolicy({...value, ...edit}, requested));
  }
});
