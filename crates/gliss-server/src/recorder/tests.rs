use super::*;
use std::sync::Barrier;

fn clock() -> ClockDescription {
    ClockDescription {
        domain_id: 7,
        process_id: 123,
        kind: ClockKind::RustInstant,
        origin_monotonic_ns: None,
        resolution_ns: None,
    }
}
fn stamp(delta_ns: u64) -> Stamp {
    Stamp {
        domain_id: 7,
        delta_ns,
    }
}

#[test]
fn all_inline_storage_and_export_fit_four_mib_and_existing_reader_bounds() {
    const {
        assert!(size_of::<Slot>() == 64);
    }
    const {
        assert!(size_of::<Event>() == 56);
    }
    const {
        assert!(EVENT_CAPACITY == 65_280);
    }
    const {
        assert!(OWNED_FOOTPRINT_BOUND_BYTES <= STORAGE_BUDGET_BYTES);
    }
    const {
        assert!(MAX_EXPORT_BYTES <= STORAGE_BUDGET_BYTES);
    }
    // Existing GLSTREC1 reader has this lower object-footprint check. The larger
    // atomic slots satisfy it honestly; the header does not invent padding.
    const {
        assert!(size_of::<Recorder>() >= ((STORAGE_BUDGET_BYTES - 16 * 1024) / 56) * 56);
    }
}

#[test]
fn disabled_or_closed_closure_does_not_sample_a_clock() {
    assert_eq!(
        record_if_enabled(None, || panic!("disabled clock")),
        Append::Disabled
    );
    static TRACE: Recorder = Recorder::new();
    assert_eq!(
        record_if_enabled(Some(&TRACE), || panic!("unstarted clock")),
        Append::Closed
    );
    TRACE.start(clock()).unwrap();
    TRACE.stop().unwrap();
    assert_eq!(
        record_if_enabled(Some(&TRACE), || panic!("closed clock")),
        Append::Closed
    );
}

#[test]
fn startup_is_one_shot_and_invalid_configuration_does_not_open_admission() {
    static TRACE: Recorder = Recorder::new();
    assert_eq!(TRACE.stop(), Err(Error::NotStarted));
    assert_eq!(TRACE.start_with_limit(clock(), 0), Err(Error::InvalidLimit));
    assert_eq!(
        TRACE.start_with_limit(clock(), EVENT_CAPACITY + 1),
        Err(Error::InvalidLimit)
    );
    let mut bad = clock();
    bad.domain_id = 0;
    assert_eq!(TRACE.start(bad), Err(Error::InvalidClock));
    bad = clock();
    bad.resolution_ns = Some(0);
    assert_eq!(TRACE.start(bad), Err(Error::InvalidClock));
    bad = clock();
    bad.origin_monotonic_ns = Some(0);
    assert_eq!(TRACE.start(bad), Err(Error::InvalidClock));
    assert!(!TRACE.is_collecting());
    TRACE.start_with_limit(clock(), 2).unwrap();
    assert_eq!(TRACE.start(clock()), Err(Error::AlreadyStarted));
    let frozen = TRACE.try_freeze().unwrap();
    assert_eq!(frozen.summary().capacity, 2);
    assert_eq!(frozen.summary().recorded, 0);
    drop(frozen);
    assert_eq!(TRACE.start(clock()), Err(Error::AlreadyStarted));
}

