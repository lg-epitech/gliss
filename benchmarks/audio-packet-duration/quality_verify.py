"""Stdlib replay of retained aligned waveforms; no media-library execution."""
import gzip
import hashlib
import json
import math
import struct
from pathlib import Path
HERE=Path(__file__).resolve().parent
ROOT=HERE.parents[1]
def digest(raw):return hashlib.sha256(raw).hexdigest()
def need(ok,message):
    if not ok:raise ValueError(message)
def main():
    inventory=json.loads((HERE/'results.json').read_bytes())['retrospective_quality']
    for name,expected in inventory['files'].items():need(digest((HERE/name).read_bytes())==expected,'quality file changed: '+name)
    result=json.loads((HERE/'quality.json').read_bytes());plan=json.loads((HERE/'quality-plan.json').read_bytes())
    need(result['script_sha256']==plan['script_sha256']==digest((HERE/'quality.py').read_bytes()),'analysis source changed')
    need(result['plan_sha256']==digest((HERE/'quality-plan.json').read_bytes()),'analysis declaration changed')
    packed=(ROOT/plan['native_file']).read_bytes();need(digest(packed)==plan['native_compressed_sha256'],'original packet fixture changed')
    native_raw=gzip.decompress(packed);native=json.loads(native_raw)
    need(digest(native_raw)==result['native_original_sha256'],'original packet bytes changed')
    pcm=gzip.decompress((HERE/'input.s16le.gz').read_bytes());need(digest(pcm)==native['source_pcm_sha256']==result['source_pcm_sha256'] and len(pcm)==61440,'exact input changed')
    need(result['decoder_library_sha256']==plan['decoder_library_sha256'] and result['quality_acceptability_proved'] is False,'quality scope changed')
    reference=struct.unpack('<30720h',pcm);need(len(result['arms'])==4,'quality arms missing')
    for index,arm in enumerate(result['arms']):
        need(arm['arm']==index and arm['status']=='decoded' and arm['packet_frames']==[960,1920,1920,960][index] and arm['lookahead_frames']==312 and arm['frames']==15360 and len(arm['channels'])==2 and arm['decoded_file']==f'arm-{index}.f32le.gz','quality arm changed')
        raw=gzip.decompress((HERE/arm['decoded_file']).read_bytes());need(len(raw)==122880 and digest(raw)==arm['decoded_sha256'],'aligned decoded samples changed')
        decoded=struct.unpack('<30720f',raw)
        for channel,stored in enumerate(arm['channels']):
            source=reference[channel::2];output=decoded[channel::2];errors=[o*32768-s for s,o in zip(source,output)];power=math.fsum(float(x)*x for x in source);squared=math.fsum(e*e for e in errors)
            recomputed={'samples':15360,'mae_s16_units':math.fsum(abs(e) for e in errors)/15360,'rmse_s16_units':math.sqrt(squared/15360),'snr_db':10*math.log10(power/squared),'exact':False}
            need(stored.keys()==recomputed.keys() and all(stored[k]==v if type(v) in [bool,int] else math.isclose(stored[k],v,rel_tol=1e-10,abs_tol=1e-9) for k,v in recomputed.items()),'quality arithmetic changed')
    need(result['arms'][0]['decoded_sha256']==result['arms'][3]['decoded_sha256'] and result['arms'][1]['decoded_sha256']==result['arms'][2]['decoded_sha256'],'same-duration repetitions differ')
    print('Four aligned320ms tone arms reproduce. No subjective quality or physical-output claim.')
if __name__=='__main__':main()
