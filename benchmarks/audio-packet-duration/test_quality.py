import importlib.util
import math
import unittest
from pathlib import Path
spec=importlib.util.spec_from_file_location('duration_quality',Path(__file__).with_name('quality.py'));q=importlib.util.module_from_spec(spec);spec.loader.exec_module(q)
class QualityTests(unittest.TestCase):
 def test_metric_identity_and_known_half_amplitude_error(self):
  self.assertTrue(q.metrics([100,200],[100/32768,200/32768])['exact']);r=q.metrics([100,200],[50/32768,100/32768]);self.assertEqual(r['mae_s16_units'],75);self.assertAlmostEqual(r['snr_db'],10*math.log10(4))
 def test_rust_round_is_away_from_zero_at_half(self):self.assertEqual([q.rust_round(x) for x in [.5,-.5,1.5,-1.5]],[1,-1,2,-2])
 def test_fixture_headers_are_validated_before_decode(self):
  packet=list(b'GLA1\x01\x02\0\0\x80\xbb\0\0'+(0).to_bytes(4,'little')+(1920).to_bytes(4,'little')+b'\x01');self.assertEqual(q.check_wire(packet,1920,0),b'\x01')
  for at in [0,4,5,6,8,12,16]:
   bad=list(packet);bad[at]^=1
   with self.assertRaises(ValueError):q.check_wire(bad,1920,0)
if __name__=='__main__':unittest.main()
