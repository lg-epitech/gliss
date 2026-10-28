"""Post hoc, read-only derivation of the audit disclosures; no project imports."""
import gzip
import hashlib
import json
from collections import Counter
from pathlib import Path
HERE=Path(__file__).resolve().parent
ROOT=HERE.parents[1]
def digest(raw):return hashlib.sha256(raw).hexdigest()
def derive():
    packed=(HERE/'cohort.json.gz').read_bytes();data=json.loads(gzip.decompress(packed))
    registration=json.loads((HERE/'registration.json').read_bytes())
    aggregate=digest(''.join(name+'\0'+registration['sources'][name]+'\n' for name in sorted(registration['sources'])).encode())
    if aggregate!=registration['source_set_sha256']:raise ValueError('source-set recipe differs')
    sizes={};gaps=[]
    for mode in ['ms20','ms40']:
        rows=[a for p in data['positions'] if p['mode']==mode for a in p['metrics']['audio_arrivals']]
        sizes[mode]={'packets':len(rows),'min_bytes':min(a['bytes'] for a in rows),'max_bytes':max(a['bytes'] for a in rows),'size_counts':dict(sorted(Counter(a['bytes'] for a in rows).items()))}
    for position in data['positions']:
        m=position['metrics'];keyframes=[a for a in m['arrivals'] if a['keyframe']]
        for before,after in zip(m['audio_arrivals'],m['audio_arrivals'][1:]):
            gap=after['at_ms']-before['at_ms']
            if gap<=100:continue
            prior=[a for a in keyframes if a['at_ms']<=after['at_ms']]
            nearest=max(prior,key=lambda a:a['at_ms']) if prior else None
            gaps.append({'slot':position['slot'],'profile':position['profile'],'mode':position['mode'],'gap_start_ms':before['at_ms'],'gap_end_ms':after['at_ms'],'gap_ms':gap,
                'preceding_keyframe_at_ms':nearest['at_ms'] if nearest else None,'preceding_keyframe_bytes':nearest['bytes'] if nearest else None,'gap_end_minus_keyframe_ms':after['at_ms']-nearest['at_ms'] if nearest else None})
    original=ROOT/'benchmarks/audio-duration-prerequisite/native.json.gz';native=json.loads(gzip.decompress(original.read_bytes()))
    prerequisite40=[c for a in native['arms'] if a['packet_frames']==1920 for c in a['chunks']]
    return {'schema':'gliss-audio-duration-posthoc-audit-evidence-v1','cohort_compressed_sha256':digest(packed),
        'source_set_recipe':'UTF8 of sorted path + NUL + lowercase file SHA256 + newline, then SHA256','source_set_sha256':aggregate,
        'live_packet_sizes':sizes,'prerequisite40_packet_count':len(prerequisite40),'prerequisite40_packet_sizes':sorted({len(c) for c in prerequisite40}),
        'audio_arrival_gaps_over_100ms':gaps,'drop_branch_attribution_proved':False,
        'qualification':'Post hoc correlation only. The aggregate browser counter does not retain a branch witness, and these traces do not guarantee that every future trial will fail. Packet size constancy does not prove that the encoder VBR setting was disabled.'}
if __name__=='__main__':print(json.dumps(derive(),indent=2))
