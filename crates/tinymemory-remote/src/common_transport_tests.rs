//! Tests for the surrounding module.

#![allow(clippy::expect_used, clippy::panic)]

use super::{classify_transport, TransportClass};

/// The verbatim chain a rustls handshake abort produces. Cognee's hosted
/// endpoint answered TCP and then sent this; `reqwest` reports it as a
/// CONNECT error, so an `is_connect` check placed first swallows it — and
/// the string never contains the word "TLS", so matching on that alone
/// misses it too. Both traps, pinned.
#[test]
fn a_rustls_handshake_abort_is_named_tls_not_connect() {
    let class = classify_transport(
        false,
        true, // reqwest really does set is_connect for this
        "client error (Connect): received fatal alert: InternalError",
    );
    assert_eq!(class, TransportClass::Tls);
    assert!(class.describe().starts_with("TLS failed"));
}

/// DNS failures are also CONNECT errors; the specific class must win.
#[test]
fn a_dns_failure_is_named_dns_not_connect() {
    let class = classify_transport(
        false,
        true,
        "client error (Connect): dns error: failed to lookup address information",
    );
    assert_eq!(class, TransportClass::Dns);
    assert!(class.describe().contains("could not be resolved"));
}

#[test]
fn a_refused_connection_is_the_connect_class() {
    let class = classify_transport(
        false,
        true,
        "client error (Connect): tcp connect error: Connection refused (os error 61)",
    );
    assert_eq!(class, TransportClass::Connect);
    assert!(class.describe().starts_with("could not connect"));
}

/// A timeout outranks everything: it is the one class reqwest states
/// outright rather than leaving to the chain's wording.
#[test]
fn a_timeout_wins_over_every_chain_hint() {
    let class = classify_transport(true, true, "dns error: something tls certificate");
    assert_eq!(class, TransportClass::Timeout);
    assert_eq!(class.describe(), "timed out");
}

#[test]
fn an_unrecognised_chain_degrades_without_claiming_a_cause() {
    let class = classify_transport(false, false, "body error: incomplete message");
    assert_eq!(class, TransportClass::Other);
    assert_eq!(class.describe(), "the request did not complete");
}

/// §A4: the typed payload rides the anyhow error and downcasts back out —
/// the property `engine_error` relies on at the contract boundary.
#[test]
fn typed_variants_survive_the_anyhow_round_trip() {
    use tinymemory_api::error::MemoryError;
    let carried = anyhow::Error::new(MemoryError::Unauthorized("key rejected".into()));
    match carried.downcast::<MemoryError>() {
        Ok(MemoryError::Unauthorized(msg)) => assert_eq!(msg, "key rejected"),
        other => panic!("lost the typed payload: {other:?}"),
    }
}

/// §U6: a threshold means a threshold. An unscored hit does not clear
/// one, an exactly-equal score does, and no-threshold callers see the
/// old behavior untouched (the filter never runs).
#[test]
fn min_score_is_honest_about_unscored_hits() {
    use super::clears_min_score;
    assert!(!clears_min_score(None, 0.1));
    assert!(clears_min_score(Some(0.8), 0.8));
    assert!(!clears_min_score(Some(0.79), 0.8));
}

/// The retry gate keys on the §A4 class, never the prose: transient
/// classes retry, deterministic answers do not.
#[test]
fn retry_gate_is_typed_and_conservative() {
    use super::HttpClient;
    use tinymemory_api::error::MemoryError;
    let transient = [
        MemoryError::Timeout("t".into()),
        MemoryError::Unreachable("u".into()),
        MemoryError::Unavailable("503".into()),
    ];
    for error in transient {
        assert!(HttpClient::retryable(&anyhow::Error::new(error)));
    }
    let settled = [
        MemoryError::Unauthorized("401".into()),
        MemoryError::Invalid("bad".into()),
        MemoryError::NotFound("gone".into()),
        MemoryError::Backend("500".into()),
    ];
    for error in settled {
        assert!(!HttpClient::retryable(&anyhow::Error::new(error)));
    }
    // Opaque errors never retry: without a class, a retry is a guess.
    assert!(!HttpClient::retryable(&anyhow::anyhow!("mystery")));
}
