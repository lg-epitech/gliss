import copy
import gzip
import importlib.util
import json
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('fill_evidence', HERE / 'verify.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class EvidenceTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.data = json.loads(gzip.decompress((HERE / 'cohort.json.gz').read_bytes()))
        cls.plan = json.loads((HERE / 'plan.json').read_bytes())
        cls.reg = json.loads((HERE / 'registration.json').read_bytes())

    def test_original_passes_both_retention_gates_remain_failed(self):
        rows = module.verify_data(self.data, self.plan, self.reg)
        self.assertTrue(all(not row['mean_reduction_gate_passed'] and not row['range_gate_passed'] for row in rows))

    def test_incomplete_or_late_packets_and_counters_are_rejected(self):
        changes = [lambda p: p['metrics']['sends'].pop(),
                   lambda p: p['metrics']['sends'][-1].update(at_ms=6000, lateness_ms=20),
                   lambda p: p['metrics']['schedule_timings'][-1].update(at_ms=6000),
                   lambda p: p['metrics']['after'].update(pendingDecodes=1),
                   lambda p: p['cpu'].update(percent_of_one_core=0),
                   lambda p: p['cpu'].update(complete_endpoint_set=False),
                   lambda p: p['resource_after']['pids.events'].update(max=1)]
        for change in changes:
            with self.subTest(change=change):
                altered = copy.deepcopy(self.data);change(altered['positions'][0])
                with self.assertRaises(ValueError):module.verify_data(altered, self.plan, self.reg)

    def test_counter_span_pool_tail_and_stale_setup_label_are_explicit(self):
        for field in ['counter', 'pool', 'phase']:
            changed = copy.deepcopy(self.data)
            if field == 'counter':
                cpu = changed['positions'][0]['cpu'];cpu['elapsed_ms'] = 8000
                cpu['percent_of_one_core'] = cpu['cpu_seconds'] * 100000 / cpu['elapsed_ms']
            elif field == 'pool': changed['pools']['opus']['packets'] = 64
            else: changed['phase'] = 'still-running'
            with self.subTest(field=field), self.assertRaises(ValueError):
                module.verify_data(changed, self.plan, self.reg)


if __name__ == '__main__':
    unittest.main()
