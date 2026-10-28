import copy
import importlib.util
import json
import unittest
from pathlib import Path
HERE=Path(__file__).resolve().parent
spec=importlib.util.spec_from_file_location('duration_evaluator',HERE/'evaluate.py');m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
class GateTests(unittest.TestCase):
 def setUp(self):
  self.plan=json.loads((HERE/'plan.json').read_bytes());self.criteria=json.loads((HERE/'criteria.json').read_bytes());self.data={'cleanup_complete':True,'positions':[]}
  for profile in self.plan['profiles']:
   for mode in self.plan['order']:
    self.data['positions'].append({'profile':profile['name'],'mode':mode,'status':'measured','measurement_valid':True,'timely_responses':6,'cpu':{'native':{'percent_of_one_core':10},'browser':{'percent_of_one_core':10 if mode=='ms20' else 8}},'metrics':{'pings':[{'sent_ms':i*900,'wire_received_ms':i*900+100} for i in range(20)],'unique_source_frames':200}})
 def test_complete_success_is_only_a_further_testing_decision(self):
  r=m.evaluate(self.data,self.plan,self.criteria);self.assertTrue(r['further_testing_candidate']);self.assertFalse(r['default_promotion_proved']);self.assertFalse(r['fidelity_acceptance_proved'])
 def test_misses_control_latency_cadence_overlap_and_small_gain_deny(self):
  changes=[lambda p:p.update(timely_responses=5),lambda p:p['metrics']['pings'].pop(),lambda p:[v.update(wire_received_ms=v['sent_ms']+1000) for v in p['metrics']['pings']],lambda p:p['metrics'].update(unique_source_frames=159),lambda p:p['cpu']['browser'].update(percent_of_one_core=10),lambda p:p['cpu']['browser'].update(percent_of_one_core=9.1)]
  for change in changes:
   with self.subTest(change=changes.index(change)):
    d=copy.deepcopy(self.data)
    for p in d['positions']:
     if p['mode']=='ms40':change(p)
    self.assertFalse(m.evaluate(d,self.plan,self.criteria)['further_testing_candidate'])
 def test_invalid_missing_or_blocked_position_suppresses_all_comparison_aggregates(self):
  for patch in [{'measurement_valid':False},{'status':'failed'},{'status':'blocked'}]:
   d=copy.deepcopy(self.data);d['positions'][0].update(patch);r=m.evaluate(d,self.plan,self.criteria);self.assertFalse(r['all_eight_eligible']);self.assertTrue(all(p['cpu'] is None for p in r['profiles']))
if __name__=='__main__':unittest.main()