#[test]
fn scalar_words_preserve_all_fields_valid_zero_wrap_and_marker_states() {
    let fields = Fields {
        input_id: Some(0),
        generation: Some(u64::MAX),
        request_ordinal: Some(0),
        epoch: Some(0),
        sequence: Some(u32::MAX),
        source_marker: Marker::Valid(0),
        response_marker: Marker::Mixed,
        count: Some(u32::MAX),
        outcome: Outcome::Ambiguous,
    };
    let event = Event::new(stamp(u64::MAX), EventKind::CaptureMarker, fields);
    assert_eq!(Event::from_words(event.words()), event);
    assert_eq!(event.fields(), fields);
    assert_eq!(
        event.frame_key(),
        Some(FrameKey {
            generation: u64::MAX,
            epoch: 0,
            sequence: u32::MAX
        })
    );
    for marker in [
        Marker::Unobserved,
        Marker::Invalid,
        Marker::Mixed,
        Marker::Valid(0),
        Marker::Valid(u32::MAX),
    ] {
        let original = Event::new(
            stamp(0),
            EventKind::CaptureMarker,
            Fields {
                source_marker: marker,
                response_marker: marker,
                ..Fields::default()
            },
        );
        assert_eq!(
            Event::from_words(original.words()).fields(),
            original.fields()
        );
    }
}

#[test]
fn configuration_mutex_contention_does_not_omit_or_block_appends() {
    static TRACE: Recorder = Recorder::new();
    TRACE.start(clock()).unwrap();
    let held = TRACE.collection.lock().unwrap();
    let result =
        std::thread::spawn(|| TRACE.append(stamp(55), EventKind::MediaObserved, Fields::default()))
            .join()
            .unwrap();
    assert_eq!(result, Append::Recorded { ordinal: 0 });
    drop(held);
    let frozen = TRACE.try_freeze().unwrap();
    assert_eq!(frozen.summary().contention, 0);
    assert_eq!(frozen.events().next().unwrap().delta_ns(), 55);
    assert!(frozen.summary().complete());
}

