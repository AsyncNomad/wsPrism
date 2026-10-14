//! Origin allowlist patterns (operator config) and Origin header matching.
#![no_main]

use libfuzzer_sys::fuzz_target;
use wsprism_gateway::policy::origin::OriginPolicy;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    // First line: allowlist pattern; rest: the request's Origin header.
    let (pattern, origin) = text.split_once('\n').unwrap_or((&text, ""));
    if let Ok(policy) = OriginPolicy::compile(&[pattern.to_string()]) {
        let allowed = policy.allows(Some(origin));
        // Requests without Origin (non-browser clients) are always allowed.
        assert!(policy.allows(None));
        // A non-wildcard pattern only ever matches its own origin (case-insensitively).
        if !pattern.contains('*') && allowed {
            assert_eq!(pattern.trim().to_ascii_lowercase(), origin.trim().to_ascii_lowercase());
        }
    }
});
