//! Fuzzes the session cookie check with every kind of Cookie header: the
//! issued cookie among others, the same cookie with a byte changed, added or
//! removed, cookies signed by hand with any expiry, nonce and key, and bytes
//! that are no cookie at all.

use axum::http::{HeaderMap, HeaderValue, header};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use super::support::{self, Rng};
use crate::{App, authorized_until, now, signed_cookie, test_support};

/// What the check makes of a Cookie header, or `None` for bytes no HTTP
/// request can carry in a header.
fn checked(app: &App, cookie: &[u8]) -> Option<Option<u64>> {
    let mut headers = HeaderMap::new();
    headers.insert(header::COOKIE, HeaderValue::from_bytes(cookie).ok()?);
    Some(authorized_until(app, &headers))
}

/// The value of the first `gliss_session` cookie in a header, read the way
/// RFC 6265 describes: pairs between semicolons, spaces around them ignored.
/// A header with bytes outside printable ASCII names no session.
fn session_cookie(header: &[u8]) -> Option<&[u8]> {
    if !header
        .iter()
        .all(|byte| matches!(byte, b'\t' | b' '..=b'~'))
    {
        return None;
    }
    header
        .split(|byte| *byte == b';')
        .find_map(|pair| pair.trim_ascii().strip_prefix(b"gliss_session=".as_slice()))
}

fn signature(key: &[u8], payload: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Other cookies and spaces around `cookie`, none of which change it.
fn among_others(rng: &mut Rng, cookie: &str) -> String {
    let others = [
        "theme=dark",
        "a=b",
        "gliss_sessions=1",
        "x=gliss_session=1",
        "=",
        "",
    ];
    let mut pairs: Vec<String> = (0..rng.below(4))
        .map(|_| rng.pick(&others).to_owned())
        .collect();
    let spaces = |rng: &mut Rng| " ".repeat(rng.below(3));
    let at = rng.below(pairs.len() + 1);
    pairs.insert(
        at,
        format!("{}gliss_session={cookie}{}", spaces(rng), spaces(rng)),
    );
    pairs.join(rng.pick(&[";", "; ", " ; "]))
}

/// A printable character that is no separator of the Cookie header.
fn character(rng: &mut Rng) -> u8 {
    loop {
        let byte = rng.between(0x21, 0x7e) as u8;
        if byte != b';' {
            return byte;
        }
    }
}

const SESSION_TTL: u64 = 3600;
const DEFAULT_ITERATIONS: u64 = 300;

#[test]
fn fuzz_session_cookie() {
    let app = test_support::app(&["--demo"]);
    assert_eq!(app.args.session_ttl, SESSION_TTL);
    let other_key = [7; 32];
    support::run("server-session-cookie", DEFAULT_ITERATIONS, |rng, _| {
        let issued = signed_cookie(&app);
        let current = now();
        // The issued cookie is accepted, alone and among other cookies.
        let header = among_others(rng, &issued);
        let until = checked(&app, header.as_bytes())
            .unwrap()
            .unwrap_or_else(|| panic!("{header:?} was refused"));
        assert!(until > current && until <= current + SESSION_TTL + 1);
        assert_eq!(issued.split('.').next().unwrap(), until.to_string());

        // One byte changed, added or removed anywhere in it is refused. The
        // rest of the header is as before, so the cookie is still the first.
        let mut changed = issued.clone().into_bytes();
        let at = rng.below(changed.len());
        match rng.below(4) {
            0 => changed[at] ^= 1 << rng.below(7),
            1 => changed[at] = character(rng),
            2 => changed.insert(at, character(rng)),
            _ => {
                changed.remove(at);
            }
        }
        if changed != issued.as_bytes() {
            let header = [b"gliss_session=".as_slice(), &changed].concat();
            if let Some(result) = checked(&app, &header) {
                assert_eq!(
                    result,
                    None,
                    "{:?} was accepted in place of {issued:?}",
                    String::from_utf8_lossy(&changed)
                );
            }
        }

        // A cookie signed by hand is accepted only with the server's key, an
        // expiry within the session lifetime from now, and a nonce of the
        // issued length. The times keep seconds away from each limit, since
        // the clock moves between here and the check.
        let (expiry, live) = match rng.below(8) {
            0 => (
                current - rng.below(100_000).min(current as usize) as u64,
                false,
            ),
            1 => (0, false),
            2 => (current + SESSION_TTL + 5 + rng.below(100_000) as u64, false),
            3 => (u64::MAX, false),
            _ => (
                current + rng.between(5, SESSION_TTL as usize - 5) as u64,
                true,
            ),
        };
        let length = rng.pick(&[32, 32, 32, 32, 0, 1, 31, 33, 64]);
        let nonce: String = (0..length)
            .map(|_| char::from(rng.pick(b"0123456789abcdef.-_")))
            .collect();
        let payload = format!("{expiry}.{nonce}");
        let ours = !rng.chance(4);
        let key = if ours { &app.cookie_key } else { &other_key };
        let forged = format!("{payload}.{}", signature(key, &payload));
        let header = among_others(rng, &forged);
        assert_eq!(
            checked(&app, header.as_bytes()).unwrap(),
            (ours && live && length == 32).then_some(expiry),
            "{header:?}"
        );

        // Anything else: whatever bytes a header can carry are refused
        // unless the issued cookie is still what they name, and none panic.
        let length = rng.below(200);
        let mut header = match rng.below(4) {
            0 => rng.bytes(length),
            1 => [b"gliss_session=".as_slice(), &rng.bytes(length)].concat(),
            2 => {
                let digits = rng.pick(&[100, 129, 4096, 70_000]);
                format!("gliss_session={}", "9".repeat(digits)).into_bytes()
            }
            _ => among_others(rng, &issued).into_bytes(),
        };
        for _ in 0..rng.below(4) {
            support::mutate(rng, &mut header, &[0, 14, 25, 58], true);
        }
        if let Some(result) = checked(&app, &header) {
            let named = session_cookie(&header) == Some(issued.as_bytes());
            assert_eq!(
                result.is_some(),
                named,
                "{:?}",
                String::from_utf8_lossy(&header)
            );
        }
    });
}

/// Found by `fuzz_session_cookie`. The signature was decoded from hex before
/// it was compared, so each of its letters was accepted in either case and
/// one session had thousands of valid cookies.
#[test]
fn a_signature_spelled_in_upper_case_is_refused() {
    let app = test_support::app(&["--demo"]);
    let (issued, respelled) = loop {
        let issued = signed_cookie(&app);
        let (payload, signature) = issued.rsplit_once('.').unwrap();
        let respelled = format!("{payload}.{}", signature.to_uppercase());
        // All but one signature in ten trillion has a letter in it.
        if respelled != issued {
            break (issued, respelled);
        }
    };
    let header = |cookie: &str| format!("gliss_session={cookie}").into_bytes();
    assert!(checked(&app, &header(&issued)).unwrap().is_some());
    assert_eq!(checked(&app, &header(&respelled)), Some(None));
}
