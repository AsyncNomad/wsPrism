//! Sec-WebSocket-Protocol parsing: version negotiation + ticket carrier.
#![no_main]

use axum::http::{header::SEC_WEBSOCKET_PROTOCOL, HeaderMap, HeaderValue};
use libfuzzer_sys::fuzz_target;
use wsprism_gateway::__internal::{Negotiated, Offered, V1};

fuzz_target!(|data: &[u8]| {
    let Ok(value) = HeaderValue::from_bytes(data) else {
        return;
    };
    let mut headers = HeaderMap::new();
    headers.insert(SEC_WEBSOCKET_PROTOCOL, value);
    let offered = Offered::parse(&headers);

    // Only a supported version may ever be selected (never the auth entry).
    if let Negotiated::Selected(v) = offered.negotiate() {
        assert_eq!(v, V1);
    }
    if let Some(token) = offered.auth_token() {
        assert!(!token.is_empty());
    }
});
