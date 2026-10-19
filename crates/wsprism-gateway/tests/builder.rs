//! `GatewayBuilder` (ADR 0009 §3): user services and authenticator plugins
//! registered without touching the gateway's source, and the rules that
//! keep them from shadowing built-ins.

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use common::*;
use serde_json::json;
use wsprism_engine::{HotService, RealtimeCtx, TextService};
use wsprism_gateway::auth::{AuthError, AuthRequest, Authenticator, Identity};
use wsprism_gateway::{config, Gateway};
use wsprism_protocol::protocol::hot::HotFrame;
use wsprism_protocol::protocol::text::Envelope;
use wsprism_protocol::{Result, WsPrismError};

/// A user Ext service: `greet.hello` -> reply with the caller's name.
struct Greeter;

#[async_trait]
impl TextService for Greeter {
    fn svc(&self) -> &'static str {
        "greet"
    }
    async fn handle(&self, ctx: &mut RealtimeCtx, env: Envelope) -> Result<()> {
        match env.msg_type.as_str() {
            "hello" => {
                ctx.reply(json!({"v":1,"svc":"greet","type":"hi","data":{"to": ctx.user()}}))
            }
            _ => Err(WsPrismError::BadRequest("unknown greet type".into())),
        }
    }
}

struct Named(&'static str);

#[async_trait]
impl TextService for Named {
    fn svc(&self) -> &'static str {
        self.0
    }
    async fn handle(&self, _: &mut RealtimeCtx, _: Envelope) -> Result<()> {
        Ok(())
    }
}

struct HotId(u8);

#[async_trait]
impl HotService for HotId {
    fn svc_id(&self) -> u8 {
        self.0
    }
    async fn handle_hot(&self, _: &mut RealtimeCtx, _: HotFrame) -> Result<()> {
        Ok(())
    }
}

/// A user authenticator: tokens `<prefix><id>` (prefix from the config's
/// `options`) are user `<id>`; anything else is not its format.
struct PrefixAuth {
    prefix: String,
}

#[async_trait]
impl Authenticator for PrefixAuth {
    fn kind(&self) -> &'static str {
        "prefix"
    }
    async fn authenticate(
        &self,
        req: &AuthRequest<'_>,
    ) -> std::result::Result<Identity, AuthError> {
        match req.token.strip_prefix(&self.prefix) {
            Some(id) if !id.is_empty() => Ok(Identity::user(id)),
            Some(_) => Err(AuthError::Rejected("empty id")),
            None => Err(AuthError::NotMine),
        }
    }
}

fn prefix_plugin(options: &serde_json::Value) -> Result<Arc<dyn Authenticator>> {
    let prefix = options["prefix"]
        .as_str()
        .ok_or_else(|| WsPrismError::BadRequest("prefix plugin needs options.prefix".into()))?;
    Ok(Arc::new(PrefixAuth {
        prefix: prefix.into(),
    }))
}

const YAML: &str = r#"
version: 1
tenants:
  - id: t
    auth:
      - kind: plugin
        name: prefix
        options: { prefix: "corp-" }
    policy:
      ext_allowlist: ["greet:*", "room:*"]
"#;

async fn serve(gw: Gateway) -> std::net::SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(gw.serve_on(l, None, std::future::pending()));
    addr
}

