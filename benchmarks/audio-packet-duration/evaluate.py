"""Deterministic prospective selection gate, not a significance test."""
import math
from statistics import mean


def evaluate(data, plan, criteria):
    expected = [(p['name'], mode) for p in plan['profiles'] for mode in plan['order']]
    positions = data['positions']
    if len(positions) != 8 or [(p['profile'], p['mode']) for p in positions] != expected:
        raise ValueError('fixed positions changed')
    all_valid = data['cleanup_complete'] and all(p['status'] == 'measured' and p.get('measurement_valid') is True for p in positions)
    results = []
    for profile in plan['profiles']:
        rows = [p for p in positions if p['profile'] == profile['name']]
        if not all_valid:
            results.append({'profile': profile['name'], 'eligible': False, 'cpu': None, 'candidate_positions_pass': False, 'cpu_gate_pass': False})
            continue
        cpu = {mode: [p['cpu']['native']['percent_of_one_core'] + p['cpu']['browser']['percent_of_one_core'] for p in rows if p['mode'] == mode] for mode in ['ms20', 'ms40']}
        candidate = [p for p in rows if p['mode'] == 'ms40']
        position_gates = []
        for p in candidate:
            m = p['metrics']
            replies = [v['wire_received_ms'] - v['sent_ms'] for v in m['pings'] if v['sent_ms'] is not None and v['wire_received_ms'] is not None]
            p95 = sorted(replies)[math.ceil(len(replies)*.95)-1] if replies else None
            position_gates.append(p['timely_responses'] == criteria['candidate_complete_audio_probes']
                and len(replies) == criteria['candidate_complete_pongs'] and p95 <= criteria['candidate_pong_p95_max_ms']
                and m['unique_source_frames'] / (plan['window_ms']/1000) >= criteria['candidate_marker_rate_min_per_second'])
        reduction = 1 - mean(cpu['ms40']) / mean(cpu['ms20'])
        cpu_gate = reduction >= criteria['cpu_mean_reduction_min_fraction'] and max(cpu['ms40']) < min(cpu['ms20'])
        results.append({'profile':profile['name'], 'eligible':True, 'cpu':cpu,
            'cpu_mean_relative_reduction':reduction, 'candidate_positions_pass':all(position_gates), 'cpu_gate_pass':cpu_gate})
    accepted = all_valid and all(r['candidate_positions_pass'] and r['cpu_gate_pass'] for r in results)
    return {'schema':'gliss-audio-packet-duration-decision-v1','all_eight_eligible':all_valid,'profiles':results,
        'further_testing_candidate':accepted,'default_packet_ms':20,'default_promotion_proved':False,
        'fidelity_acceptance_proved':False,'physical_speaker_latency_proved':False}
