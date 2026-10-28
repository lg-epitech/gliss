"""Recompute the registered audio-duration comparison from retained raw evidence."""
import gzip
import hashlib
import importlib.util
import json
import io
import tarfile
import math
from pathlib import Path

HERE=Path(__file__).resolve().parent
ROOT=HERE.parents[1]
LOSS=['stale_raw_events','stale_raw_bytes','stale_pending_discards','stale_pending_bytes','reset_pending_discards','reset_pending_bytes','raw_lag_events','raw_lag_messages','encoder_init_errors','encoder_errors','writer_stale_packets','writer_stale_bytes','writer_lag_events','writer_lag_messages']
KEYS=LOSS+['emitted_packets','emitted_frames','emitted_wire_bytes']
def require(ok,message):
    if not ok:raise ValueError(message)
def digest(raw):return hashlib.sha256(raw).hexdigest()
def close(a,b):return math.isclose(a,b,rel_tol=1e-9,abs_tol=1e-6)
def load(name,path):
    spec=importlib.util.spec_from_file_location(name,path);m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m);return m

def check_cpu(p):
    before=p['cpu_boundaries']['before'];after=p['cpu_boundaries']['after']
    for name in ['native','guest','browser']:
        b,a=before[name],after[name];cpu=p['cpu'][name]
        elapsed=(a['begin_ms']+a['end_ms']-b['begin_ms']-b['end_ms'])/2
        require(b['end_ms']>=b['begin_ms'] and a['end_ms']>=a['begin_ms'],'inverted counter acquisition')
        require(close(elapsed,cpu['elapsed_ms']),'CPU boundary denominator changed')
        if name=='browser':
            first={(r['pid'],r['start']):r['cpu'] for r in b['rows']};last={(r['pid'],r['start']):r['cpu'] for r in a['rows']}
            common=first.keys()&last.keys();seconds=sum(last[k]-first[k] for k in common)
            require(all(last[k]>=first[k] for k in common),'CPU counter regressed')
            require(close(seconds,cpu['cpu_seconds']),'browser CPU numerator changed')
            require((cpu['matched_processes'],cpu['before_processes'],cpu['after_processes'])==(len(common),len(first),len(last)),'browser endpoint inventory changed')
            require(cpu['complete_endpoint_set']==(len(common)==len(first)==len(last)),'endpoint completeness changed')
        else:
            require(b['start']==a['start'] and b['allowed_cpus']==a['allowed_cpus']==('0' if name=='native' else '19'),'process identity or affinity changed')
            require(a['ticks']>=b['ticks'] and cpu['ticks']==a['ticks']-b['ticks'],'process CPU numerator changed')
            seconds=cpu['ticks']/cpu['ticks_per_second']
        require(close(seconds*100000/elapsed,cpu['percent_of_one_core']),'CPU percent changed')

def check_position(p,plan):
    m=p['metrics'];duration=p['audio_packet_ms'];arrivals=m['audio_arrivals'];samples=m['native_audio_samples']
    require(duration==plan['audio_packet_ms'][p['mode']],'duration changed')
    require(len(arrivals)<=1024 and all(0<=a['at_ms']<20000 and a['frames']==duration*48 and 21<=a['bytes']<=1295 and type(a['sequence']) is int and 0<=a['sequence']<=0xffffffff for a in arrivals),'audio arrival bounds changed')
    require(m['wire_packets']==len(arrivals) and m['wire_frames']==sum(a['frames'] for a in arrivals) and m['wire_bytes']==sum(a['bytes'] for a in arrivals),'audio totals changed')
    gaps=sum(b['sequence']!=((a['sequence']+1)&0xffffffff) for a,b in zip(arrivals,arrivals[1:]))
    require(gaps==m['audio_sequence_gaps'],'audio gaps changed')
    require(len(samples)<=32 and all(0<=s['at_ms']<20000 and s['packet_ms']==duration and set(s['counters'])==set(KEYS) and all(type(v) is int and 0<=v<=2**53-1 for v in s['counters'].values()) for s in samples),'native telemetry bounds changed')
    require(all(b['at_ms']>=a['at_ms'] and all(b['counters'][k]>=a['counters'][k] for k in KEYS) for a,b in zip(samples,samples[1:])),'native counter regression')
    deltas={k:samples[-1]['counters'][k]-samples[0]['counters'][k] for k in KEYS} if samples else None
    span=samples[-1]['at_ms']-samples[0]['at_ms'] if samples else None
    reasons=[]
    if duration not in [20,40] or m['invalid_audio_headers']!=0 or gaps:reasons.append('headers-or-sequence')
    if m['wire_frames']!=m['wire_packets']*duration*48 or not 940800<=m['wire_frames']<=979200:reasons.append('delivered-duration')
    if m['audio_after']['dropped']!=m['audio_before']['dropped']:reasons.append('browser-drops')
    if abs(m['audio_after']['scheduled']-m['audio_before']['scheduled']-m['wire_packets'])>4:reasons.append('schedule-count')
    if m['invalid_audio_stats']!=0 or not 18<=len(samples)<=21 or span is None or not 17000<=span<20000:reasons.append('native-coverage')
    if deltas is None or any(deltas[k]!=0 for k in LOSS):reasons.append('native-discards-or-errors')
    assessment=p['audio_assessment']
    require(assessment['valid']==(not reasons) and assessment['reasons']==reasons and assessment['counter_deltas']==deltas and assessment['native_sample_span_ms']==span,'audio assessment changed')
    check_cpu(p)
    argv=p['native_argv']
    for flag,value in [('--audio-packet-ms',str(duration)),('--h264-bitrate','500000'),('--in-flight-frames','8'),('--video-frame-skip','false'),('--video-qp-min','42'),('--video-qp-max','51')]:
        require(argv.count(flag)==1 and argv[argv.index(flag)+1]==value,'native CLI changed')
    require(argv.count('--stream-stats')==1 and p['frame_skip'] is False and p['qp_bounds']=={'min':42,'max':51,'scope':'requested native bounds; not coded-QP attestation'},'video policy changed')
    expected=(not reasons and not p['observation_overflow'] and not m['overflow'] and not p['page_errors'] and m['observer_failure'] is None
      and m['video_errors_delta']==0 and m['video_marker_eligible'] and m['sequence_gaps']==m['invalid_skip_stats']==m['invalid_video_headers']==0
      and p['cpu']['browser']['complete_endpoint_set'] and all(20000<=c['elapsed_ms']<=22000 for c in p['cpu'].values())
      and all(q['source_valid'] and q['submission_lateness_ms']<=100 and q['status'] not in ['invalid-observation','preexisting-or-ambiguous'] for q in p['pulses']))
    require(p['measurement_valid']==expected,'measurement validity changed')