#[tokio::test]
async fn user_service_and_auth_plugin_work_end_to_end() {
    let gw = Gateway::builder()
        .config(config::load_from_str(YAML).unwrap())
        .text_service(Greeter)
        .auth_plugin("prefix", prefix_plugin)
        .build()
        .expect("build");
    let addr = serve(gw.clone()).await;

    let (mut ws, _) = try_connect_with(
        &ws_url(addr, "t", None),
        &[("authorization", "Bearer corp-alice")],
    )
    .await
    .expect("plugin admits corp-alice");
    match next_event(&mut ws).await {
        Event::Json(v) => assert_eq!(v["data"]["user"], "alice", "{v}"),
        other => panic!("{other:?}"),
    }
    send_json(&mut ws, json!({"v":1,"svc":"greet","type":"hello","seq":1})).await;
    match next_event(&mut ws).await {
        Event::Json(v) => {
            assert_eq!(v["data"]["to"], "alice", "{v}");
            assert_eq!(v["reply_to"], 1, "{v}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(gw.session_count(), 1);
    assert!(gw.metrics_text().contains("wsprism_ws_sessions_active"));

    // A plugin returning an id outside the user id rule is overruled by the
    // chain (review M3).
    let r = try_connect_with(
        &ws_url(addr, "t", None),
        &[("authorization", "Bearer corp-a::b")],
    )
    .await;
    assert!(matches!(r, Err(e) if e.contains("401")));

    // Not the plugin's format, and no other link: rejected.
    let r = try_connect_with(&ws_url(addr, "t", None), &[("authorization", "Bearer dev")]).await;
    assert!(matches!(r, Err(e) if e.contains("401")));
}

fn build_err(
    f: impl FnOnce(wsprism_gateway::GatewayBuilder) -> wsprism_gateway::GatewayBuilder,
    yaml: &str,
) -> String {
    match f(Gateway::builder().config(config::load_from_str(yaml).unwrap())).build() {
        Ok(_) => panic!("build must fail"),
        Err(e) => e.client_message().to_string(),
    }
}

#[test]
fn builder_refuses_to_shadow_or_guess() {
    const PLAIN: &str = "version: 1\ntenants:\n  - id: t\n";
    assert!(build_err(|b| b.text_service(Named("chat")), PLAIN).contains("chat"));
    assert!(build_err(|b| b.text_service(Named("sys")), PLAIN).contains("sys"));
    assert!(build_err(
        |b| b.text_service(Named("x")).text_service(Named("x")),
        PLAIN
    )
    .contains("twice"));
    assert!(build_err(|b| b.hot_service(HotId(1)), PLAIN).contains("hot service id 1"));
    // A plugin named in the config must be registered.
    assert!(build_err(|b| b, YAML).contains("not registered"));
    // The factory's own validation runs at build time.
    let bad_opts = YAML.replace(r#"options: { prefix: "corp-" }"#, "options: {}");
    assert!(
        build_err(|b| b.auth_plugin("prefix", prefix_plugin), &bad_opts).contains("options.prefix")
    );
    // No config at all.
    assert!(Gateway::builder().build().is_err());
}

#[test]
fn plugin_entries_are_strict_and_builtin_kinds_still_catch_typos() {
    let typo = "version: 1\ntenants:\n  - id: t\n    auth: [{ kind: jtw }]\n";
    assert!(config::load_from_str(typo).is_err(), "unknown kind");
    let extra =
        "version: 1\ntenants:\n  - id: t\n    auth: [{ kind: plugin, name: p, option: {} }]\n";
    assert!(config::load_from_str(extra).is_err(), "misspelled options");
}

#[test]
fn allowlist_must_name_registered_services() {
    let yaml =
        "version: 1\ntenants:\n  - id: t\n    policy: { ext_allowlist: [\"greet:hello\"] }\n";
    let err = match Gateway::builder()
        .config(config::load_from_str(yaml).unwrap())
        .build()
    {
        Ok(_) => panic!("unregistered greet must fail the build"),
        Err(e) => e.client_message().to_string(),
    };
    assert!(
        err.contains("greet") && err.contains("not registered"),
        "{err}"
    );
    Gateway::builder()
        .config(config::load_from_str(yaml).unwrap())
        .text_service(Greeter)
        .build()
        .expect("greet is registered");

    let hot = "version: 1\ntenants:\n  - id: t\n    policy: { hot_allowlist: [\"9:*\"] }\n";
    assert!(Gateway::builder()
        .config(config::load_from_str(hot).unwrap())
        .build()
        .is_err());
}
