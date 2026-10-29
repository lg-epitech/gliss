//! CPU clocks sampled only by explicit stream diagnostics.
use serde_json::{Value, json};
use std::time::Duration;

#[cfg(target_os = "linux")]
pub fn thread_cpu_time() -> Option<Duration> {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // This clock follows the current blocking worker, not elapsed wall time.
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut value) } != 0
        || value.tv_sec < 0
        || !(0..1_000_000_000).contains(&value.tv_nsec)
    {
        return None;
    }
    Some(Duration::new(value.tv_sec as u64, value.tv_nsec as u32))
}
#[cfg(not(target_os = "linux"))]
pub fn thread_cpu_time() -> Option<Duration> {
    None
}

pub fn process_cpu(elapsed: Duration) -> Value {
    process_times().map_or(Value::Null, |(user, system)| {
        json!({
            "elapsed_ms": elapsed.as_secs_f64() * 1000.0,
            "user_ms": user, "system_ms": system,
            "scope": "process including all sessions and threads"
        })
    })
}
#[cfg(unix)]
fn process_times() -> Option<(f64, f64)> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return None;
    }
    // getrusage initializes the output only on success.
    let usage = unsafe { usage.assume_init() };
    let millis = |time: libc::timeval| {
        (time.tv_sec >= 0 && (0..1_000_000).contains(&time.tv_usec))
            .then_some(time.tv_sec as f64 * 1000.0 + time.tv_usec as f64 / 1000.0)
    };
    Some((millis(usage.ru_utime)?, millis(usage.ru_stime)?))
}
#[cfg(not(unix))]
fn process_times() -> Option<(f64, f64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[cfg(target_os = "linux")]
    fn cpu_clocks_are_nonnegative_and_report_no_identity_or_paths() {
        let before = thread_cpu_time().unwrap();
        let after = thread_cpu_time().unwrap();
        assert!(after >= before);
        let cpu = process_cpu(Duration::from_millis(42));
        assert_eq!(cpu["elapsed_ms"], 42.0);
        assert!(cpu["user_ms"].as_f64().unwrap() >= 0.0);
        assert!(cpu["system_ms"].as_f64().unwrap() >= 0.0);
        assert_eq!(cpu.as_object().unwrap().len(), 4);
        assert!(cpu["scope"].as_str().unwrap().contains("all sessions"));
    }
}
