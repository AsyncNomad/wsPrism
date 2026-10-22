//! Typed configuration schema for the gateway (`wsprism.yaml`).
//!
//! Unknown fields are rejected to avoid silently ignoring operator intent.

use serde::Deserialize;
use wsprism_protocol::error::{Result, WsPrismError};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct GatewayConfig {
    /// Schema version (must be 1).
    pub version: u32,

    #[serde(default)]
    pub gateway: GatewaySection,

    #[serde(default)]
    pub tenants: Vec<TenantConfig>,
}

impl GatewayConfig {
    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            return Err(WsPrismError::UnsupportedVersion);
        }
        if self.tenants.is_empty() {
            return Err(WsPrismError::BadRequest("tenants must not be empty".into()));
        }

        // Unique tenant ids
        {
            use std::collections::HashSet;
            let mut seen = HashSet::new();
            for t in &self.tenants {
                if t.id.trim().is_empty() {
                    return Err(WsPrismError::BadRequest(
                        "tenant.id must not be empty".into(),
                    ));
                }
                if !crate::ident::is_safe_id(&t.id) {
                    // Ids become parts of internal keys and metric labels.
                    return Err(WsPrismError::BadRequest(format!(
                        "tenant.id must be 1-{} characters of [A-Za-z0-9_-]: {:?}",
                        crate::ident::MAX_ID_LEN,
                        t.id
                    )));
                }
                if t.id.starts_with('_') {
                    // Reserved for internal labels such as `_unknown` (metrics).
                    return Err(WsPrismError::BadRequest(format!(
                        "tenant.id must not start with '_' (reserved): {}",
                        t.id
                    )));
                }
                if !seen.insert(t.id.clone()) {
                    return Err(WsPrismError::BadRequest(format!(
                        "duplicate tenant id: {}",
                        t.id
                    )));
                }
                t.validate()?;
            }
        }

        self.gateway.validate()?;
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct GatewaySection {
    /// Listener address (host:port).
    #[serde(default = "default_listen")]
    pub listen: String,

    /// Server API for backends (ADR 0008 §3). Absent = disabled. Bind it to
    /// loopback or an internal network, never the public interface.
    #[serde(default)]
    pub api: Option<ApiConfig>,

    /// Also serve `/metrics` on the public listener. Off by default: metrics
    /// name tenants and reveal load. They are always served on the internal
    /// `api.listen` listener.
    #[serde(default)]
    pub public_metrics: bool,

    /// How session logs show user ids, client session ids and IP addresses
    /// (personal data): `plain` (default), `hashed` (HMAC-SHA256, 16 hex
    /// characters), or `off` (left out). docs/OPERATIONS.md.
    #[serde(default)]
    pub log_identities: LogIdentitiesMode,

    /// With `log_identities: hashed`: an environment variable holding the
    /// HMAC key (at least 16 bytes). The same key on every node and across
    /// restarts makes one person's hashes equal everywhere. Without it, each
    /// process uses a random key.
    #[serde(default)]
    pub log_identity_key_env: Option<String>,

    /// How room traffic reaches other gateway nodes (ADR 0011). Default:
    /// `memory` (one node).
    #[serde(default)]
    pub broker: BrokerConfig,

    /// WebSocket ping interval in milliseconds.
    #[serde(default = "default_ping_interval_ms")]
    pub ping_interval_ms: u64,

    /// Idle timeout in milliseconds.
    /// Connections with no activity beyond this are closed.
    #[serde(default = "default_idle_timeout_ms")]
    pub idle_timeout_ms: u64,

    /// Outbound writer send timeout in milliseconds.
    ///
    /// If a client is slow and a write stalls longer than this, the session is
    /// closed to protect tail latency.
    #[serde(default = "default_writer_send_timeout_ms")]
    pub writer_send_timeout_ms: u64,

    /// Grace period (ms) after entering draining mode before process exits.
    ///
    /// During draining, readiness becomes 503 and new upgrades are rejected.
    #[serde(default = "default_drain_grace_ms")]
    pub drain_grace_ms: u64,

    /// Capacity of each session's outbound message queue. When it is full,
    /// lossy publishes to the session are dropped and replies to the
    /// session's own requests close it as a slow consumer (C9).
    #[serde(default = "default_session_queue_capacity")]
    pub session_queue_capacity: usize,

    /// Socket options for accepted connections.
    #[serde(default)]
    pub socket: SocketConfig,

    /// Reverse proxies / load balancers (IPs or CIDRs) whose
    /// `X-Forwarded-For` is trusted to name the client. Empty = trust none
    /// (the TCP peer is the client).
    #[serde(default)]
    pub trusted_proxies: Vec<String>,

    // Sprint 5: Handshake Defender Configuration
    #[serde(default)]
    pub handshake_limit: HandshakeConfig,
}

/// Socket options applied to every accepted TCP connection.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct SocketConfig {
    /// Disable Nagle's algorithm (TCP_NODELAY). Keep this on for a realtime
    /// gateway: with Nagle + delayed ACKs small frames can wait tens of ms.
    #[serde(default = "default_true")]
    pub tcp_nodelay: bool,
}

