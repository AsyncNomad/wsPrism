//! Where Redis is, and the connections to it (0.2): one fixed address, or
//! the primary that Redis Sentinel names, followed across failovers.
//!
//! The command and publish connections live here and are replaced when the
//! primary moves; the pub/sub tasks ask for a fresh client on every
//! (re)connect and reconnect when told the primary moved: a demoted primary
//! keeps their subscriptions open but no longer receives the publishes.
//! What a failover means for the data (a promoted replica may miss the last
//! writes) is handled where the data is: a new `run_id` starts new history
//! epochs and makes the nodes register again (ADR 0011 pre-release review).
//! The one write that nothing repairs, a consumed one-time ticket, waits
//! for replicas on its own connection (`acked`; `WAIT` blocks the
//! connection it runs on). The slot heartbeat and the load probe have
//! their own too (`own`): they must not queue behind a burst of session
//! commands, or a busy node would lose its slot (review #3).

use std::sync::{Arc, RwLock};
use std::time::Duration;

use redis::aio::ConnectionManager;
use redis::sentinel::SentinelClient;
use tokio::sync::watch;

/// How often Sentinel is asked which node is the primary.
const FOLLOW_EVERY: Duration = Duration::from_secs(1);
/// Longest a (re)connection attempt waits: a primary that died without a
/// reset (its address gone) would otherwise hold requests for long.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) enum Locator {
    Fixed(redis::Client),
    Sentinel(tokio::sync::Mutex<SentinelClient>),
}

/// See the module docs.
pub(crate) struct Link {
    locator: Locator,
    publish_timeout: Duration,
    /// Scripts the pipelined publishes call by hash: loaded on every
    /// primary right away (a new primary has none).
    scripts: &'static [&'static str],
    conn: RwLock<Option<ConnectionManager>>,
    publish: RwLock<Option<ConnectionManager>>,
    /// For writes followed by `WAIT` (only when replica acks are asked for).
    acked: RwLock<Option<ConnectionManager>>,
    /// Heartbeat and load probe.
    own: RwLock<Option<ConnectionManager>>,
    /// The longest `WAIT` asked for: the `acked` connection's response
    /// timeout leaves room for it. `None` = no `acked` connection.
    acked_wait: Option<Duration>,
    /// The primary the connections point at (Sentinel mode).
    primary: RwLock<Option<String>>,
    moved: watch::Sender<u64>,
}

fn read<T: Clone>(l: &RwLock<Option<T>>) -> Option<T> {
    l.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn write<T>(l: &RwLock<Option<T>>, v: T) {
    *l.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(v);
}

impl Link {
    pub(crate) fn new(
        locator: Locator,
        publish_timeout: Duration,
        scripts: &'static [&'static str],
        acked_wait: Option<Duration>,
    ) -> Self {
        Self {
            locator,
            publish_timeout,
            scripts,
            conn: RwLock::new(None),
            publish: RwLock::new(None),
            acked: RwLock::new(None),
            own: RwLock::new(None),
            acked_wait,
            primary: RwLock::new(None),
            moved: watch::channel(0).0,
        }
    }

    pub(crate) fn follows_sentinel(&self) -> bool {
        matches!(self.locator, Locator::Sentinel(_))
    }

    /// A client for the current primary.
    pub(crate) async fn client(&self) -> redis::RedisResult<redis::Client> {
        match &self.locator {
            Locator::Fixed(c) => Ok(c.clone()),
            Locator::Sentinel(s) => s.lock().await.async_get_client().await,
        }
    }

    /// (Re)build the command and publish connections to the current
    /// primary.
    pub(crate) async fn connect(&self) -> redis::RedisResult<()> {
        let client = self.client().await?;
        let mut conn = client
            .get_connection_manager_with_config(
                redis::aio::ConnectionManagerConfig::new()
                    .set_connection_timeout(Some(CONNECT_TIMEOUT)),
            )
            .await?;
        for script in self.scripts {
            let _: String = redis::cmd("SCRIPT")
                .arg("LOAD")
                .arg(*script)
                .query_async(&mut conn)
                .await?;
        }
        let publish = client
            .get_connection_manager_with_config(
                redis::aio::ConnectionManagerConfig::new()
                    .set_response_timeout(Some(self.publish_timeout))
                    .set_connection_timeout(Some(CONNECT_TIMEOUT)),
            )
            .await?;
        if let Some(wait) = self.acked_wait {
            let acked = client
                .get_connection_manager_with_config(
                    redis::aio::ConnectionManagerConfig::new()
                        .set_response_timeout(Some(wait + CONNECT_TIMEOUT))
                        .set_connection_timeout(Some(CONNECT_TIMEOUT)),
                )
                .await?;
            write(&self.acked, acked);
        }
        let own = client
            .get_connection_manager_with_config(
                redis::aio::ConnectionManagerConfig::new()
                    .set_response_timeout(Some(CONNECT_TIMEOUT))
                    .set_connection_timeout(Some(CONNECT_TIMEOUT)),
            )
            .await?;
        write(&self.own, own);
        write(&self.conn, conn);
        write(&self.publish, publish);
        write(&self.primary, address(&client));
        Ok(())
    }

    pub(crate) fn conn(&self) -> Option<ConnectionManager> {
        read(&self.conn)
    }

    pub(crate) fn publish(&self) -> Option<ConnectionManager> {
        read(&self.publish)
    }

    /// The heartbeat's and load probe's connection (see `own`).
    pub(crate) fn own(&self) -> Option<ConnectionManager> {
        read(&self.own)
    }

    /// The connection for writes followed by `WAIT` (see `acked`).
    pub(crate) fn acked(&self) -> Option<ConnectionManager> {
        read(&self.acked)
    }

    /// Changes when the primary moved (pub/sub tasks reconnect).
    pub(crate) fn moved(&self) -> watch::Receiver<u64> {
        self.moved.subscribe()
    }

    /// Sentinel mode: ask which node is the primary, and move everything
    /// there when it changed. Runs until aborted.
    pub(crate) async fn follow(self: Arc<Self>) {
        loop {
            tokio::time::sleep(FOLLOW_EVERY).await;
            let now = match self.client().await {
                Ok(c) => address(&c),
                Err(e) => {
                    tracing::warn!("redis broker: sentinel did not name a primary: {e}");
                    continue;
                }
            };
            if read(&self.primary).as_deref() == Some(now.as_str()) {
                continue;
            }
            tracing::warn!(primary = %now, "redis broker: the primary moved; reconnecting");
            match self.connect().await {
                Ok(()) => {
                    self.moved.send_modify(|n| *n += 1);
                }
                Err(e) => tracing::warn!("redis broker: connecting to the new primary failed: {e}"),
            }
        }
    }
}

fn address(client: &redis::Client) -> String {
    format!("{:?}", client.get_connection_info().addr())
}