#[test]
fn eight_real_concurrent_writers_never_tear_duplicate_or_lose_payloads() {
    static TRACE: Recorder = Recorder::new();
    TRACE.start(clock()).unwrap();
    const THREADS: usize = 8;
    const PER_THREAD: usize = 2048;
    let barrier = Barrier::new(THREADS);
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for thread in 0..THREADS {
            let barrier = &barrier;
            handles.push(scope.spawn(move || {
                barrier.wait();
                for index in 0..PER_THREAD {
                    let id = (thread * PER_THREAD + index) as u64;
                    let result = TRACE.append(
                        stamp(id),
                        EventKind::EncodeCompleted,
                        Fields {
                            input_id: Some(id),
                            generation: Some(id ^ 0xa55a_a55a_a55a_a55a),
                            request_ordinal: Some(id * 2),
                            epoch: Some(thread as u32),
                            sequence: Some(index as u32),
                            source_marker: Marker::Valid(id as u32),
                            response_marker: Marker::Valid(!(id as u32)),
                            count: Some((id as u32).wrapping_mul(7)),
                            outcome: Outcome::Accepted,
                        },
                    );
                    assert!(matches!(result, Append::Recorded { .. }));
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
    });
    let frozen = TRACE.try_freeze().unwrap();
    assert_eq!(frozen.summary().recorded, THREADS * PER_THREAD);
    assert!(frozen.summary().complete());
    assert_eq!(frozen.summary().contention, 0);
    let mut ids = Vec::with_capacity(THREADS * PER_THREAD);
    for event in frozen.events() {
        let fields = event.fields();
        let id = fields.input_id.unwrap();
        ids.push(id);
        assert_eq!(event.delta_ns(), id);
        assert_eq!(fields.generation, Some(id ^ 0xa55a_a55a_a55a_a55a));
        assert_eq!(fields.request_ordinal, Some(id * 2));
        assert_eq!(fields.epoch, Some((id as usize / PER_THREAD) as u32));
        assert_eq!(fields.sequence, Some((id as usize % PER_THREAD) as u32));
        assert_eq!(fields.source_marker, Marker::Valid(id as u32));
        assert_eq!(fields.response_marker, Marker::Valid(!(id as u32)));
        assert_eq!(fields.count, Some((id as u32).wrapping_mul(7)));
        assert_eq!(fields.outcome, Outcome::Accepted);
    }
    ids.sort_unstable();
    assert_eq!(ids, (0..(THREADS * PER_THREAD) as u64).collect::<Vec<_>>());
}

#[test]
fn stop_freeze_waits_for_a_real_registered_partially_written_slot() {
    static TRACE: Recorder = Recorder::new();
    TRACE.start(clock()).unwrap();
    let entered = Barrier::new(2);
    let release = Barrier::new(2);
    let (busy, closed) = std::thread::scope(|scope| {
        let writer = scope.spawn(|| {
            let attempt = TRACE.attempt().unwrap();
            let ordinal = TRACE.next.fetch_add(1, Ordering::Relaxed);
            TRACE.slots[ordinal].words[0].store(999, Ordering::Relaxed);
            entered.wait();
            release.wait();
            TRACE.slots[ordinal].publish(Event::new(
                stamp(40),
                EventKind::CapturePublished,
                Fields {
                    generation: Some(0),
                    ..Fields::default()
                },
            ));
            drop(attempt);
        });
        entered.wait();
        let busy = matches!(TRACE.try_freeze(), Err(Error::Busy));
        let closed = TRACE.append(stamp(99), EventKind::InputDequeued, Fields::default());
        release.wait();
        writer.join().unwrap();
        (busy, closed)
    });
    assert!(busy);
    assert_eq!(closed, Append::Closed);
    let frozen = TRACE.try_freeze().unwrap();
    assert_eq!(frozen.summary().recorded, 1);
    let event = frozen.events().next().unwrap();
    assert_eq!(event.delta_ns(), 40);
    assert_eq!(event.fields().generation, Some(0));
    assert!(frozen.summary().complete());
}

#[test]
fn real_stop_race_conserves_recorded_and_closed_attempts_without_late_mutation() {
    static TRACE: Recorder = Recorder::new();
    TRACE.start(clock()).unwrap();
    let barrier = Barrier::new(5);
    let recorded = AtomicUsize::new(0);
    let closed = AtomicUsize::new(0);
    let unexpected = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for thread in 0..4 {
            let barrier = &barrier;
            let recorded = &recorded;
            let closed = &closed;
            let unexpected = &unexpected;
            handles.push(scope.spawn(move || {
                for index in 0..1000 {
                    if index == 500 {
                        barrier.wait();
                    }
                    match TRACE.append(
                        stamp((thread * 1000 + index) as u64),
                        EventKind::MediaObserved,
                        Fields::default(),
                    ) {
                        Append::Recorded { .. } => {
                            recorded.fetch_add(1, Ordering::Relaxed);
                        }
                        Append::Closed => {
                            closed.fetch_add(1, Ordering::Relaxed);
                        }
                        _ => {
                            unexpected.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }));
        }
        barrier.wait();
        TRACE.stop().unwrap();
        for handle in handles {
            handle.join().unwrap();
        }
    });
    let frozen = TRACE.try_freeze().unwrap();
    assert_eq!(unexpected.load(Ordering::Relaxed), 0);
    assert_eq!(
        recorded.load(Ordering::Relaxed) + closed.load(Ordering::Relaxed),
        4000
    );
    assert!(recorded.load(Ordering::Relaxed) >= 2000);
    assert_eq!(frozen.summary().recorded, recorded.load(Ordering::Relaxed));
    assert!(frozen.summary().complete());
    let before = frozen.events().count();
    assert_eq!(
        TRACE.append(stamp(99999), EventKind::ControlReceived, Fields::default()),
        Append::Closed
    );
    assert_eq!(frozen.events().count(), before);
}

#[test]
fn overflow_counts_ranges_and_foreign_clock_remain_explicit() {
    static TRACE: Recorder = Recorder::new();
    TRACE.start_with_limit(clock(), 2).unwrap();
    for id in 0..2 {
        assert_eq!(
            TRACE.append(stamp(id), EventKind::ControlReceived, Fields::default()),
            Append::Recorded {
                ordinal: id as usize
            }
        );
    }
    for at in [90, 40, 70] {
        assert_eq!(
            TRACE.append(stamp(at), EventKind::EncodeCompleted, Fields::default()),
            Append::Omitted(Omission::Full)
        );
    }
    assert_eq!(
        TRACE.append(
            Stamp {
                domain_id: 9,
                delta_ns: 999
            },
            EventKind::UpdateHeader,
            Fields::default()
        ),
        Append::Omitted(Omission::WrongClockDomain)
    );
    let frozen = TRACE.try_freeze().unwrap();
    assert_eq!(
        frozen
            .events()
            .map(|event| event.delta_ns())
            .collect::<Vec<_>>(),
        [0, 1]
    );
    assert_eq!(frozen.summary().overflow, 3);
    assert_eq!(frozen.summary().wrong_clock, 1);
    assert_eq!(
        frozen.summary().losses[EventKind::EncodeCompleted as usize],
        LossSummary {
            count: 3,
            timed_count: 3,
            first_ns: Some(40),
            last_ns: Some(90)
        }
    );
    assert_eq!(
        frozen.summary().losses[EventKind::UpdateHeader as usize],
        LossSummary {
            count: 1,
            timed_count: 0,
            first_ns: None,
            last_ns: None
        }
    );
    assert!(!frozen.summary().complete());
}

#[test]
fn concurrent_overflow_has_a_finite_unique_index_and_conserved_kind_ledger() {
    static TRACE: Recorder = Recorder::new();
    TRACE.start_with_limit(clock(), 3).unwrap();
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for thread in 0..8 {
            handles.push(scope.spawn(move || {
                for index in 0..64 {
                    TRACE.append(
                        stamp((thread * 64 + index) as u64),
                        EventKind::MediaObserved,
                        Fields::default(),
                    );
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
    });
    let frozen = TRACE.try_freeze().unwrap();
    assert_eq!(TRACE.next.load(Ordering::Relaxed), 3);
    assert_eq!(frozen.summary().recorded, 3);
    assert_eq!(frozen.summary().overflow, 509);
    assert_eq!(
        frozen.summary().losses[EventKind::MediaObserved as usize].count,
        509
    );
    assert_eq!(
        frozen.summary().losses[EventKind::MediaObserved as usize].timed_count,
        509
    );
    assert_eq!(frozen.summary().contention, 0);
}

#[test]
fn full_declared_capacity_never_wraps_or_overwrites() {
    static TRACE: Recorder = Recorder::new();
    TRACE.start(clock()).unwrap();
    for index in 0..EVENT_CAPACITY {
        assert_eq!(
            TRACE.append(
                stamp(index as u64),
                EventKind::MediaObserved,
                Fields::default()
            ),
            Append::Recorded { ordinal: index }
        );
    }
    assert_eq!(
        TRACE.append(stamp(u64::MAX), EventKind::MediaObserved, Fields::default()),
        Append::Omitted(Omission::Full)
    );
    let frozen = TRACE.try_freeze().unwrap();
    assert_eq!(frozen.events().len(), EVENT_CAPACITY);
    assert_eq!(frozen.events().next().unwrap().delta_ns(), 0);
    assert_eq!(
        frozen.events().last().unwrap().delta_ns(),
        (EVENT_CAPACITY - 1) as u64
    );
    assert_eq!(TRACE.next.load(Ordering::Relaxed), EVENT_CAPACITY);
}

#[test]
fn saturated_ledger_and_poisoned_configuration_fail_completeness() {
    static TRACE: Recorder = Recorder::new();
    TRACE.start_with_limit(clock(), 1).unwrap();
    TRACE.append(stamp(1), EventKind::ControlReceived, Fields::default());
    TRACE.overflow.store(u64::MAX, Ordering::Relaxed);
    let loss = &TRACE.losses[EventKind::ControlReceived as usize];
    loss.count.store(u64::MAX, Ordering::Relaxed);
    loss.timed_count.store(u64::MAX, Ordering::Relaxed);
    loss.first_ns.store(5, Ordering::Relaxed);
    loss.last_ns.store(5, Ordering::Relaxed);
    TRACE.append(stamp(10), EventKind::ControlReceived, Fields::default());
    let frozen = TRACE.try_freeze().unwrap();
    assert!(frozen.summary().counter_saturated);
    assert!(!frozen.summary().complete());
    assert_eq!(frozen.summary().losses[0].first_ns, Some(5));
    assert_eq!(frozen.summary().losses[0].last_ns, Some(10));

    static POISONED: Recorder = Recorder::new();
    POISONED.start(clock()).unwrap();
    assert!(
        std::thread::spawn(|| {
            let _held = POISONED.collection.lock().unwrap();
            panic!("actual configuration poison");
        })
        .join()
        .is_err()
    );
    // Append has no lock to poison, but terminal export still exposes the failed configuration lock.
    assert_eq!(
        POISONED.append(stamp(3), EventKind::InputDequeued, Fields::default()),
        Append::Recorded { ordinal: 0 }
    );
    let frozen = POISONED.try_freeze().unwrap();
    assert!(frozen.summary().poisoned);
    assert!(!frozen.summary().complete());
}

#[test]
fn wire_export_matches_explicit_little_endian_payload_and_footprint() {
    static TRACE: Recorder = Recorder::new();
    TRACE.start(clock()).unwrap();
    TRACE.append(
        stamp(123),
        EventKind::CapturePublished,
        Fields {
            generation: Some(0),
            epoch: Some(0),
            sequence: Some(u32::MAX),
            source_marker: Marker::Valid(0),
            response_marker: Marker::Invalid,
            count: Some(0),
            outcome: Outcome::PixelsChanged,
            ..Fields::default()
        },
    );
    let frozen = TRACE.try_freeze().unwrap();
    let mut bytes = Vec::new();
    assert_eq!(frozen.write_binary(&mut bytes).unwrap(), bytes.len());
    assert_eq!(
        bytes.len(),
        FIXED_HEADER_BYTES + KIND_COUNT * LOSS_ROW_BYTES + EVENT_BYTES
    );
    assert_eq!(&bytes[..8], b"GLSTREC1");
    let word = |at| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
    assert_eq!(word(24), STORAGE_BUDGET_BYTES as u64);
    assert_eq!(word(32), size_of::<Recorder>() as u64);
    assert_eq!(word(40), EVENT_CAPACITY as u64);
    assert_eq!(word(48), 1);
    assert_eq!(word(120), 0);
    let record = &bytes[FIXED_HEADER_BYTES + KIND_COUNT * LOSS_ROW_BYTES..];
    assert_eq!(u64::from_le_bytes(record[..8].try_into().unwrap()), 123);
    assert_eq!(
        u32::from_le_bytes(record[36..40].try_into().unwrap()),
        u32::MAX
    );
    assert_eq!(record[52], EventKind::CapturePublished as u8);
    assert_eq!(
        record[53],
        HAS_GENERATION | HAS_EPOCH | HAS_SEQUENCE | HAS_SOURCE | HAS_COUNT
    );
    assert_eq!(record[54], 3 | (1 << 2));
    assert_eq!(record[55], Outcome::PixelsChanged as u8);
}

#[test]
fn partial_export_error_does_not_reopen_or_invent_a_successful_extent() {
    struct Broken(Vec<u8>);
    impl Write for Broken {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if !self.0.is_empty() {
                return Err(io::Error::other("actual partial write"));
            }
            self.0.extend_from_slice(&bytes[..3]);
            Ok(3)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    static TRACE: Recorder = Recorder::new();
    TRACE.start(clock()).unwrap();
    let frozen = TRACE.try_freeze().unwrap();
    let mut target = Broken(Vec::new());
    assert!(frozen.write_binary(&mut target).is_err());
    assert_eq!(target.0, b"GLS");
    assert_eq!(
        TRACE.append(stamp(5), EventKind::MediaObserved, Fields::default()),
        Append::Closed
    );
}
