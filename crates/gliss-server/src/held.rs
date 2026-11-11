//! Keys and pointer buttons a session holds down on the VNC server.
//!
//! The session releases them when it ends, so the guest is not left with a
//! stuck key. That needs a record of what is down, and the browser decides
//! what goes into it.

use gliss_core::Input;
use std::collections::HashSet;

/// Most keys held at once. A keyboard has about a hundred keys and reports
/// every one of them at most; this is more than two keyboards pressed whole.
/// Without a bound, a client pressing keys it never releases grows the record
/// by one entry per message for as long as its session lasts.
pub const MAX_HELD_KEYS: usize = 256;

pub const TOO_MANY_KEYS: &str = "Too many keys are held down. Release one first.";

#[derive(Default)]
pub struct Held {
    keys: HashSet<u32>,
    pointer: Option<(u16, u16, u8)>,
}

impl Held {
    /// Whether `input` may be sent on. Only pressing one key more than
    /// [`MAX_HELD_KEYS`] is refused; a held key may repeat.
    pub fn admits(&self, input: &Input) -> bool {
        match input {
            Input::Key {
                keysym, down: true, ..
            } => self.keys.len() < MAX_HELD_KEYS || self.keys.contains(keysym),
            _ => true,
        }
    }

    /// Record an input the VNC server was sent. An input whose write failed
    /// is not recorded: the server never saw the key go down, or up.
    pub fn written(&mut self, input: &Input) {
        match input {
            Input::Key { keysym, down, .. } => {
                if *down {
                    self.keys.insert(*keysym);
                } else {
                    self.keys.remove(keysym);
                }
            }
            Input::Pointer { x, y, buttons, .. } => self.pointer = Some((*x, *y, *buttons)),
            Input::Clipboard { .. } | Input::Resize { .. } => {}
        }
    }

    /// The inputs that release everything still held.
    pub fn releases(&self) -> Vec<Input> {
        let keys = self.keys.iter().map(|keysym| Input::Key {
            keysym: *keysym,
            down: false,
            id: 0,
        });
        let buttons = self
            .pointer
            .filter(|(_, _, buttons)| *buttons != 0)
            .map(|(x, y, _)| Input::Pointer {
                x,
                y,
                buttons: 0,
                id: 0,
            });
        keys.chain(buttons).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(keysym: u32, down: bool) -> Input {
        Input::Key {
            keysym,
            down,
            id: 0,
        }
    }

    #[test]
    fn the_record_never_holds_more_than_the_limit() {
        let mut held = Held::default();
        // Every keysym a client can name, pressed and never released.
        for keysym in 0..100_000 {
            let press = key(keysym, true);
            assert_eq!(held.admits(&press), keysym < MAX_HELD_KEYS as u32);
            if held.admits(&press) {
                held.written(&press);
            }
        }
        assert_eq!(held.keys.len(), MAX_HELD_KEYS);
        assert_eq!(held.releases().len(), MAX_HELD_KEYS);
        // A held key repeats, a release is always sent, and it makes room.
        assert!(held.admits(&key(7, true)));
        assert!(!held.admits(&key(70_000, true)));
        assert!(held.admits(&key(70_000, false)));
        held.written(&key(7, false));
        assert!(held.admits(&key(70_000, true)));
    }

    #[test]
    fn only_written_inputs_are_recorded() {
        let mut held = Held::default();
        // Admitted, but the write failed: nothing to release.
        assert!(held.admits(&key(65, true)));
        assert!(held.releases().is_empty());
        held.written(&key(65, true));
        // A release that could not be written leaves the key recorded.
        assert!(held.admits(&key(65, false)));
        assert_eq!(held.releases().len(), 1);
        held.written(&key(65, false));
        assert!(held.releases().is_empty());
    }

    #[test]
    fn held_buttons_are_released_where_the_pointer_was() {
        let mut held = Held::default();
        let pointer = |buttons| Input::Pointer {
            x: 3,
            y: 4,
            buttons,
            id: 9,
        };
        held.written(&pointer(0));
        assert!(held.releases().is_empty());
        held.written(&pointer(5));
        held.written(&key(65, true));
        let releases = held.releases();
        assert_eq!(releases.len(), 2);
        assert!(matches!(
            releases[0],
            Input::Key {
                keysym: 65,
                down: false,
                id: 0
            }
        ));
        assert!(matches!(
            releases[1],
            Input::Pointer {
                x: 3,
                y: 4,
                buttons: 0,
                id: 0
            }
        ));
    }
}
