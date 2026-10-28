"""Confirmation failures cannot be hidden by aggregating another position."""
import copy
import importlib.util
import json
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('qp_confirmation', HERE / 'evaluate.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class ConfirmationTests(unittest.TestCase):
    def setUp(self):
        self.criteria = json.loads((HERE / 'criteria.json').read_bytes())
        self.data = {'cleanup_complete': True, 'positions': []}
        for profile in ['loopback', 'kernel-limited']:
            for mode in ['floor26', 'floor42', 'floor42', 'floor26']:
                self.data['positions'].append({'slot': len(self.data['positions']) + 1,
                    'profile': profile, 'mode': mode, 'status': 'measured', 'measurement_valid': True,
                    'native_exit': {'code': 0, 'signal': None}, 'timely_responses': 6,
                    'pulses': [{'status': 'observed', 'trigger_to_observed_ms': 300} for _ in range(6)],
                    'metrics': {'pings': [{'sent_ms': i * 300, 'wire_received_ms': i * 300 + 100}
                                          for i in range(20)],
                        'paints': [{'source_frame': i} for i in range(48)], 'unique_source_frames': 48,
                        'arrivals': [{'keyframe': True}], 'invalid_video_headers': 0,
                        'video_errors_delta': 0, 'sequence_gaps': 0, 'overflow': False,
                        'observer_failure': None, 'invalid_stamp_paints': 0}})

    def test_fixed_boundary_accepts_and_preserves_quality_limit(self):
        value = module.evaluate(self.data, self.criteria)
        self.assertTrue(value['accepted'])
        self.assertFalse(value['quality_acceptance_proved'])
        self.assertEqual([r['slot'] for r in value['positions']], [2, 3, 6, 7])

    def test_each_position_must_pass_every_gate(self):
        mutations = [lambda p: p['pulses'][0].update(status='late', trigger_to_observed_ms=700),
            lambda p: p['metrics']['pings'][0].update(wire_received_ms=None),
            lambda p: [ping.update(wire_received_ms=ping['sent_ms'] + 151) for ping in p['metrics']['pings']],
            lambda p: p['metrics'].update(paints=p['metrics']['paints'][:-1], unique_source_frames=47),
            lambda p: p['metrics'].update(arrivals=[]),
            lambda p: p['metrics'].update(sequence_gaps=1),
            lambda p: p.update(measurement_valid=False)]
        for mutate in mutations:
            with self.subTest(mutation=mutate):
                changed = copy.deepcopy(self.data)
                mutate(changed['positions'][5])
                self.assertFalse(module.evaluate(changed, self.criteria)['accepted'])

    def test_failed_control_measurement_and_cleanup_deny_acceptance(self):
        for field in ['measurement', 'cleanup']:
            changed = copy.deepcopy(self.data)
            if field == 'measurement': changed['positions'][0]['measurement_valid'] = False
            else: changed['cleanup_complete'] = False
            self.assertFalse(module.evaluate(changed, self.criteria)['accepted'])

    def test_reordered_or_missing_slots_are_not_comparisons(self):
        for positions in [self.data['positions'][:-1], list(reversed(self.data['positions']))]:
            with self.assertRaises(ValueError):
                module.evaluate({**self.data, 'positions': positions}, self.criteria)


if __name__ == '__main__':
    unittest.main()
