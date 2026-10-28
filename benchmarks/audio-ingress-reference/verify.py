"""Replay modeled assembly ages from the original RFB ingress observations."""
import gzip
import hashlib
import io
import json
import math
import statistics
import tarfile
from pathlib import Path

HERE = Path(__file__).resolve().parent


def require(value, message):
    if not value:
        raise ValueError(message)


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def replay(rows, target):
    pending = 0;oldest = None;discarded = 0;ages = []
    for row in rows:
        size, received, now = row['bytes'], row['ingress_ms'], row['consumed_ms']
        if now - received > 100:
            discarded += pending + size;pending = 0;oldest = None;continue
        if oldest is not None and now - oldest > 100:
            discarded += pending;pending = 0;oldest = None
        while size:
            if pending == 0: oldest = received
            count = min(size, target - pending);pending += count;size -= count
            if pending == target:
                ages.append(now - oldest);pending = 0;oldest = None
    return ages, pending, discarded


def main():
    inventory = json.loads((HERE / 'results.json').read_bytes())
    for name, expected in inventory['files'].items():require(digest((HERE / name).read_bytes()) == expected, name + ' changed')
    records = {}
    for name, record in inventory['records'].items():
        packed = (HERE / record['file']).read_bytes();require(digest(packed) == record['compressed_sha256'], 'Compressed bytes changed')
        raw = gzip.decompress(packed)
        require(len(raw) == record['original_bytes'] and digest(raw) == record['original_sha256'], 'Original bytes changed')
        records[name] = raw
    plan = json.loads((HERE / 'plan.json').read_bytes());reg = json.loads((HERE / 'registration.json').read_bytes())
    require(plan['motion_order'] == [False, True, True, False] and plan['window_ms'] == 6000
            and plan['model_age_limit_ms'] == 100 and plan['packet_model_frames'] == [960, 1920], 'Fixed reference changed')
    require(reg['positions_executed'] == plan['positions_executed'] == 0 and reg['status'] == 'prospective'
            and reg['plan_sha256'] == digest((HERE / 'plan.json').read_bytes()), 'Registration changed')
    with tarfile.open(fileobj=io.BytesIO(gzip.decompress((HERE / 'source.tar.gz').read_bytes()))) as archive:
        sources = {}
        for member in archive:
            require(member.isfile() and not member.name.startswith('/') and '..' not in Path(member.name).parts
                    and member.name not in sources, 'Invalid source archive')
            sources[member.name] = archive.extractfile(member).read()
    require(set(sources) == set(reg['sources']), 'Source closure changed')
    for name, expected in reg['sources'].items():require(digest(sources[name]) == expected, 'Source bytes changed')
    cohort = json.loads(records['cohort.json'])
    require(cohort['plan_sha256'] == reg['plan_sha256'] and cohort['binary_sha256'] == reg['binary_sha256']
            and cohort['cleanup_complete'] and 'failure' not in cohort and len(cohort['positions']) == 4,
            'Cohort binding or cleanup failed')
    for name, expected in cohort['sources'].items():require(reg['sources'][name] == expected, 'Observed source differs')
    total = 0
    for index, p in enumerate(cohort['positions']):
        require(p['slot'] == index + 1 and p['motion'] is plan['motion_order'][index]
                and p['status'] == 'measured' and p['measurement_valid'] is True
                and p['native_exit']['exit'] == {'code': 0, 'signal': None}, 'Reference slot incomplete')
        raw = records[p['file']]
        require(len(raw) == p['report_bytes'] and digest(raw) == p['report_sha256'], 'Native record binding changed')
        data = json.loads(raw);rows = data['events']
        require(data['schema'] == 'gliss-audio-ingress-reference-v1' and data['window_ms'] == 6000
                and 6000 <= data['elapsed_ms'] <= 6100 and data['audio_disabled']
                and data['raw_lagged_messages'] == data['transitions'] == 0
                and 0 < len(rows) <= 2048 and 0 < data['raw_bytes'] <= 2097152, 'Invalid ingress window')
        require(data['raw_bytes'] == sum(row['bytes'] for row in rows), 'Raw byte accounting changed')
        require(all(0 < r['bytes'] <= 65536 and r['bytes'] % 4 == 0 and 0 <= r['consumed_ms'] < 6000
                    and r['ingress_ms'] <= r['consumed_ms'] and math.isclose(r['consumed_ms'] - r['ingress_ms'],
                    r['ingress_to_consumer_ms'], abs_tol=1e-6) for r in rows), 'Raw event invalid')
        require(all(b['ingress_ms'] >= a['ingress_ms'] and b['consumed_ms'] >= a['consumed_ms']
                    for a, b in zip(rows, rows[1:])), 'Event clock order changed')
        require([m['packet_frames'] for m in data['models']] == [960, 1920], 'Models missing')
        for m in data['models']:
            target = m['packet_frames'] * 4;ages, pending, discarded = replay(rows, target)
            require(len(ages) == m['completed'] == len(m['assembly_age_ms']) and len(ages) > 0
                    and m['pending_bytes'] == pending and m['discarded_bytes'] == discarded
                    and data['raw_bytes'] == len(ages) * target + pending + discarded
                    and all(math.isclose(a, b, abs_tol=1e-6) for a, b in zip(ages, m['assembly_age_ms'])),
                    'Modeled assembly accounting changed')
            print(f"slot {index+1} {'motion' if p['motion'] else 'static'}, {m['packet_frames']} frames: "
                  f"{len(ages)} modeled packets; median {statistics.median(ages):.2f} ms, max {max(ages):.2f} ms ingress age")
        total += len(rows)
    print(f'Verified four positions and {total} raw observations. Models exclude encoding/writer work and guest sample age.')


if __name__ == '__main__':
    main()
