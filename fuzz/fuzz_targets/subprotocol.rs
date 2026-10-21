//! Sec-WebSocket-Protocol parsing: version negotiation + ticket carrier.
#![no_main]

use axum::http::{header::SEC_WEBSOCKET_PROTOCOL, HeaderMap, HeaderValue};
use libfuzzer_sys::fuzz_target;
use wsprism_gateway::__internal::{Negotiated, Offered, V1, V2_JSON};

fuzz_target!(|data: &[u8]| {
    let Ok(value) = HeaderValue::from_bytes(data) else {
        return;
    };
    let mut headers = HeaderMap::new();
    headers.insert(SEC_WEBSOCKET_PROTOCOL, value);
    let offered = Offered::parse(&headers);

    // Only a supported version that the client offered may be selected
    // (never the auth entry). (This asserted v1 only until v2 existed: the
    // first CI fuzz run offered `wsprism.v2.json` and tripped it.)
    if let Negotiated::Selected(v) = offered.negotiate() {
        assert!(v == V2_JSON || v == V1, "{v}");
        let text = std::str::from_utf8(data).expect("a selected version comes from text");
        assert!(text.split(',').any(|e| e.trim() == v), "{v} was not offered");
    }
    if let Some(token) = offered.auth_token() {
        assert!(!token.is_empty());
    }
});