impl Default for SocketConfig {
    fn default() -> Self {
        Self { tcp_nodelay: true }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct HandshakeConfig {
    /// On by default (per-IP and global handshake rate limits). Turn off
    /// only for load tests from a single address.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Global limiter: burst capacity.
    #[serde(default = "default_hs_global_burst")]
    pub global_burst: u32,
    /// Global limiter: refill rate per sec.
    #[serde(default = "default_hs_global_rps")]
    pub global_rps: u32,

    /// Per-IP limiter: burst capacity.
    #[serde(default = "default_hs_ip_burst")]
    pub per_ip_burst: u32,
    /// Per-IP limiter: refill rate per sec.
    #[serde(default = "default_hs_ip_rps")]
    pub per_ip_rps: u32,

    /// Clean up per-ip map opportunistically when it grows too big.
    #[serde(default = "default_hs_max_entries")]
    pub max_ip_entries: usize,

    /// IPv6 clients are limited per prefix of this length (default /64: one
    /// subscriber usually holds a whole /64, so per-address limits are easy
    /// to sidestep). 32..=128.
    #[serde(default = "default_hs_ipv6_prefix")]
    pub ipv6_prefix_len: u8,
}

fn default_hs_ipv6_prefix() -> u8 {
    64
}

impl Default for HandshakeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            global_burst: 200,
            global_rps: 100,
            per_ip_burst: 50,
            per_ip_rps: 10,
            max_ip_entries: 50_000,
            ipv6_prefix_len: default_hs_ipv6_prefix(),
        }
    }
}

// Handshake defaults
fn default_hs_global_burst() -> u32 {
    200
}
fn default_hs_global_rps() -> u32 {
    100
}
fn default_hs_ip_burst() -> u32 {
    50
}
fn default_hs_ip_rps() -> u32 {
    10
}
fn default_hs_max_entries() -> usize {
    50_000
}

/// `gateway.log_identities`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogIdentitiesMode {
    #[default]
    Plain,
    Hashed,
    Off,
}

impl Default for GatewaySection {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            public_metrics: false,
            log_identities: LogIdentitiesMode::default(),
            log_identity_key_env: None,
            broker: BrokerConfig::default(),
            ping_interval_ms: default_ping_interval_ms(),
            idle_timeout_ms: default_idle_timeout_ms(),
            writer_send_timeout_ms: default_writer_send_timeout_ms(),
            drain_grace_ms: default_drain_grace_ms(),
            session_queue_capacity: default_session_queue_capacity(),
            socket: SocketConfig::default(),
            api: None,
            trusted_proxies: Vec::new(),
            handshake_limit: HandshakeConfig::default(),
        }
    }
}

impl GatewaySection {
    pub fn validate(&self) -> Result<()> {
        if let BrokerConfig::Redis(r) = &self.broker {
            r.validate()?;
        }
        if !(32..=128).contains(&self.handshake_limit.ipv6_prefix_len) {
            return Err(WsPrismError::BadRequest(
                "gateway.handshake_limit.ipv6_prefix_len must be 32-128".into(),
            ));
        }
        if !(5000..=120000).contains(&self.ping_interval_ms) {
            return Err(WsPrismError::BadRequest(
                "gateway.ping_interval_ms must be between 5000 and 120000".into(),
            ));
        }
        if !(10000..=600000).contains(&self.idle_timeout_ms) {
            return Err(WsPrismError::BadRequest(
                "gateway.idle_timeout_ms must be between 10000 and 600000".into(),
            ));
        }
        if self.idle_timeout_ms <= self.ping_interval_ms {
            return Err(WsPrismError::BadRequest(
                "gateway.idle_timeout_ms must be greater than ping_interval_ms".into(),
            ));
        }
        if !(50..=60000).contains(&self.writer_send_timeout_ms) {
            return Err(WsPrismError::BadRequest(
                "gateway.writer_send_timeout_ms must be between 50 and 60000".into(),
            ));
        }
        if !(1..=1_048_576).contains(&self.session_queue_capacity) {
            return Err(WsPrismError::BadRequest(
                "gateway.session_queue_capacity must be between 1 and 1048576".into(),
            ));
        }
        if self.drain_grace_ms > 600000 {
            return Err(WsPrismError::BadRequest(
                "gateway.drain_grace_ms must be <= 600000".into(),
            ));
        }
        Ok(())
    }
}

