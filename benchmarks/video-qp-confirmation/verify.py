"""Verify the confirmation evidence and recompute its fixed-criteria decision."""
import gzip
import hashlib
import importlib.util
import json
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]


def require(value, message):
    if not value:
        raise ValueError(message)


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def load(name, path, expected):
    require(digest(path.read_bytes()) == expected, name + ' changed')
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main():
    record = json.loads((HERE / 'results.json').read_bytes())
    require(record['schema'] == 'gliss-video-qp-confirmation-inventory-v1', 'Unknown inventory')
    for name in ['criteria', 'decision', 'validation']:
        require(digest((HERE / (name + '.json')).read_bytes()) == record[name + '_sha256'],
                name + ' changed')
    registration = json.loads((HERE / 'registration.json').read_bytes())
    require(registration['status'] == 'prospective' and registration['positions_executed'] == 0
            and registration['plan_sha256'] == record['cohort']['plan_sha256']
            and registration['criteria_sha256'] == record['criteria_sha256'], 'Registration mismatch')
    pair = load('qp_confirmation_pair', ROOT / record['pair_verifier'], record['pair_verifier_sha256'])
    shared = load('qp_confirmation_motion', ROOT / record['shared_verifier'], record['shared_verifier_sha256'])
    shared.HERE = HERE
    data = shared.verify(record['cohort'])
    plan = json.loads((HERE / 'plan.json').read_bytes())
    require(pair.check_plan(plan) == ('floor26', 'floor42'), 'Confirmation policies changed')
    for p in data['positions']:
        q = plan['qp_bounds'][p['mode']]
        require(p['qp_bounds'] == {**q, 'scope': 'requested native bounds; not coded-QP attestation'}
                and p['frame_skip'] is False and p['in_flight_frames'] == 8, 'Live policy changed')
        argv = p['native_argv']
        for flag, value in [('--in-flight-frames', '8'), ('--h264-bitrate', '500000'),
                            ('--video-frame-skip', 'false'), ('--video-qp-min', str(q['min'])),
                            ('--video-qp-max', '51')]:
            require(argv.count(flag) == 1 and argv[argv.index(flag) + 1] == value, 'CLI mismatch')
        require(argv.count('--stream-stats') == 1, 'Shared stats missing')
        m = p['metrics']
        require(m['invalid_video_headers'] == m['sequence_gaps'] == m['invalid_skip_stats'] == 0
                and m['skip_samples'] == [], 'Invalid media diagnostics')
        require(all(type(a['keyframe']) is bool and a['bytes'] >= 25 for a in m['arrivals'])
                and all(b['sequence'] == ((a['sequence'] + 1) & 0xffffffff)
                        for a, b in zip(m['arrivals'], m['arrivals'][1:])), 'Video flags or continuity changed')
    evaluator_path = HERE / 'evaluate.py'
    evaluator = load('qp_confirmation_decision', evaluator_path,
                     registration['sources']['benchmarks/video-qp-confirmation/evaluate.py'])
    decision = evaluator.evaluate(data, json.loads((HERE / 'criteria.json').read_bytes()))
    require(decision == json.loads((HERE / 'decision.json').read_bytes()), 'Decision changed')
    require(decision['accepted'] and not decision['quality_acceptance_proved'], 'Scope changed')
    validation = json.loads((HERE / 'validation.json').read_bytes())
    prior = validation['unchanged_runtime_portable_tests']
    require(digest((HERE / prior['validation']).read_bytes()) == prior['validation_sha256'],
            'Prior test evidence changed')
    require(validation['confirmation_tests'] == {'passed': 4, 'failed': 0}
            and validation['owner_terminal'] == {'active_state': 'inactive', 'main_pid': 0,
                                                'result': 'success', 'exit_status': 0},
            'Validation incomplete')
    for reference in record['quality_references']:
        packed = (HERE / reference['file']).read_bytes()
        require(digest(packed) == reference['compressed_sha256'], 'Image reference changed')
        arm = json.loads(gzip.decompress(packed))['arms'][reference['arm_index']]
        require({'min': arm['qp_min'], 'max': arm['qp_max']} == reference['requested_qp_bounds'],
                'Image policy mismatch')
    print('All four candidate positions pass the preregistered gates. All 48 probes retained; '
          'image acceptability and physical latency remain unproved.')


if __name__ == '__main__':
    main()
