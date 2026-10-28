"""Apply the prospectively fixed confirmation criteria to original positions."""
import math


def evaluate(data, criteria):
    if criteria['status'] != 'prospective' or criteria['positions_executed'] != 0:
        raise ValueError('Criteria must precede execution')
    positions = data['positions']
    expected = [(profile, mode) for profile in ['loopback', 'kernel-limited']
                for mode in [criteria['control'], criteria['candidate'],
                             criteria['candidate'], criteria['control']]]
    if len(positions) != 8 or [(p['profile'], p['mode']) for p in positions] != expected:
        raise ValueError('Fixed positions missing or reordered')
    if [p['slot'] for p in positions] != list(range(1, 9)):
        raise ValueError('Position identities changed')
    rows = []
    failures = []
    if data['cleanup_complete'] is not True:
        failures.append('cohort cleanup incomplete')
    for p in positions:
        if p['status'] != 'measured' or p.get('measurement_valid') is not True:
            failures.append(f"slot {p['slot']}: invalid or incomplete measurement")
        if p['mode'] != criteria['candidate']:
            continue
        row = {'slot': p['slot'], 'profile': p['profile'], 'accepted': False}
        if p['status'] != 'measured' or p.get('measurement_valid') is not True:
            rows.append(row)
            continue
        m = p['metrics']
        rules = criteria['each_candidate_position']
        replies = [ping['wire_received_ms'] - ping['sent_ms'] for ping in m['pings']
                   if ping['wire_received_ms'] is not None and ping['sent_ms'] is not None]
        complete = len(m['pings']) == len(replies) == rules['complete_wire_pongs']
        p95 = sorted(replies)[math.ceil(len(replies) * .95) - 1] if complete else None
        timely = sum(pulse['status'] == 'observed' and pulse['trigger_to_observed_ms'] is not None
                     and 0 <= pulse['trigger_to_observed_ms'] < 700 for pulse in p['pulses'])
        unique = len({paint['source_frame'] for paint in m['paints']
                      if paint['source_frame'] is not None})
        refreshes = sum(a['keyframe'] is True for a in m['arrivals'])
        checks = {'sound': len(p['pulses']) == timely == p['timely_responses'] ==
                  rules['sound_responses_before_700ms'],
                  'complete_controls': complete,
                  'control_p95': p95 is not None and 0 <= p95 <= rules['wire_p95_max_ms'],
                  'marker_cadence': unique == m['unique_source_frames'] and
                  unique >= rules['distinct_painted_source_ids_min'],
                  'refresh_in_window': refreshes >= rules['refresh_pictures_min'],
                  'headers': m['invalid_video_headers'] <= rules['invalid_video_headers_max'],
                  'decoder': m['video_errors_delta'] <= rules['video_decode_errors_max'],
                  'sequence': m['sequence_gaps'] <= rules['sequence_gaps_max'],
                  'observer': m['overflow'] is False and m['observer_failure'] is None and
                  m['invalid_stamp_paints'] == 0,
                  'native_exit': p['native_exit'] == {'code': 0, 'signal': None}}
        row.update(timely_sound=timely, wire_pongs=len(replies), wire_p95_ms=p95,
                   distinct_painted_ids=unique, refresh_pictures=refreshes,
                   checks=checks, accepted=all(checks.values()))
        failures.extend(f"slot {p['slot']}: {name}" for name, passed in checks.items() if not passed)
        rows.append(row)
    if [row['slot'] for row in rows] != criteria['required_candidate_slots']:
        raise ValueError('Candidate slots changed')
    return {'schema': 'gliss-video-qp-confirmation-decision-v1',
            'accepted': not failures, 'candidate': criteria['candidate'],
            'positions': rows, 'failures': failures, 'quality_acceptance_proved': False}
