//! Cases a fuzzer of the media window found.

use std::time::{Duration, Instant};

use super::{ACK_TIMEOUT, Window};

/// The deadline was taken from the first
/// unacknowledged frame in the queue that had been written, which is the
/// oldest write only while writes are reported in the order of the queue.
/// The session's one writer does report them so; the window did not say it
/// relied on that.
#[test]
fn a_write_reported_after_a_newer_one_does_not_move_the_deadline() {
    let start = Instant::now();
    let mut window = Window::new(2, 100).unwrap();
    window.reserve(1, 20, start).unwrap();
    window.reserve(2, 20, start).unwrap();
    window.written(0, 2, start, Duration::ZERO).unwrap();
    assert_eq!(window.oldest_deadline(), Some(start + ACK_TIMEOUT));
    let later = start + Duration::from_secs(5);
    window.written(0, 1, later, Duration::ZERO).unwrap();
    assert_eq!(window.oldest_deadline(), Some(start + ACK_TIMEOUT));
}