fn default_listen() -> String {
    "0.0.0.0:8080".into()
}
fn default_ping_interval_ms() -> u64 {
    20000
}
fn default_idle_timeout_ms() -> u64 {
    60000
}
fn default_writer_send_timeout_ms() -> u64 {
    1500
}
fn default_session_queue_capacity() -> usize {
    1024
}
fn default_drain_grace_ms() -> u64 {
    2000
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct TenantConfig {
    /// Tenant identifier (namespaces policy and runtime state).
    pub id: String,

    #[serde(default)]
    pub limits: TenantLimits,

    /// Sprint 2+: policy controls (strict by default).
    #[serde(default)]
    pub policy: TenantPolicy,

    /// Authenticators tried in order (ADR 0008 §1). Empty = the development
    /// stub (`dev` / `dev:<id>`), with a startup warning.
    #[serde(default)]
    pub auth: Vec<AuthConfig>,

    /// Keys a backend presents to the server API; the key selects this tenant.
    #[serde(default)]
    pub api_keys: Vec<ApiKeyConfig>,
    /// Forward matching Ext requests to a backend (ADR 0008 §6).
    #[serde(default)]
    pub rpc: Option<RpcConfig>,
    /// Rooms that keep recent messages for recovery (ADR 0011 §3); the
    /// first rule matching a room applies.
    #[serde(default)]
    pub history: Vec<HistoryRuleConfig>,
}

/// One `tenants[].history` rule.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct HistoryRuleConfig {
    /// Room pattern: exact name or `prefix*`.
    pub rooms: String,
    /// Messages kept per room (1-10000).
    #[serde(default = "default_history_size")]
    pub size: usize,
    /// Seconds a message is kept (1-86400).
    #[serde(default = "default_history_ttl_s")]
    pub ttl_s: u64,
    /// Bytes kept per room (1 KiB - 64 MiB); the oldest messages go first.
    #[serde(default = "default_history_max_bytes")]
    pub max_bytes: usize,
}

fn default_history_size() -> usize {
    200
}
fn default_history_ttl_s() -> u64 {
    600
}
fn default_history_max_bytes() -> usize {
    1 << 20
}

impl HistoryRuleConfig {
    pub fn validate(&self) -> Result<()> {
        let name = self.rooms.strip_suffix('*').unwrap_or(&self.rooms);
        if !name.is_empty() {
            wsprism_engine::validate_room_name(name).map_err(|e| {
                WsPrismError::BadRequest(format!(
                    "history.rooms {:?}: {}",
                    self.rooms,
                    e.client_message()
                ))
            })?;
        }
        if !(1..=10_000).contains(&self.size) {
            return Err(WsPrismError::BadRequest(
                "history.size must be between 1 and 10000".into(),
            ));
        }
        if !(1..=86_400).contains(&self.ttl_s) {
            return Err(WsPrismError::BadRequest(
                "history.ttl_s must be between 1 and 86400".into(),
            ));
        }
        if !(1024..=64 << 20).contains(&self.max_bytes) {
            return Err(WsPrismError::BadRequest(
                "history.max_bytes must be between 1024 and 67108864".into(),
            ));
        }
        Ok(())
    }
}

/// The server API listener.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ApiConfig {
    /// host:port, e.g. `127.0.0.1:8081`.
    pub listen: String,
}

/// A tenant API key, stored as the SHA-256 of the key (lowercase hex).
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ApiKeyConfig {
    /// For logs only.
    pub name: String,
    pub sha256: String,
}

/// `gateway.broker` (ADR 0011).
#[derive(Debug, Deserialize, Clone, PartialEq, Eq, Default)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum BrokerConfig {
    /// One node; rooms live in this process.
    #[default]
    Memory,
    /// Nodes share rooms through Redis pub/sub (experimental; ADR 0011 R3).
    Redis(Box<RedisBrokerConfig>),
}

/// `gateway.broker` with `kind: redis`.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct RedisBrokerConfig {
    /// `redis://` or `rediss://` URL. Exactly one of `url` / `url_env`.
    #[serde(default)]
    pub url: Option<String>,
    /// Environment variable holding the URL (keeps a password out of the
    /// config file).
    #[serde(default)]
    pub url_env: Option<String>,
    /// Prefix of every key and channel (several clusters can share one Redis).
    #[serde(default = "default_redis_prefix")]
    pub prefix: String,
    /// Subscriber connections, 1-64.
    #[serde(default = "default_pubsub_connections")]
    pub pubsub_connections: usize,
    /// Seconds a node's slot survives without a heartbeat, 3-300.
    #[serde(default = "default_slot_ttl_s")]
    pub slot_ttl_s: u64,
    /// Frames queued for Redis in total (the outbox, ADR 0011 B4).
    #[serde(default = "default_outbox_capacity")]
    pub outbox_capacity: usize,
    /// Frames one tenant may have queued.
    #[serde(default = "default_outbox_per_tenant")]
    pub outbox_per_tenant: usize,
    /// Refuse new frames (`UNAVAILABLE`) while the oldest queued one has
    /// waited this long, 10-60000 ms: fail fast instead of piling up latency.
    #[serde(default = "default_outbox_max_wait_ms")]
    pub outbox_max_wait_ms: u64,
    /// With a `rediss://` URL: a private CA and/or a client certificate
    /// (mutual TLS). Without it, `rediss://` trusts the system store.
    #[serde(default)]
    pub tls: Option<RedisTlsConfig>,
    /// Follow the primary through Redis Sentinel. `url` / `url_env` then
    /// only carry the primary's credentials and database (the host is not
    /// used). Not with TLS yet.
    #[serde(default)]
    pub sentinel: Option<RedisSentinelConfig>,
    /// Consuming a one-time ticket waits until replicas have it, so a
    /// failover cannot accept it again. Default: on with `sentinel`
    /// (1 replica, 500 ms), off without; `replicas: 0` turns it off.
    #[serde(default)]
    pub replica_ack: Option<RedisReplicaAckConfig>,
    /// Refuse new sessions (503 + `Retry-After`) while Redis is slow or
    /// this node's session claims pile up; on by default.
    #[serde(default)]
    pub admission: RedisAdmissionConfig,
}

