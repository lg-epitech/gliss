//! Cases a fuzzer of the session cookie check found.

use axum::http::{HeaderMap, HeaderValue, header};

use crate::{App, authorized_until, signed_cookie, test_support};

/// What the check makes of a Cookie header, or `None` for bytes no HTTP
/// request can carry in a header.
fn checked(app: &App, cookie: &[u8]) -> Option<Option<u64>> {
    let mut headers = HeaderMap::new();
    headers.insert(header::COOKIE, HeaderValue::from_bytes(cookie).ok()?);
    Some(authorized_until(app, &headers))
}

/// The signature was decoded from hex before
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
