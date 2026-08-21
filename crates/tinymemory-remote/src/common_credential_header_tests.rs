//! Tests for the surrounding module.

#![allow(clippy::expect_used, clippy::panic)]

use super::{credential_header, Auth, HttpClient};

/// The point of the helper. `reqwest` only redacts a header value whose
/// sensitive flag is set, and `RequestBuilder::header` handed a plain
/// string leaves it clear -- which is how an API key ends up rendered in
/// full by anything that formats the request.
#[test]
fn a_credential_header_is_marked_sensitive() {
    let header = credential_header("Token m0-secret").expect("a plain key is a valid header");
    assert!(header.is_sensitive());
}

/// The value still has to be the credential; marking it sensitive must not
/// change what goes on the wire.
#[test]
fn marking_it_sensitive_does_not_change_the_value() {
    let header = credential_header("Token m0-secret").expect("valid");
    assert_eq!(header.as_bytes(), b"Token m0-secret");
}

/// A credential carrying a newline cannot be a header. Rejecting it here
/// names the credential; letting it through defers the failure into `send`,
/// where it reads as a transport fault.
#[test]
fn a_credential_that_cannot_be_a_header_is_refused_by_name() {
    let error = credential_header("key\r\nX-Injected: 1").expect_err("must not be accepted");
    assert!(format!("{error}").contains("credential"), "got: {error}");
}

/// And the refusal must not print the credential it refused.
#[test]
fn the_refusal_does_not_echo_the_credential() {
    let error = credential_header("supersecret\nX-Injected: 1").expect_err("must not be accepted");
    let rendered = format!("{error:?}");
    assert!(!rendered.contains("supersecret"), "leaked: {rendered}");
}

/// Both credential-bearing schemes go through the helper, so both reach
/// the wire redacted. `Auth::Bearer` is covered by `reqwest`'s own
/// `bearer_auth`, which sets the flag itself.
#[test]
fn both_manual_schemes_send_a_sensitive_authorization_value() {
    for auth in [
        Auth::ApiKey("cg-secret".into()),
        Auth::Token("m0-secret".into()),
    ] {
        let client = HttpClient::new("https://example.test", auth).expect("valid endpoint");
        let request = client
            .request(reqwest::Method::GET, "v1/thing")
            .expect("a plain key builds")
            .build()
            .expect("request builds");
        let sensitive = request
            .headers()
            .values()
            .any(reqwest::header::HeaderValue::is_sensitive);
        assert!(sensitive, "no sensitive header on {:?}", request.headers());
    }
}