/// `gateway.broker.admission`.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct RedisAdmissionConfig {
    /// Redis probe latency (ms) above which new sessions are refused,
    /// 0 = never refuse, else 10-60000.
    #[serde(default = "default_admission_latency_ms")]
    pub max_latency_ms: u64,
    /// This node's session claims waiting for Redis above which new
    /// sessions are refused, 1-1000000.
    #[serde(default = "default_admission_pending")]
    pub max_pending_claims: usize,
}

impl Default for RedisAdmissionConfig {
    fn default() -> Self {
        Self {
            max_latency_ms: default_admission_latency_ms(),
            max_pending_claims: default_admission_pending(),
        }
    }
}

fn default_admission_latency_ms() -> u64 {
    500
}
fn default_admission_pending() -> usize {
    10_000
}

/// `gateway.broker.replica_ack`.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct RedisReplicaAckConfig {
    /// Replicas that must acknowledge, 0-16 (0 = do not wait).
    #[serde(default = "default_ack_replicas")]
    pub replicas: u16,
    /// The longest wait, 1-10000 ms; past it the redemption fails (closed).
    #[serde(default = "default_ack_timeout_ms")]
    pub timeout_ms: u64,
}

impl Default for RedisReplicaAckConfig {
    fn default() -> Self {
        Self {
            replicas: default_ack_replicas(),
            timeout_ms: default_ack_timeout_ms(),
        }
    }
}

fn default_ack_replicas() -> u16 {
    1
}
fn default_ack_timeout_ms() -> u64 {
    500
}

/// `gateway.broker.sentinel`.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct RedisSentinelConfig {
    /// Sentinel addresses, `redis://host:26379`.
    pub addresses: Vec<String>,
    /// The primary's name in the Sentinel configuration (`mymaster`).
    pub master_name: String,
}

/// `gateway.broker.tls` (PEM files, read at startup).
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct RedisTlsConfig {
    /// CA certificate(s) the Redis server's certificate must chain to,
    /// instead of the system trust store.
    #[serde(default)]
    pub ca_file: Option<String>,
    /// Client certificate for mutual TLS (with `client_key_file`).
    #[serde(default)]
    pub client_cert_file: Option<String>,
    /// Its private key.
    #[serde(default)]
    pub client_key_file: Option<String>,
}

fn default_redis_prefix() -> String {
    "wsp".into()
}
fn default_pubsub_connections() -> usize {
    4
}
fn default_slot_ttl_s() -> u64 {
    30
}
fn default_outbox_capacity() -> usize {
    65_536
}
fn default_outbox_per_tenant() -> usize {
    16_384
}
fn default_outbox_max_wait_ms() -> u64 {
    1000
}

impl RedisBrokerConfig {
    /// `replica_ack` in effect: as set, else on with Sentinel (a failover
    /// is expected) and off without. `None` = off.
    pub fn effective_replica_ack(&self) -> Option<RedisReplicaAckConfig> {
        match self.replica_ack {
            Some(a) if a.replicas == 0 => None,
            Some(a) => Some(a),
            None => self.sentinel.is_some().then(RedisReplicaAckConfig::default),
        }
    }

    /// The URL (from `url` or the `url_env` variable).
    pub fn resolve_url(&self) -> Result<String> {
        match (&self.url, &self.url_env) {
            (Some(u), None) => Ok(u.clone()),
            (None, Some(var)) => std::env::var(var).map_err(|_| {
                WsPrismError::BadRequest(format!("gateway.broker.url_env {var} is not set"))
            }),
            _ => Err(WsPrismError::BadRequest(
                "gateway.broker: exactly one of url / url_env".into(),
            )),
        }
    }

