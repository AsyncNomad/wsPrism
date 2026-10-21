//! One QUIC endpoint + WebTransport + MoQ session per participant.

use anyhow::Context;
use moq_native_ietf::{quic, tls};
use moq_transport::session::{Publisher, Session, Subscriber};

pub struct Conn {
    /// Kept to close the connection explicitly (graceful leave).
    pub wt: web_transport::Session,
    pub session: Session,
    pub publisher: Publisher,
    pub subscriber: Subscriber,
}

pub async fn open(url: &url::Url) -> anyhow::Result<Conn> {
    let tls = tls::Args {
        disable_verify: true,
        ..Default::default()
    }
    .load()?;
    let endpoint = quic::Endpoint::new(quic::Config::new("0.0.0.0:0".parse()?, None, tls)?)?;
    let (wt, _cid, transport) = endpoint.client.connect(url, None).await?;
    // The endpoint must outlive the connection.
    std::mem::forget(endpoint);
    let (session, publisher, subscriber) = Session::connect(wt.clone(), None, transport)
        .await
        .context("MoQ session")?;
    Ok(Conn { wt, session, publisher, subscriber })
}
