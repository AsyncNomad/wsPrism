//! S6: with the handshake defender enabled, admitted handshakes must not
//! deadlock the server (DashMap guard held across `len()`).

mod common;

use common::*;

const YAML: &str = r#"
version: 1
gateway:
  handshake_limit:
    enabled: true
    global_rps: 100
    global_burst: 100
    per_ip_rps: 100
    per_ip_burst: 100
tenants:
  - id: t
    policy:
      ext_allowlist: ["room:*"]
"#;

#[tokio::test]
async fn admitted_handshakes_complete_with_defender_enabled() {
    let gw = spawn(YAML).await;
    for i in 0..5 {
        let r = try_connect_url(&ws_url(gw.addr, "t", "dev", None)).await;
        assert!(r.is_ok(), "handshake #{i} failed: {:?}", r.err());
    }
}
