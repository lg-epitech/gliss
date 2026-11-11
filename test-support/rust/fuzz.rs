//! Deterministic fuzzing for tests that run under `cargo test`.
//!
//! The toolchain is stable and nothing may be installed, so there is no
//! coverage feedback here: a fuzz test generates inputs from a grammar,
//! mutates them, and checks every result against a reference model. Each
//! iteration has its own generator, seeded from the campaign seed and the
//! iteration number, so a failure replays alone from the two numbers it prints.
//! `docs/development.md` describes the environment variables.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// SplitMix64: one word of state, and every seed gives a full-period sequence.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut word = self.0;
        word = (word ^ (word >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        word = (word ^ (word >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        word ^ (word >> 31)
    }

    /// A number in `0..bound`. The bias of the remainder is far below what a
    /// test can observe.
    pub fn below(&mut self, bound: usize) -> usize {
        assert!(bound > 0, "an empty range has no member");
        (self.next() % bound as u64) as usize
    }

    /// A number in `low..=high`.
    pub fn between(&mut self, low: usize, high: usize) -> usize {
        low + self.below(high - low + 1)
    }

    /// True once in `one_in` calls.
    pub fn chance(&mut self, one_in: usize) -> bool {
        self.below(one_in) == 0
    }

    pub fn byte(&mut self) -> u8 {
        self.next() as u8
    }

    pub fn bytes(&mut self, length: usize) -> Vec<u8> {
        (0..length).map(|_| self.byte()).collect()
    }

    pub fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len())]
    }
}

/// Values at the edges of what a length, a count or a coordinate can hold.
const EXTREMES: [u32; 16] = [
    0,
    1,
    2,
    0x7f,
    0x80,
    0xff,
    0x100,
    0x7fff,
    0x8000,
    0xffff,
    0x0001_0000,
    0x0010_0001,
    0x7fff_ffff,
    0x8000_0000,
    0xffff_fffe,
    0xffff_ffff,
];

/// Where the next change lands: on a field the generator pointed out, three
/// times in four, because most bytes of a stream are payload.
fn position(rng: &mut Rng, length: usize, fields: &[usize]) -> usize {
    if !fields.is_empty() && !rng.chance(4) {
        (rng.pick(fields) + rng.below(4)).min(length - 1)
    } else {
        rng.below(length)
    }
}

/// Applies one byte-level change: a flipped bit, a replaced, inserted or
/// deleted run, a cut, a run copied from elsewhere, or an extreme value
/// written over a two-byte or four-byte field. `fields` are offsets worth
/// aiming at, and `big_endian` is the byte order of the format's numbers.
pub fn mutate(rng: &mut Rng, bytes: &mut Vec<u8>, fields: &[usize], big_endian: bool) {
    if bytes.is_empty() {
        let length = 1 + rng.below(8);
        *bytes = rng.bytes(length);
        return;
    }
    let at = position(rng, bytes.len(), fields);
    match rng.below(10) {
        0 => bytes[at] ^= 1 << rng.below(8),
        1 => bytes[at] = rng.byte(),
        2 => bytes[at] = rng.pick(&[0, 1, 0x7f, 0x80, 0xfe, 0xff]),
        3 => {
            let length = 1 + rng.below(8);
            bytes.splice(at..at, rng.bytes(length));
        }
        4 => {
            let end = (at + 1 + rng.below(8)).min(bytes.len());
            bytes.drain(at..end);
        }
        5 => bytes.truncate(at),
        6 => {
            // A run from elsewhere in the same input, written over this place
            // or pushed into it: fields end up where other fields are expected.
            let from = rng.below(bytes.len());
            let end = (from + 1 + rng.below(32)).min(bytes.len());
            let run = bytes[from..end].to_vec();
            if rng.chance(2) {
                bytes.splice(at..at, run);
            } else {
                let end = (at + run.len()).min(bytes.len());
                bytes.splice(at..end, run);
            }
        }
        7 | 8 => {
            let value = rng.pick(&EXTREMES);
            let width = if rng.chance(2) { 2 } else { 4 };
            let field = if big_endian {
                value.to_be_bytes()[4 - width..].to_vec()
            } else {
                value.to_le_bytes()[..width].to_vec()
            };
            let end = (at + width).min(bytes.len());
            bytes.splice(at..end, field);
        }
        _ => {
            // A small step up or down, for counts and lengths that are off by one.
            bytes[at] = if rng.chance(2) {
                bytes[at].wrapping_add(1)
            } else {
                bytes[at].wrapping_sub(1)
            };
        }
    }
}

fn variable(name: &str) -> Option<u64> {
    let text = std::env::var(name).ok()?;
    let value = match text.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => text.parse(),
    };
    Some(value.unwrap_or_else(|_| panic!("{name} must be a number, not {text:?}")))
}