    pub fn validate(&self) -> Result<()> {
        let bad = |m: &str| Err(WsPrismError::BadRequest(format!("gateway.broker.{m}")));
        if self.url.is_some() == self.url_env.is_some() {
            return bad("url / url_env: exactly one is required");
        }
        if let Some(u) = &self.url {
            if !(u.starts_with("redis://") || u.starts_with("rediss://")) {
                return bad("url must start with redis:// or rediss://");
            }
            if self.tls.is_some() && !u.starts_with("rediss://") {
                return bad("tls needs a rediss:// url");
            }
        }
        if let Some(s) = &self.sentinel {
            if s.addresses.is_empty() || s.addresses.iter().any(|a| !a.starts_with("redis://")) {
                return bad("sentinel.addresses: one or more redis://host:port");
            }
            if s.master_name.is_empty() {
                return bad("sentinel.master_name must not be empty");
            }
            if self.tls.is_some() {
                return bad("sentinel with tls is not supported yet");
            }
        }
        let adm = &self.admission;
        if adm.max_latency_ms != 0 && !(10..=60_000).contains(&adm.max_latency_ms) {
            return bad("admission.max_latency_ms must be 0 (off) or 10-60000");
        }
        if !(1..=1_000_000).contains(&adm.max_pending_claims) {
            return bad("admission.max_pending_claims must be 1-1000000");
        }
        if let Some(a) = &self.replica_ack {
            if a.replicas > 16 {
                return bad("replica_ack.replicas must be 0-16");
            }
            if !(1..=10_000).contains(&a.timeout_ms) {
                return bad("replica_ack.timeout_ms must be 1-10000");
            }
        }
        if let Some(t) = &self.tls {
            if t.client_cert_file.is_some() != t.client_key_file.is_some() {
                return bad("tls: client_cert_file and client_key_file go together");
            }
            if t.ca_file.is_none() && t.client_cert_file.is_none() {
                return bad("tls: set ca_file and/or client_cert_file + client_key_file");
            }
        }
        if self.prefix.is_empty()
            || self.prefix.len() > 64
            || !self
                .prefix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
        {
            return bad("prefix must be 1-64 of [A-Za-z0-9_.-]");
        }
        if !(1..=64).contains(&self.pubsub_connections) {
            return bad("pubsub_connections must be 1-64");
        }
        if !(3..=300).contains(&self.slot_ttl_s) {
            return bad("slot_ttl_s must be 3-300");
        }
        if self.outbox_capacity == 0
            || !(1..=self.outbox_capacity).contains(&self.outbox_per_tenant)
        {
            return bad("outbox_per_tenant must be 1..=outbox_capacity");
        }
        if !(10..=60_000).contains(&self.outbox_max_wait_ms) {
            return bad("outbox_max_wait_ms must be 10-60000");
        }
        Ok(())
    }
}

/// One link of a tenant's authentication chain.
#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum AuthConfig {
    /// `dev` -> user `dev`, `dev:<id>` -> user `<id>`. Development only.
    Dev,
    /// One-time tickets issued through the server API (`wpt_...`).
    Ticket,
    /// Signed JWTs (`sub` = user), verified with one pinned algorithm.
    Jwt(JwtConfig),
    /// Ask a backend over HTTP (ADR 0008 §6). Accepts every token, so it
    /// belongs last in the chain.
    Proxy(ProxyAuthConfig),
    /// An authenticator registered with `GatewayBuilder::auth_plugin`
    /// (ADR 0009 §3).
    Plugin(PluginAuthConfig),
}

/// `kind: plugin`: `name` selects the registered factory, `options` is
/// passed to it as-is.
#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct PluginAuthConfig {
    pub name: String,
    #[serde(default)]
    pub options: serde_json::Value,
}

/// `kind: proxy`: `POST url {tenant, token, client_ip}` ->
/// `200 {user, permissions?, expires_at?}` / `401`·`403`.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ProxyAuthConfig {
    pub url: String,
    #[serde(default = "default_connect_timeout_ms")]
    pub timeout_ms: u64,
    /// Environment variable whose value is sent as `Authorization: Bearer`.
    #[serde(default)]
    pub secret_env: Option<String>,
    /// Concurrent calls at most; beyond it connections get 503.
    #[serde(default = "default_connect_in_flight")]
    pub max_in_flight: u32,
}

fn default_connect_timeout_ms() -> u64 {
    2_000
}

fn default_connect_in_flight() -> u32 {
    256
}

/// `tenants[].rpc`: Ext messages matching `allow` go to
/// `POST url {tenant, user, sid, svc, type, room, data}`; the answer
/// `{data}` / `{error: {code, msg}}` comes back as `sys.reply` / `sys.error`.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct RpcConfig {
    pub url: String,
    /// `svc:type` / `svc:*` rules (built-in services excluded).
    pub allow: Vec<String>,
    #[serde(default = "default_rpc_timeout_ms")]
    pub timeout_ms: u64,
    /// Calls in flight per session; more are answered `RATE_LIMITED`.
    #[serde(default = "default_rpc_in_flight")]
    pub max_in_flight: u32,
    /// Calls in flight for the whole tenant (protects the backend from a
    /// connection storm); more are answered `RESOURCE_EXHAUSTED`.
    #[serde(default = "default_rpc_in_flight_total")]
    pub max_in_flight_total: u32,
    #[serde(default)]
    pub secret_env: Option<String>,
    /// Forward a request naming a `room` only if this session has joined
    /// it (default). With `false` the request is forwarded anyway and the
    /// backend gets `joined: false` — it must not trust such a room.
    #[serde(default = "default_true")]
    pub require_joined_room: bool,
}

fn default_rpc_timeout_ms() -> u64 {
    5_000
}

fn default_rpc_in_flight() -> u32 {
    16
}

fn default_rpc_in_flight_total() -> u32 {
    1024
}

/// A backend URL: `http` or `https`, with a host.
fn validate_backend_url(what: &str, url: &str) -> Result<()> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or_else(|| {
            WsPrismError::BadRequest(format!("{what}.url must start with http:// or https://"))
        })?;
    if rest.is_empty() || rest.starts_with('/') {
        return Err(WsPrismError::BadRequest(format!("{what}.url has no host")));
    }
    Ok(())
}

fn validate_timeout(what: &str, ms: u64, in_flight: u32) -> Result<()> {
    if !(1..=60_000).contains(&ms) {
        return Err(WsPrismError::BadRequest(format!(
            "{what}.timeout_ms must be 1-60000"
        )));
    }
    if in_flight == 0 {
        return Err(WsPrismError::BadRequest(format!(
            "{what}.max_in_flight must be > 0"
        )));
    }
    Ok(())
}

