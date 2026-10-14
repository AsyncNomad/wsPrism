//! X-Forwarded-For resolution behind trusted proxies (client-controlled header).
#![no_main]

use std::net::IpAddr;

use axum::http::{HeaderMap, HeaderValue};
use libfuzzer_sys::fuzz_target;
use wsprism_gateway::transport::client_ip::TrustedProxies;

fuzz_target!(|data: &[u8]| {
    let Ok(value) = HeaderValue::from_bytes(data) else {
        return;
    };
    let mut headers = HeaderMap::new();
    headers.insert("x-forwarded-for", value);
    let proxies = TrustedProxies::parse(&["10.0.0.0/8".to_string(), "::1".to_string()])
        .expect("static config");
    let peer: IpAddr = "10.0.0.1".parse().expect("static ip");

    let client = proxies.client_ip(peer, &headers);

    // The result is either the peer or an address literally present in the header.
    let text = String::from_utf8_lossy(data);
    let listed = text.split(',').filter_map(|h| h.trim().parse::<IpAddr>().ok()).any(|ip| {
        ip == client
            || matches!(ip, IpAddr::V6(v6) if v6.to_ipv4_mapped().map(IpAddr::V4) == Some(client))
    });
    assert!(client == peer || listed, "client {client} not derived from {text:?}");
});
