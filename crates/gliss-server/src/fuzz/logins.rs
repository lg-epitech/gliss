//! Fuzzes the allowance of failed logins: attempts and successes from any
//! addresses at any times, next to a model that keeps one count for each
//! client.

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    time::{Duration, Instant},
};

use super::{FAILURES_PER_WINDOW, Logins, MAX_ADDRESSES, WINDOW};
use crate::fuzz::support::{self, Rng};

/// Who an address is counted as.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Counted {
    V4([u8; 4]),
    /// The network an IPv6 address is in: its first eight bytes.
    V6([u8; 8]),
    /// Everyone there was no room to count alone.
    Together,
}

fn counted(address: IpAddr) -> Counted {
    match address {
        IpAddr::V4(address) => Counted::V4(address.octets()),
        IpAddr::V6(address) => match address.octets() {
            // An IPv4 client that reached an IPv6 socket.
            [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, a, b, c, d] => Counted::V4([a, b, c, d]),
            octets => Counted::V6(octets[..8].try_into().unwrap()),
        },
    }
}

/// The time an allowance began and the failures taken from it since.
#[derive(Default)]
struct Model(HashMap<Counted, (Instant, u32)>);

impl Model {
    fn expired(since: Instant, now: Instant) -> bool {
        // A clock that reads earlier than before has not passed a minute.
        now.checked_duration_since(since)
            .is_some_and(|age| age >= WINDOW)
    }

    fn attempt(&mut self, address: IpAddr, now: Instant) -> bool {
        let mut client = counted(address);
        if !self.0.contains_key(&client) && self.0.len() >= MAX_ADDRESSES {
            self.0.retain(|_, (since, _)| !Self::expired(*since, now));
            if self.0.len() >= MAX_ADDRESSES {
                client = Counted::Together;
            }
        }
        let (since, failures) = self.0.entry(client).or_insert((now, 0));
        if Self::expired(*since, now) {
            (*since, *failures) = (now, 0);
        }
        if *failures >= FAILURES_PER_WINDOW {
            return false;
        }
        *failures += 1;
        true
    }

    fn succeeded(&mut self, address: IpAddr) {
        let own = counted(address);
        let client = if self.0.contains_key(&own) {
            own
        } else {
            Counted::Together
        };
        if let Some((_, failures)) = self.0.get_mut(&client) {
            *failures = failures.saturating_sub(1);
            if *failures == 0 {
                self.0.remove(&client);
            }
        }
    }
}

/// One of `pool` addresses: IPv4, IPv4 as seen through an IPv6 socket, and
/// IPv6 addresses that share networks.
fn address(rng: &mut Rng, pool: usize) -> IpAddr {
    let host = rng.below(pool) as u32;
    match rng.below(4) {
        0 => IpAddr::V4(Ipv4Addr::from(0x0a00_0000 + host)),
        1 => IpAddr::V6(Ipv4Addr::from(0x0a00_0000 + host).to_ipv6_mapped()),
        _ => {
            let network = u128::from(0x2001_0db8_0000_0000u64 + u64::from(host)) << 64;
            IpAddr::V6(Ipv6Addr::from(network | u128::from(rng.next())))
        }
    }
}

/// One table through `steps` attempts and successes from `pool` addresses.
fn session(rng: &mut Rng, pool: usize, steps: usize) {
    let logins = Logins::default();
    let mut model = Model::default();
    let start = Instant::now() + Duration::from_secs(3600);
    let mut now = start;
    for _ in 0..steps {
        match rng.below(12) {
            0 => now += Duration::from_millis(rng.pick(&[1, 1000, 59_999, 60_000, 60_001])),
            // The clock is monotonic, but nothing here depends on it.
            1 => now = start + Duration::from_secs(rng.below(120) as u64),
            _ => {}
        }
        let address = address(rng, pool);
        if rng.chance(5) {
            logins.succeeded(address);
            model.succeeded(address);
        } else {
            // A burst from one address, as a client that guesses makes.
            for _ in 0..rng.pick(&[1, 1, 1, 5, 40]) {
                assert_eq!(logins.attempt(address, now), model.attempt(address, now));
            }
        }
        let clients = logins.clients.lock().unwrap();
        assert!(clients.len() <= MAX_ADDRESSES + 1);
        assert_eq!(clients.len(), model.0.len());
        assert!(
            clients
                .values()
                .all(|allowance| (1..=FAILURES_PER_WINDOW).contains(&allowance.failures))
        );
    }
}

const DEFAULT_ITERATIONS: u64 = 40;

#[test]
fn fuzz_login_allowance() {
    support::run(
        "server-login-allowance",
        DEFAULT_ITERATIONS,
        |rng, iteration| {
            if iteration % 40 == 7 {
                // More addresses than the table follows, to fill it and go on.
                session(rng, MAX_ADDRESSES * 2, MAX_ADDRESSES + 600);
            } else {
                let pool = rng.pick(&[1, 3, 40]);
                let steps = rng.between(1, 300);
                session(rng, pool, steps);
            }
        },
    );
}
