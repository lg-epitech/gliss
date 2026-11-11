//! Fuzzes the record of held keys and buttons with any sequence of inputs,
//! next to a sorted set that plays the same part.

use std::collections::BTreeSet;

use gliss_core::Input;

use super::{Held, MAX_HELD_KEYS};
use crate::fuzz::support::{self, Rng};

fn input(rng: &mut Rng, keysyms: u32) -> Input {
    let id = rng.next() as u32;
    match rng.below(10) {
        0..7 => Input::Key {
            // A small range, so that keys repeat and are released, and now
            // and then any keysym a client can name.
            keysym: if rng.chance(20) {
                rng.next() as u32
            } else {
                rng.below(keysyms as usize) as u32
            },
            down: !rng.chance(3),
            id,
        },
        7 | 8 => Input::Pointer {
            x: rng.next() as u16,
            y: rng.next() as u16,
            buttons: if rng.chance(2) { 0 } else { rng.byte() },
            id,
        },
        9 if rng.chance(2) => Input::Clipboard {
            text: "text".into(),
            id,
        },
        _ => Input::Resize {
            width: rng.next() as u16,
            height: rng.next() as u16,
            id,
        },
    }
}

const DEFAULT_ITERATIONS: u64 = 60;

#[test]
fn fuzz_held_keys() {
    support::run("server-held-keys", DEFAULT_ITERATIONS, |rng, _| {
        let mut held = Held::default();
        let mut keys = BTreeSet::new();
        let mut pointer = None;
        // Few enough keysyms to stay under the limit, or enough to reach it.
        let keysyms = rng.pick(&[4, 200, 300, 2000]);
        for _ in 0..rng.between(1, 1500) {
            let input = input(rng, keysyms);
            // Only one key more than the limit is refused.
            let admitted = match &input {
                Input::Key {
                    keysym, down: true, ..
                } => keys.len() < MAX_HELD_KEYS || keys.contains(keysym),
                _ => true,
            };
            assert_eq!(held.admits(&input), admitted);
            // The session writes an admitted input to the VNC server and
            // records it if the write went through.
            if admitted && !rng.chance(10) {
                held.written(&input);
                match input {
                    Input::Key {
                        keysym, down: true, ..
                    } => {
                        keys.insert(keysym);
                    }
                    Input::Key { keysym, .. } => {
                        keys.remove(&keysym);
                    }
                    Input::Pointer { x, y, buttons, .. } => pointer = Some((x, y, buttons)),
                    Input::Clipboard { .. } | Input::Resize { .. } => {}
                }
            }
            assert!(keys.len() <= MAX_HELD_KEYS);
            if rng.chance(50) {
                // Everything held is released once: each key, then the buttons
                // where the pointer was left.
                let mut released = BTreeSet::new();
                let mut button_release = None;
                for release in held.releases() {
                    assert!(button_release.is_none(), "a release after the buttons");
                    match release {
                        Input::Key {
                            keysym,
                            down: false,
                            id: 0,
                        } => assert!(released.insert(keysym), "a key released twice"),
                        Input::Pointer {
                            x,
                            y,
                            buttons: 0,
                            id: 0,
                        } => button_release = Some((x, y)),
                        other => panic!("{other:?} releases nothing"),
                    }
                }
                assert_eq!(released, keys);
                assert_eq!(
                    button_release,
                    pointer
                        .filter(|(_, _, buttons)| *buttons != 0)
                        .map(|(x, y, _)| (x, y))
                );
            }
        }
    });
}
