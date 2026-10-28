import copy
import importlib.util
import json
import unittest
from pathlib import Path
HERE=Path(__file__).resolve().parent
spec=importlib.util.spec_from_file_location('duration_verifier',HERE/'verify.py');v=importlib.util.module_from_spec(spec);spec.loader.exec_module(v)

def position():
 zero={k:0 for k in v.KEYS};samples=[{'at_ms':500+1000*i,'packet_ms':40,'counters':dict(zero)} for i in range(20)]
 m={'native_audio_samples':samples,'audio_arrivals':[{'at_ms':i*40,'sequence':i,'frames':1920,'bytes':300} for i in range(500)],'wire_packets':500,'wire_frames':960000,'wire_bytes':150000,'audio_sequence_gaps':0,'invalid_audio_headers':0,'audio_before':{'dropped':0,'scheduled':10},'audio_after':{'dropped':0,'scheduled':510},'invalid_audio_stats':0,'overflow':False,'observer_failure':None,'video_errors_delta':0,'video_marker_eligible':True,'sequence_gaps':0,'invalid_skip_stats':0,'invalid_video_headers':0}
 before={};after={};cpu={}
 for name,affinity,ticks in [('native','0',100),('guest','19',20)]:
  before[name]={'begin_ms':0,'end_ms':0,'start':'20','allowed_cpus':affinity,'ticks':0};after[name]={**before[name],'begin_ms':20000,'end_ms':20000,'ticks':ticks};cpu[name]={'elapsed_ms':20000,'ticks':ticks,'ticks_per_second':100,'percent_of_one_core':ticks/20}
 before['browser']={'begin_ms':0,'end_ms':0,'rows':[{'pid':1,'start':'20','cpu':0}]};after['browser']={'begin_ms':20000,'end_ms':20000,'rows':[{'pid':1,'start':'20','cpu':2}]};cpu['browser']={'elapsed_ms':20000,'cpu_seconds':2,'percent_of_one_core':10,'matched_processes':1,'before_processes':1,'after_processes':1,'complete_endpoint_set':True}
 return {'mode':'ms40','audio_packet_ms':40,'metrics':m,'cpu_boundaries':{'before':before,'after':after},'cpu':cpu,'audio_assessment':{'valid':True,'reasons':[],'counter_deltas':zero,'native_sample_span_ms':19000},'native_argv':['--audio-packet-ms','40','--h264-bitrate','500000','--in-flight-frames','8','--video-frame-skip','false','--video-qp-min','42','--video-qp-max','51','--stream-stats'],'frame_skip':False,'qp_bounds':{'min':42,'max':51,'scope':'requested native bounds; not coded-QP attestation'},'observation_overflow':False,'page_errors':[],'pulses':[{'source_valid':True,'submission_lateness_ms':0,'status':'observed'} for _ in range(6)],'measurement_valid':True}
class VerifierTests(unittest.TestCase):
 def test_positive_raw_recomputation(self):v.check_position(position(),{'audio_packet_ms':{'ms40':40}})
 def test_packet_frames_totals_sequence_and_counters_cannot_be_relabelled(self):
  for mutate in [lambda p:p['metrics']['audio_arrivals'][0].update(frames=960),lambda p:p['metrics'].update(wire_bytes=1),lambda p:p['metrics']['audio_arrivals'][2].update(sequence=99),lambda p:p['metrics']['native_audio_samples'][-1]['counters'].update(writer_stale_packets=1),lambda p:p['audio_assessment'].update(native_sample_span_ms=20000),lambda p:p['native_argv'].__setitem__(1,'20')]:
   p=position();mutate(p)
   with self.assertRaises(ValueError):v.check_position(p,{'audio_packet_ms':{'ms40':40}})
 def test_raw_cpu_boundaries_and_process_sets_bind_summary(self):
  for mutate in [lambda p:p['cpu']['browser'].update(cpu_seconds=1),lambda p:p['cpu']['native'].update(elapsed_ms=19000),lambda p:p['cpu_boundaries']['after']['native'].update(start='21'),lambda p:p['cpu_boundaries']['after']['browser']['rows'][0].update(pid=2),lambda p:p['cpu']['guest'].update(percent_of_one_core=0)]:
   p=position();mutate(p)
   with self.assertRaises(ValueError):v.check_position(p,{'audio_packet_ms':{'ms40':40}})
if __name__=='__main__':unittest.main()