/// FNV-1a: each target's default seed, and the fingerprint of a long result
/// in a vector file.
pub fn fingerprint(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Checks the vector file `name` in `test-support/fuzz` against `current`,
/// what the Rust side makes of the same inputs today. The Node tests run the
/// browser's parsers over these files, so the two sides are compared without
/// one test run needing the other. `GLISS_FUZZ_WRITE_VECTORS` rewrites the
/// file instead, after a deliberate change of format or behaviour.
pub fn assert_vectors(name: &str, current: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../test-support/fuzz")
        .join(name);
    if std::env::var_os("GLISS_FUZZ_WRITE_VECTORS").is_some() {
        std::fs::write(&path, current).unwrap();
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed == current,
        "{} is not what this build produces; if that is intended, rewrite it with \
         GLISS_FUZZ_WRITE_VECTORS=1 and run the Node tests",
        path.display()
    );
}

/// Where a campaign writes vectors for the Node tests to read: the file
/// `name` in the directory `GLISS_FUZZ_VECTORS` names, if it names one.
pub fn campaign_vectors(name: &str) -> Option<std::fs::File> {
    let directory = std::path::PathBuf::from(std::env::var_os("GLISS_FUZZ_VECTORS")?);
    std::fs::create_dir_all(&directory).unwrap();
    Some(std::fs::File::create(directory.join(name)).unwrap())
}

/// Says how to replay the iteration that panicked.
struct Replay<'a> {
    target: &'a str,
    seed: u64,
    iteration: u64,
}

impl Drop for Replay<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "fuzz target {} failed at seed {} iteration {}; replay it alone with \
                 GLISS_FUZZ_SEED={} GLISS_FUZZ_START={} GLISS_FUZZ_ITERATIONS=1",
                self.target, self.seed, self.iteration, self.seed, self.iteration
            );
        }
    }
}

/// How long one iteration may take before the run is declared stuck. The
/// slowest iterations take tens of milliseconds, on a busy machine too.
const STUCK: Duration = Duration::from_secs(300);

/// Runs `iteration` with a fresh generator for each iteration number.
///
/// `default_iterations` keeps the suite fast; `GLISS_FUZZ_ITERATIONS` replaces
/// it for a campaign, `GLISS_FUZZ_SEED` selects another sequence of inputs and
/// `GLISS_FUZZ_START` the first iteration number. An iteration that never
/// returns aborts the process with its seed, since a test cannot be cancelled.
pub fn run(target: &str, default_iterations: u64, mut iteration: impl FnMut(&mut Rng, u64)) {
    let seed = variable("GLISS_FUZZ_SEED").unwrap_or_else(|| fingerprint(target.as_bytes()));
    let start = variable("GLISS_FUZZ_START").unwrap_or(0);
    let count = variable("GLISS_FUZZ_ITERATIONS").unwrap_or(default_iterations);
    let progress = Arc::new(AtomicU64::new(start));
    let finished = Arc::new(AtomicBool::new(false));
    let watchdog = std::thread::spawn({
        let (progress, finished, target) = (progress.clone(), finished.clone(), target.to_owned());
        move || {
            let mut seen = (progress.load(Ordering::Relaxed), Instant::now());
            while !finished.load(Ordering::Relaxed) {
                std::thread::park_timeout(Duration::from_millis(200));
                let current = progress.load(Ordering::Relaxed);
                if current != seen.0 {
                    seen = (current, Instant::now());
                } else if seen.1.elapsed() > STUCK {
                    eprintln!(
                        "fuzz target {target} did not finish seed {seed} iteration {current} \
                         in {STUCK:?}"
                    );
                    std::process::abort();
                }
            }
        }
    });
    let started = Instant::now();
    for number in start..start.saturating_add(count) {
        progress.store(number, Ordering::Relaxed);
        let replay = Replay {
            target,
            seed,
            iteration: number,
        };
        // The golden ratio spreads consecutive iteration numbers over the seeds.
        let mut rng = Rng::new(seed ^ number.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        iteration(&mut rng, number);
        drop(replay);
    }
    finished.store(true, Ordering::Relaxed);
    watchdog.thread().unpark();
    watchdog.join().expect("fuzz watchdog");
    eprintln!(
        "fuzz target {target}: {count} iterations from {start} with seed {seed} in {:.1?}",
        started.elapsed()
    );
}

thread_local! {
    /// The largest single request this thread has made of the allocator.
    static LARGEST: Cell<usize> = const { Cell::new(0) };
}

/// The system allocator, which also remembers the largest request of each
/// thread. A test binary that installs it can bound what a piece of code
/// allocates at once, whatever its input claims.
pub struct MeasuringAllocator;

fn requested(size: usize) {
    // A thread that is ending has no cell any more; its requests do not count.
    let _ = LARGEST.try_with(|largest| largest.set(largest.get().max(size)));
}

// SAFETY: every request is passed to the system allocator unchanged.
unsafe impl GlobalAlloc for MeasuringAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        requested(layout.size());
        // SAFETY: the caller's contract is the system allocator's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        requested(layout.size());
        // SAFETY: as above.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        requested(size);
        // SAFETY: as above.
        unsafe { System.realloc(pointer, layout, size) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: as above.
        unsafe { System.dealloc(pointer, layout) }
    }
}

/// Runs `work` without counting what it allocates: a test's own bookkeeping
/// shares the thread with the code it measures.
pub fn unmeasured<T>(work: impl FnOnce() -> T) -> T {
    let before = largest_allocation();
    let result = work();
    LARGEST.with(|largest| largest.set(before));
    result
}

/// Forgets the largest request of this thread so far.
pub fn reset_largest_allocation() {
    LARGEST.with(|largest| largest.set(0));
}

/// The largest request of this thread since the last reset, in bytes. Zero in
/// a binary that has not installed [`MeasuringAllocator`].
pub fn largest_allocation() -> usize {
    LARGEST.with(Cell::get)
}