impl ProxyAuthConfig {
    pub fn validate(&self) -> Result<()> {
        validate_backend_url("auth proxy", &self.url)?;
        validate_timeout("auth proxy", self.timeout_ms, self.max_in_flight)
    }
}

impl RpcConfig {
    pub fn validate(&self) -> Result<()> {
        validate_backend_url("rpc", &self.url)?;
        validate_timeout("rpc", self.timeout_ms, self.max_in_flight)?;
        if self.max_in_flight_total == 0 {
            return Err(WsPrismError::BadRequest(
                "rpc.max_in_flight_total must be > 0".into(),
            ));
        }
        if self.allow.is_empty() {
            return Err(WsPrismError::BadRequest(
                "rpc.allow must not be empty".into(),
            ));
        }
        Ok(())
    }
}

/// `kind: jwt` (ADR 0008 §5). Exactly one key source: `secret_file` /
/// `secret_env` for HS*, `public_key_file` (PEM) for the others.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct JwtConfig {
    pub algorithm: JwtAlgorithm,
    #[serde(default)]
    pub secret_file: Option<String>,
    /// Name of an environment variable holding the secret.
    #[serde(default)]
    pub secret_env: Option<String>,
    #[serde(default)]
    pub public_key_file: Option<String>,
    /// JWKS URL of the issuer (asymmetric algorithms only); keys are picked
    /// by the token's `kid` (ADR 0012). `https`, or `http` to loopback.
    #[serde(default)]
    pub jwks_url: Option<String>,
    /// Refresh the JWKS when older than this (60-86400 s, default 300).
    #[serde(default = "default_jwks_refresh_s")]
    pub jwks_refresh_s: u64,
    /// Least time between request-triggered refetches (unknown `kid`, keys
    /// never loaded): 1-3600 s, default 30.
    #[serde(default = "default_jwks_min_refetch_s")]
    pub jwks_min_refetch_s: u64,
    /// Stop trusting the last fetched keys once they are older than this
    /// and no refresh succeeds (a key the IdP revoked must not stay valid
    /// through a JWKS outage): 1-604800 s, default 86400.
    #[serde(default = "default_jwks_max_stale_s")]
    pub jwks_max_stale_s: u64,
    /// If set, only tokens whose header `kid` equals it are this link's
    /// (others pass to the next link) — chain two links to rotate keys.
    #[serde(default)]
    pub kid: Option<String>,
    #[serde(default)]
    pub issuer: Option<String>,
    #[serde(default)]
    pub audience: Option<String>,
    /// Clock skew tolerated on `exp` / `nbf`, seconds (<= 300).
    #[serde(default = "default_jwt_leeway_s")]
    pub leeway_s: u64,
    /// Reject tokens without a `wsprism.tenant` claim (a claim naming
    /// another tenant is always rejected).
    #[serde(default)]
    pub require_tenant: bool,
}

