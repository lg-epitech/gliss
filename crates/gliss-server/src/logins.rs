//! The allowance of failed logins, kept per client address.
//!
//! One counter for everyone would let any client that can reach the listener
//! use it up and keep every other client out. Behind a reverse proxy every
//! client arrives from the proxy's address and they share one allowance
//! again: forwarding headers are not read, because nothing says which proxy
//! may be believed.

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr},
    sync::Mutex,
    time::{Duration, Instant},
};

/// Failed logins one address may make in a [`WINDOW`]. A person retyping a
/// key stays far below it, and a key of the required 24 characters cannot be
/// searched at this rate. It is the number the single shared counter had.
pub const FAILURES_PER_WINDOW: u32 = 30;
const WINDOW: Duration = Duration::from_secs(60);
/// Addresses followed at once: about 150 KiB. An entry lasts a minute, so
/// only thousands of addresses failing together reach this. The addresses
/// beyond it share one allowance, so the total stays limited.
const MAX_ADDRESSES: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Client {
    V4(Ipv4Addr),
    /// The first 64 bits: one IPv6 subscriber holds all the addresses behind them.
    V6Network(u64),
    /// Every address there was no room to follow on its own.
    Others,
}

impl Client {
    fn of(address: IpAddr) -> Self {
        match address.to_canonical() {
            IpAddr::V4(address) => Self::V4(address),
            IpAddr::V6(address) => Self::V6Network((u128::from(address) >> 64) as u64),
        }
    }
}

struct Allowance {
    since: Instant,
    failures: u32,
}

#[derive(Default)]
pub struct Logins {
    clients: Mutex<HashMap<Client, Allowance>>,
}

impl Logins {
    /// Takes one failure from the allowance of `address` before its request
    /// is looked at, and returns false when none is left. Counting first
    /// means requests in flight together cannot exceed the allowance.
    /// [`Logins::succeeded`] gives the failure back.
    pub fn attempt(&self, address: IpAddr, now: Instant) -> bool {
        let mut clients = self.clients.lock().unwrap_or_else(|lock| lock.into_inner());
        let mut client = Client::of(address);
        if !clients.contains_key(&client) && clients.len() >= MAX_ADDRESSES {
            clients.retain(|_, allowance| now.saturating_duration_since(allowance.since) < WINDOW);
            if clients.len() >= MAX_ADDRESSES {
                client = Client::Others;
            }
        }
        let allowance = clients.entry(client).or_insert(Allowance {
            since: now,
            failures: 0,
        });
        if now.saturating_duration_since(allowance.since) >= WINDOW {
            *allowance = Allowance {
                since: now,
                failures: 0,
            };
        }
        if allowance.failures >= FAILURES_PER_WINDOW {
            return false;
        }
        allowance.failures += 1;
        true
    }

    /// The attempt presented the right key: it was not a failure.
    pub fn succeeded(&self, address: IpAddr) {
        let mut clients = self.clients.lock().unwrap_or_else(|lock| lock.into_inner());
        // Counted under the address itself, or with the others when there was no room.
        for client in [Client::of(address), Client::Others] {
            if let Some(allowance) = clients.get_mut(&client) {
                allowance.failures = allowance.failures.saturating_sub(1);
                if allowance.failures == 0 {
                    clients.remove(&client);
                }
                return;
            }
        }
    }
}

#[cfg(test)]
#[path = "fuzz/logins.rs"]
mod fuzz;

#[cfg(test)]
mod tests {
    use super::*;

    fn address(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    /// How many of `tries` failed logins are looked at.
    fn admitted(logins: &Logins, from: &str, tries: u32, now: Instant) -> u32 {
        (0..tries)
            .filter(|_| logins.attempt(address(from), now))
            .count() as u32
    }

    #[test]
    fn an_address_that_fails_does_not_use_up_the_allowance_of_another() {
        let (logins, now) = (Logins::default(), Instant::now());
        assert_eq!(
            admitted(&logins, "203.0.113.7", 1000, now),
            FAILURES_PER_WINDOW
        );
        assert!(!logins.attempt(address("203.0.113.7"), now));
        assert!(logins.attempt(address("203.0.113.8"), now));
        assert!(logins.attempt(address("2001:db8::1"), now));
    }

    #[test]
    fn successful_logins_leave_the_allowance_and_the_table_as_they_were() {
        let (logins, now) = (Logins::default(), Instant::now());
        for _ in 0..1000 {
            assert!(logins.attempt(address("203.0.113.7"), now));
            logins.succeeded(address("203.0.113.7"));
        }
        assert!(logins.clients.lock().unwrap().is_empty());
        assert_eq!(
            admitted(&logins, "203.0.113.7", 40, now),
            FAILURES_PER_WINDOW
        );
    }

    #[test]
    fn the_allowance_comes_back_a_minute_after_the_first_failure() {
        let (logins, start) = (Logins::default(), Instant::now());
        assert_eq!(
            admitted(&logins, "203.0.113.7", 40, start),
            FAILURES_PER_WINDOW
        );
        let almost = start + WINDOW - Duration::from_millis(1);
        assert_eq!(admitted(&logins, "203.0.113.7", 40, almost), 0);
        assert_eq!(
            admitted(&logins, "203.0.113.7", 40, start + WINDOW),
            FAILURES_PER_WINDOW
        );
    }

    #[test]
    fn an_ipv6_network_is_one_client_and_a_mapped_address_is_its_ipv4_address() {
        let (logins, now) = (Logins::default(), Instant::now());
        assert_eq!(admitted(&logins, "2001:db8:1:2::1", 20, now), 20);
        assert_eq!(admitted(&logins, "2001:db8:1:2:ffff::9", 20, now), 10);
        assert_eq!(
            admitted(&logins, "2001:db8:1:3::1", 40, now),
            FAILURES_PER_WINDOW
        );
        assert_eq!(admitted(&logins, "::ffff:203.0.113.7", 20, now), 20);
        assert_eq!(admitted(&logins, "203.0.113.7", 20, now), 10);
    }

    #[test]
    fn the_table_is_bounded_and_addresses_beyond_it_share_one_allowance() {
        let (logins, start) = (Logins::default(), Instant::now());
        for host in 0..MAX_ADDRESSES as u32 {
            assert!(logins.attempt(IpAddr::V4(Ipv4Addr::from(0x0a00_0000 + host)), start));
        }
        // No room: the next 10000 addresses are counted together.
        let beyond = (0..10_000u32)
            .filter(|host| logins.attempt(IpAddr::V4(Ipv4Addr::from(0x0b00_0000 + host)), start))
            .count() as u32;
        assert_eq!(beyond, FAILURES_PER_WINDOW);
        assert_eq!(logins.clients.lock().unwrap().len(), MAX_ADDRESSES + 1);
        // An address with its own entry keeps its own allowance.
        assert_eq!(
            admitted(&logins, "10.0.0.1", 40, start),
            FAILURES_PER_WINDOW - 1
        );
        // Entries a minute old make room again.
        let later = start + WINDOW;
        assert_eq!(
            admitted(&logins, "11.0.0.1", 40, later),
            FAILURES_PER_WINDOW
        );
        assert!(logins.clients.lock().unwrap().len() <= 2);
    }
}
