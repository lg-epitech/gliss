"""Verify the retained QP comparison and recompute its scoped summaries."""
import gzip
import hashlib
import importlib.util
import io
import json
import math
import statistics
import tarfile
from pathlib import Path
HERE=Path(__file__).resolve().parent
ROOT=HERE.parent.parent


def require(value,message):
    if not value:raise ValueError(message)


def digest(raw):return hashlib.sha256(raw).hexdigest()


def read_record(record):
    packed=(HERE/record['file']).read_bytes()
    require(digest(packed)==record['compressed_sha256'],'Compressed observations changed')
    raw=gzip.decompress(packed)
    require(len(raw)==record['original_bytes'] and digest(raw)==record['original_sha256'],'Original observations changed')
    return raw


def main():
    inventory=json.loads((HERE/'results.json').read_text())
    require(digest((HERE/'validation.json').read_bytes())==inventory['validation_sha256'],'Validation changed')
    tool=ROOT/inventory['shared_verifier']
    require(digest(tool.read_bytes())==inventory['shared_verifier_sha256'],'Shared verifier changed')
    spec=importlib.util.spec_from_file_location('qp_motion_evidence',tool)
    shared=importlib.util.module_from_spec(spec);spec.loader.exec_module(shared);shared.HERE=HERE
    data=shared.verify(inventory['cohort'])
    for p in data['positions']:
        qp=None if p['mode']=='default' else {'min':26,'max':51,'scope':'requested native bounds; not coded-QP attestation'}
        require(p['qp_bounds']==qp and p['frame_skip'] is False and p['in_flight_frames']==8,'QP policy changed')
        argv=p['native_argv']
        for flag,value in [('--in-flight-frames','8'),('--h264-bitrate','500000'),('--video-frame-skip','false')]:
            require(argv.count(flag)==1 and argv[argv.index(flag)+1]==value,'Recorded native argv changed')
        for flag,value in [('--video-qp-min','26'),('--video-qp-max','51')]:
            require((flag not in argv) if qp is None else (argv.count(flag)==1 and argv[argv.index(flag)+1]==value),'QP CLI mismatch')
        m=p['metrics'];arrivals=m['arrivals']
        require(m['invalid_video_headers']==m['sequence_gaps']==m['invalid_skip_stats']==0 and m['skip_samples']==[],'Video diagnostics invalid')
        require(all(type(a['keyframe']) is bool and a['bytes']>=25 for a in arrivals),'Picture flags missing')
        require(all(b['sequence']==((a['sequence']+1)&0xffffffff) for a,b in zip(arrivals,arrivals[1:])),'Video gap')
        print(f"  IDRs {sum(a['keyframe'] for a in arrivals)}, largest AU packet {max(a['bytes'] for a in arrivals)} bytes")
    plan_raw=(HERE/'component-plan.json').read_bytes()
    require(digest(plan_raw)==inventory['component_plan_sha256'],'Component plan changed')
    plan=json.loads(plan_raw)
    reg_raw=(HERE/'component-registration.json').read_bytes()
    require(digest(reg_raw)==inventory['component_registration_sha256'],'Component registration changed')
    reg=json.loads(reg_raw)
    require(reg['positions_executed']==0 and reg['plan_sha256']==digest(plan_raw) and reg['input_bytes']==49152000,'Input registration invalid')
    archive=(HERE/'component-source.tar.gz').read_bytes()
    require(digest(archive)==inventory['component_source_archive_sha256'],'Component source archive changed')
    with tarfile.open(fileobj=io.BytesIO(gzip.decompress(archive))) as t:
        for name,expected in reg['sources'].items():
            info=t.getmember(name);require(info.isfile(),'Non-file component source')
            require(digest(t.extractfile(info).read())==expected==plan['source_pins'][name],'Component source changed')
    component=json.loads(read_record(inventory['component']))
    journal_raw=read_record(inventory['component_journal'])
    journal=[json.loads(line) for line in journal_raw.splitlines()]
    require(len(journal)==102 and len(journal)<=plan['limits']['journal_records_max'] and
            len(journal_raw)<=plan['limits']['journal_bytes_max'] and journal[-1]=={'event':'complete'},'Partial component journal')
    require([a['wide'] for a in component['arms']]==[False,True,True,False],'Component arms missing')
    for arm_index,arm in enumerate(component['arms']):
        require(len(arm['frames'])==12,'Frame positions missing')
        for i,row in enumerate(arm['frames']):
            require(row['index']==i and row['source_counter']==i*3 and row['bytes']>24 and type(row['keyframe']) is bool,'Frame identity invalid')
            require(any(j=={'event':'frame','arm':arm_index,'data':row} for j in journal),'Frame observation not durable')
            for prefix in ['rgb','document','texture']:
                mae=row[prefix+'_mae'];mse=row[prefix+'_mse']
                require(math.isfinite(mae) and math.isfinite(mse) and 0<=mae<=255 and mae*mae<=mse+1e-6 and mse<=65025,'Invalid error metric')
            for suffix in ['mae','mse']:
                require(row['rgb_'+suffix]*1280*800+1e-5>=row['document_'+suffix]*950*460+row['texture_'+suffix]*360*180,'Inconsistent region error')
        print(f"component {arm_index+1} {'wide' if arm['wide'] else 'default'}: "
              f"{sum(f['bytes'] for f in arm['frames'])} bytes; "
              f"document MAE {statistics.mean(f['document_mae'] for f in arm['frames']):.3f}; "
              f"texture MAE {statistics.mean(f['texture_mae'] for f in arm['frames']):.3f}")
    for a,b in [(0,3),(1,2)]:
        strip=lambda arm:[{k:v for k,v in f.items() if k!='encode_wall_ns'} for f in arm['frames']]
        require(strip(component['arms'][a])==strip(component['arms'][b]),'Repeated component observations drifted')
    print('Verified eight live positions, all 48 sound probes, four shared-input image arms and the durable frame journal. Replay to remeasure image error.')


if __name__=='__main__':main()