fn default_jwt_leeway_s() -> u64 {
    30
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum JwtAlgorithm {
    HS256,
    HS384,
    HS512,
    RS256,
    RS384,
    RS512,
    PS256,
    PS384,
    PS512,
    ES256,
    ES384,
    EdDSA,
}

impl JwtAlgorithm {
    pub fn is_hmac(self) -> bool {
        matches!(self, Self::HS256 | Self::HS384 | Self::HS512)
    }
}

fn default_jwks_refresh_s() -> u64 {
    300
}

fn default_jwks_min_refetch_s() -> u64 {
    30
}

fn default_jwks_max_stale_s() -> u64 {
    86_400
}

/// `https://...`, or `http://` to a loopback host (tests, sidecars).
fn jwks_url_allowed(url: &str) -> bool {
    if url.starts_with("https://") && url.len() > "https://".len() {
        return true;
    }
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let host = rest.split(['/', '?']).next().unwrap_or("");
    let host = host.rsplit_once(':').map_or(host, |(h, p)| {
        if p.bytes().all(|b| b.is_ascii_digit()) {
            h
        } else {
            host
        }
    });
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

impl JwtConfig {
    pub fn validate(&self) -> Result<()> {
        let secret = self.secret_file.is_some() as u8 + self.secret_env.is_some() as u8;
        let public = self.public_key_file.is_some() as u8 + self.jwks_url.is_some() as u8;
        let ok = if self.algorithm.is_hmac() {
            secret == 1 && public == 0
        } else {
            secret == 0 && public == 1
        };
        if !ok {
            return Err(WsPrismError::BadRequest(format!(
                "jwt {:?}: set exactly one of {}",
                self.algorithm,
                if self.algorithm.is_hmac() {
                    "secret_file / secret_env"
                } else {
                    "public_key_file / jwks_url"
                }
            )));
        }
        if let Some(url) = &self.jwks_url {
            if !jwks_url_allowed(url) {
                return Err(WsPrismError::BadRequest(format!(
                    "jwt jwks_url must be https (http only to loopback): {url}"
                )));
            }
            if !(1..=604_800).contains(&self.jwks_max_stale_s) {
                return Err(WsPrismError::BadRequest(
                    "jwt jwks_max_stale_s must be 1-604800".into(),
                ));
            }
            if !(1..=3_600).contains(&self.jwks_min_refetch_s) {
                return Err(WsPrismError::BadRequest(
                    "jwt jwks_min_refetch_s must be 1-3600".into(),
                ));
            }
            if !(60..=86_400).contains(&self.jwks_refresh_s) {
                return Err(WsPrismError::BadRequest(
                    "jwt jwks_refresh_s must be 60-86400".into(),
                ));
            }
        }
        if self.leeway_s > 300 {
            return Err(WsPrismError::BadRequest(
                "jwt leeway_s must be <= 300".into(),
            ));
        }
        Ok(())
    }
}

impl TenantConfig {
    pub fn validate(&self) -> Result<()> {
        for k in &self.api_keys {
            if k.sha256.len() != 64 || !k.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(WsPrismError::BadRequest(format!(
                    "tenant {}: api_keys[{}].sha256 must be 64 hex characters (SHA-256 of the key)",
                    self.id, k.name
                )));
            }
        }
        let in_tenant = |e: WsPrismError| {
            WsPrismError::BadRequest(format!("tenant {}: {}", self.id, e.client_message()))
        };
        for a in &self.auth {
            match a {
                AuthConfig::Jwt(j) => j.validate(),
                AuthConfig::Proxy(p) => p.validate(),
                AuthConfig::Dev | AuthConfig::Ticket | AuthConfig::Plugin(_) => Ok(()),
            }
            .map_err(in_tenant)?;
        }
        if let Some(rpc) = &self.rpc {
            rpc.validate().map_err(in_tenant)?;
        }
        for h in &self.history {
            h.validate().map_err(in_tenant)?;
        }
        if self.limits.max_frame_bytes == 0 {
            return Err(WsPrismError::BadRequest(
                "limits.max_frame_bytes must be > 0".into(),
            ));
        }
        self.policy.validate()?;
        Ok(())
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct TenantLimits {
    /// Maximum allowed frame size for this tenant (bytes).
    #[serde(default = "default_max_frame_bytes")]
    pub max_frame_bytes: usize,

    // Sprint 5: Resource Governance
    /// Max concurrent sessions for the entire tenant. 0 = unlimited.
    /// Lock-free best-effort: under extreme contention a tiny overshoot is possible.
    #[serde(default)]
    pub max_sessions_total: u64,
    /// Max active rooms for the tenant. 0 = unlimited.
    /// A room counts as "active" if it has at least one session.
    #[serde(default)]
    pub max_rooms_total: u64,
    /// Max users allowed in a single room. 0 = unlimited.
    #[serde(default)]
    pub max_users_per_room: u64,
    /// Max unique rooms a single user can join. 0 = unlimited.
    /// Default 64; `0` = unlimited (each joined room costs memory).
    #[serde(default = "default_max_rooms_per_user")]
    pub max_rooms_per_user: u64,
}

impl TenantLimits {
    /// The part of the limits the engine enforces on room joins.
    pub fn room_limits(&self) -> wsprism_engine::RoomLimits {
        wsprism_engine::RoomLimits {
            max_rooms_total: self.max_rooms_total,
            max_users_per_room: self.max_users_per_room,
            max_rooms_per_user: self.max_rooms_per_user,
        }
    }
}

impl Default for TenantLimits {
    fn default() -> Self {
        Self {
            max_frame_bytes: 4096,
            max_sessions_total: 0,
            max_rooms_total: 0,
            max_users_per_room: 0,
            max_rooms_per_user: default_max_rooms_per_user(),
        }
    }
}

fn default_max_rooms_per_user() -> u64 {
    64
}

fn default_max_frame_bytes() -> usize {
    4096
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RateLimitScope {
    Tenant,
    Connection,
    Both,
}

fn default_rate_limit_scope() -> RateLimitScope {
    RateLimitScope::Connection
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum HotErrorMode {
    SysError,
    Silent,
}

fn default_hot_error_mode() -> HotErrorMode {
    HotErrorMode::SysError
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SessionMode {
    Single,
    Multi,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OnExceed {
    Deny,
    KickOldest,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct SessionPolicy {
    #[serde(default = "default_session_mode")]
    pub mode: SessionMode,

    #[serde(default = "default_max_sessions_per_user")]
    pub max_sessions_per_user: u32,

    #[serde(default = "default_on_exceed")]
    pub on_exceed: OnExceed,
}

fn default_session_mode() -> SessionMode {
    SessionMode::Multi
}
fn default_max_sessions_per_user() -> u32 {
    4
}
fn default_on_exceed() -> OnExceed {
    OnExceed::Deny
}

impl Default for SessionPolicy {
    fn default() -> Self {
        Self {
            mode: default_session_mode(),
            max_sessions_per_user: default_max_sessions_per_user(),
            on_exceed: default_on_exceed(),
        }
    }
}

/// Tenant policy knobs.
/// Defaults are STRICT (deny-by-default).
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct TenantPolicy {
    /// Tenant-level or connection-level inbound rate limit in requests per second.
    #[serde(default = "default_rate_limit_rps")]
    pub rate_limit_rps: u32,

    /// Burst capacity for token bucket.
    #[serde(default = "default_rate_limit_burst")]
    pub rate_limit_burst: u32,

    /// Where to apply rate limiting.
    #[serde(default = "default_rate_limit_scope")]
    pub rate_limit_scope: RateLimitScope,

    /// Ext lane allowlist entries, like "svc:type"
    #[serde(default = "default_ext_allowlist")]
    pub ext_allowlist: Vec<String>,

    /// Hot lane allowlist entries, like "sid:opcode"
    #[serde(default)]
    pub hot_allowlist: Vec<String>,

    /// Session policy (1:1 / 1:N)
    #[serde(default)]
    pub sessions: SessionPolicy,

    /// Hot lane error surface (sys.error vs silent)
    #[serde(default = "default_hot_error_mode")]
    pub hot_error_mode: HotErrorMode,

    /// If true, hot lane requires active_room.
    #[serde(default = "default_hot_requires_active_room")]
    pub hot_requires_active_room: bool,

    /// Send a session's own Hot state back to it through the room relay.
    /// Default true (compatible); state-sync tenants usually want false, which
    /// saves ~1/N of a room's Hot traffic.
    #[serde(default = "default_true")]
    pub hot_echo_to_sender: bool,

    /// Tell room members when someone joins or leaves (`sys.member_joined` /
    /// `sys.member_left`) and list current members in `sys.joined`
    /// (ADR 0010). Off by default: each join/leave is broadcast to the room.
    #[serde(default)]
    pub presence_events: bool,

    /// Send each session's pending keyed Hot state at most this many times
    /// per second (0 = as soon as it arrives). Fewer flushes under heavy
    /// update rates, at the cost of up to one tick of extra latency.
    #[serde(default)]
    pub hot_tick_hz: u32,

    /// Browser origins allowed to open sockets (CSWSH protection). Requests
    /// without an `Origin` header (non-browser clients) are always allowed.
    /// Empty (default) = deny every browser origin. See `policy::origin`.
    #[serde(default)]
    pub allowed_origins: Vec<String>,

    /// Where clients may present their ticket, in priority order. The query
    /// string is not included by default because URLs are logged by proxies.
    #[serde(default = "default_ticket_sources")]
    pub ticket_sources: Vec<TicketSource>,
}

/// A place a client can carry its ticket on the upgrade request.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TicketSource {
    /// `Authorization: Bearer <token>` (native clients).
    Header,
    /// `Sec-WebSocket-Protocol: wsprism.v1, wsprism.auth.<token>` (browsers).
    Subprotocol,
    /// `?ticket=<token>` (legacy; ends up in access logs).
    Query,
}

fn default_ticket_sources() -> Vec<TicketSource> {
    vec![TicketSource::Header, TicketSource::Subprotocol]
}

fn default_hot_requires_active_room() -> bool {
    true
}

impl Default for TenantPolicy {
    fn default() -> Self {
        Self {
            rate_limit_rps: default_rate_limit_rps(),
            rate_limit_burst: default_rate_limit_burst(),
            rate_limit_scope: default_rate_limit_scope(),
            ext_allowlist: default_ext_allowlist(),
            hot_allowlist: Vec::new(),
            sessions: SessionPolicy::default(),
            hot_error_mode: default_hot_error_mode(),
            hot_requires_active_room: default_hot_requires_active_room(),
            hot_echo_to_sender: true,
            presence_events: false,
            hot_tick_hz: 0,
            allowed_origins: Vec::new(),
            ticket_sources: default_ticket_sources(),
        }
    }
}

impl TenantPolicy {
    pub fn validate(&self) -> Result<()> {
        if self.hot_tick_hz > 240 {
            return Err(WsPrismError::BadRequest(
                "policy.hot_tick_hz must be 0 (immediate) or at most 240".into(),
            ));
        }
        if self.rate_limit_rps == 0 || self.rate_limit_burst == 0 {
            return Err(WsPrismError::BadRequest(
                "policy.rate_limit_rps and rate_limit_burst must be > 0".into(),
            ));
        }
        if self.ticket_sources.is_empty() {
            return Err(WsPrismError::BadRequest(
                "policy.ticket_sources must not be empty".into(),
            ));
        }
        for (i, src) in self.ticket_sources.iter().enumerate() {
            if self.ticket_sources[..i].contains(src) {
                return Err(WsPrismError::BadRequest(format!(
                    "policy.ticket_sources: duplicate entry {src:?}"
                )));
            }
        }
        // sessions policy sanity
        match self.sessions.mode {
            SessionMode::Single => {
                if self.sessions.max_sessions_per_user != 1 {
                    return Err(WsPrismError::BadRequest(
                        "policy.sessions.mode=single requires max_sessions_per_user=1".into(),
                    ));
                }
            }
            SessionMode::Multi => {
                if self.sessions.max_sessions_per_user == 0 {
                    return Err(WsPrismError::BadRequest(
                        "policy.sessions.max_sessions_per_user must be > 0".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

fn default_rate_limit_rps() -> u32 {
    200
}
fn default_rate_limit_burst() -> u32 {
    400
}
fn default_ext_allowlist() -> Vec<String> {
    vec!["room:join".into(), "room:leave".into()]
}