def main():
    inventory=json.loads((HERE/'results.json').read_bytes())
    require(inventory['schema']=='gliss-audio-packet-duration-inventory-v1','inventory schema changed')
    record=inventory['cohort'];registration=json.loads((HERE/'registration.json').read_bytes());plan=json.loads((HERE/'plan.json').read_bytes())
    require(registration['positions_executed']==0 and registration['status']=='prospective','registration no longer prospective')
    for name in ['criteria','decision','validation']:
        require(digest((HERE/(name+'.json')).read_bytes())==inventory[name+'_sha256'],name+' changed')
    require(registration['criteria_sha256']==inventory['criteria_sha256'],'prospective criteria changed')
    for name,change in inventory.get('post_execution_verifier_repairs',{}).items():
        require(registration['sources'][name]==change['registered_sha256'] and digest((ROOT/name).read_bytes())==change['final_sha256'],'verifier repair binding changed')
    require(digest((HERE/'evaluate.py').read_bytes())==registration['sources']['benchmarks/audio-packet-duration/evaluate.py'],'prospective selector changed')
    for key in ['results','plan','registration','source_archive']:
        require(digest((HERE/record[key]).read_bytes())==record[key+'_sha256'],key+' binding changed')
    require(registration['plan_sha256']==digest((HERE/'plan.json').read_bytes()),'prospective plan changed')
    archived={}
    with tarfile.open(fileobj=io.BytesIO(gzip.decompress((HERE/record['source_archive']).read_bytes()))) as tar:
        for member in tar:
            require(member.isfile() and not member.name.startswith('/') and '..' not in Path(member.name).parts and member.name not in archived,'unsafe source member')
            archived[member.name]=tar.extractfile(member).read()
    require(set(archived)==set(registration['sources']),'source closure changed')
    require(all(digest(archived[n])==h for n,h in registration['sources'].items()),'archived source bytes changed')
    shared=load('audio_duration_shared',HERE/'verify_shared.py');shared.HERE=HERE
    raw=gzip.decompress((HERE/record['results']).read_bytes());data=json.loads(raw)
    if data['cleanup_complete'] and all(p['status']=='measured' for p in data['positions']):
        require(len(data['guest_builds'])==2 and [r['profile'] for r in data['guest_builds']]==['loopback','kernel-limited'],'guest inventory changed')
        data=shared.verify(record,allow_ineligible=True)
    else:
        # No aggregate performance claim on failed or partial evidence.
        require(digest((HERE/record['results']).read_bytes())==record['results_sha256'] and digest(raw)==record['original_sha256'] and len(raw)==record['original_bytes'],'failed evidence changed')
        require(data['plan_sha256']==registration['plan_sha256']==digest((HERE/'plan.json').read_bytes()),'failed plan binding changed')
        require(data['binary_sha256']==registration['binary_sha256']==record['binary_sha256'],'failed executable binding changed')
    require(len(data['positions'])==8,'declared slots missing')
    for p in data['positions']:
        if p['status']=='measured':check_position(p,plan)
    criteria=json.loads((HERE/'criteria.json').read_bytes())
    decision=load('audio_duration_evaluate',HERE/'evaluate.py').evaluate(data,plan,criteria)
    require(decision==json.loads((HERE/'decision.json').read_bytes()),'selection decision changed')
    require(decision['default_packet_ms']==20 and not decision['default_promotion_proved'],'scope changed')
    print('All eight slots retained. Further-testing candidate:',decision['further_testing_candidate'],'; default remains20ms.')
if __name__=='__main__':main()
