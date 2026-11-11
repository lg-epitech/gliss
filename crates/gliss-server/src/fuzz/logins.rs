//! Fuzzes the allowance of failed logins: attempts from any addresses at any
//! times, each of which may succeed at any later moment, next to a model that
//! keeps for each client the attempts counted against it.

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

/// One admitted attempt: who it was counted as, and its number among all
/// attempts.
type Counting = (Counted, u64);

/// For each client, the time its minute began and the attempts admitted in
/// it that have not succeeded.
#[derive(Default)]
struct Model {
    clients: HashMap<Counted, (Instant, Vec<u64>)>,
    attempts: u64,
}

impl Model {
    fn expired(since: Instant, now: Instant) -> bool {
        // A clock that reads earlier than before has not passed a minute.
        now.checked_duration_since(since)
            .is_some_and(|age| age >= WINDOW)
    }

    fn attempt(&mut self, address: IpAddr, now: Instant) -> Option<Counting> {
        let mut client = counted(address);
        if !self.clients.contains_key(&client) && self.clients.len() >= MAX_ADDRESSES {
            self.clients
                .retain(|_, (since, _)| !Self::expired(*since, now));
            if self.clients.len() >= MAX_ADDRESSES {
                client = Counted::Together;
            }
        }
        let (since, failures) = self.clients.entry(client).or_insert((now, Vec::new()));
        if Self::expired(*since, now) {
            // A new minute: what was counted in the last one is forgotten.
            *since = now;
            failures.clear();
        }
        if failures.len() >= FAILURES_PER_WINDOW as usize {
            return None;
        }
        self.attempts += 1;
        failures.push(self.attempts);
        Some((client, self.attempts))
    }

    /// The attempt is no failure after all, if it is still counted as one.
    fn succeeded(&mut self, (client, attempt): Counting) {
        if let Some((_, failures)) = self.clients.get_mut(&client) {
            failures.retain(|counted| *counted != attempt);
            if failures.is_empty() {
                self.clients.remove(&client);
            }
        }
    }

    fn failures(&self) -> usize {
        self.clients
            .values()
            .map(|(_, failures)| failures.len())
            .sum()
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

/// One table through `steps` bursts of attempts and successes from `pool`
/// addresses.
fn session(rng: &mut Rng, pool: usize, steps: usize) {
    let logins = Logins::default();
    let mut model = Model::default();
    // Attempts that were admitted and may still present the right key.
    let mut open = Vec::new();
    let start = Instant::now() + Duration::from_secs(3600);
    let mut now = start;
    for _ in 0..steps {
        match rng.below(12) {
            0 => now += Duration::from_millis(rng.pick(&[1, 1000, 59_999, 60_000, 60_001])),
            // The clock is monotonic, but nothing here depends on it.
            1 => now = start + Duration::from_secs(rng.below(120) as u64),
            _ => {}
        }
        if rng.chance(5) && !open.is_empty() {
            // An attempt succeeds: the newest, as a login that is answered at
            // once, or any older one, whatever has happened since it was counted.
            let which = if rng.chance(2) {
                open.len() - 1
            } else {
                rng.below(open.len())
            };
            let (charge, counting) = open.swap_remove(which);
            logins.succeeded(charge);
            model.succeeded(counting);
        } else {
            let address = address(rng, pool);
            // A burst from one address, as a client that guesses makes.
            for _ in 0..rng.pick(&[1, 1, 1, 5, 40]) {
                match (logins.attempt(address, now), model.attempt(address, now)) {
                    (Some(charge), Some(counting)) => open.push((charge, counting)),
                    (None, None) => {}
                    (charge, _) => panic!(
                        "an attempt was {} where the model says otherwise",
                        if charge.is_some() {
                            "admitted"
                        } else {
                            "refused"
                        }
                    ),
                }
            }
        }
        let clients = logins.clients.lock().unwrap();
        assert!(clients.len() <= MAX_ADDRESSES + 1);
        assert_eq!(clients.len(), model.clients.len());
        assert!(
            clients
                .values()
                .all(|allowance| (1..=FAILURES_PER_WINDOW).contains(&allowance.failures))
        );
        let failures: u32 = clients.values().map(|allowance| allowance.failures).sum();
        assert_eq!(failures as usize, model.failures());
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
