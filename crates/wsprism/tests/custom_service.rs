//! What an external user does, using only the `wsprism` facade: build a
//! gateway with a custom service and authenticator, serve it, talk to it.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use wsprism::prelude::*;
use wsprism::serde_json::{json, Value};

/// `shout.say` -> broadcasts the text upper-cased to the room.
struct Shout;

#[async_trait]
impl TextService for Shout {
    fn svc(&self) -> &'static str {
        "shout"
    }
    async fn handle(&self, ctx: &mut RealtimeCtx, env: Envelope) -> Result<()> {
        let room = env
            .room
            .ok_or_else(|| WsPrismError::BadRequest("room".into()))?;
        let text: Value = env
            .data
            .map(|d| wsprism::serde_json::from_str(d.get()).unwrap_or(Value::Null))
            .unwrap_or(Value::Null);
        let loud = text.as_str().unwrap_or_default().to_uppercase();
        ctx.publish_room_reliable(
            &room,
            Outgoing {
                qos: QoS::Reliable,
                payload: Payload::TextJson(
                    json!({"v":1,"svc":"shout","type":"heard","room":room,"data":loud}),
                ),
            },
        )
    }
}

/// Tokens `key-<user>` only.
struct KeyAuth;

#[async_trait]
impl Authenticator for KeyAuth {
    fn kind(&self) -> &'static str {
        "key"
    }
    async fn authenticate(
        &self,
        req: &AuthRequest<'_>,
    ) -> std::result::Result<Identity, AuthError> {
        req.token
            .strip_prefix("key-")
            .map(Identity::user)
            .ok_or(AuthError::NotMine)
    }
}

const YAML: &str = r#"
version: 1
tenants:
  - id: acme
    auth: [{ kind: plugin, name: key }]
    policy:
      ext_allowlist: ["room:*", "shout:*"]
"#;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn next_json(ws: &mut Ws) -> Value {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(3), ws.next())
            .await
            .expect("timely message")
            .expect("open stream")
            .expect("valid frame");
        if let Message::Text(t) = msg {
            return wsprism::serde_json::from_str(&t).expect("json");
        }
    }
}

#[tokio::test]
async fn a_custom_service_and_authenticator_through_the_facade() {
    let gateway = Gateway::builder()
        .config(wsprism::config::load_from_str(YAML).expect("config"))
        .text_service(Shout)
        .auth_plugin("key", |_| Ok(Arc::new(KeyAuth)))
        .build()
        .expect("build");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(
        gateway
            .clone()
            .serve_on(listener, None, std::future::pending()),
    );

    let mut req = format!("ws://{addr}/v1/ws?tenant=acme")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("authorization", "Bearer key-zoe".parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .expect("connect");
    assert_eq!(next_json(&mut ws).await["data"]["user"], "zoe");

    ws.send(Message::Text(
        json!({"v":1,"svc":"room","type":"join","room":"hall"}).to_string(),
    ))
    .await
    .unwrap();
    assert_eq!(next_json(&mut ws).await["type"], "joined");
    ws.send(Message::Text(
        json!({"v":1,"svc":"shout","type":"say","room":"hall","data":"hello"}).to_string(),
    ))
    .await
    .unwrap();
    let heard = next_json(&mut ws).await;
    assert_eq!(heard["data"], "HELLO", "{heard}");
    assert_eq!(gateway.session_count(), 1);
}
