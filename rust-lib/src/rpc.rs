//! Proxyable Ethereum JSON-RPC client core.
//!
//! Holds per-chain configuration (endpoint + proxy policy), persisted to disk,
//! and exposes chainId-keyed RPC calls. Every outbound request is built through
//! the fail-closed [`crate::proxy`] chokepoint, so a chain configured with
//! `proxy_required` and no usable proxy refuses to call rather than leaking in
//! the clear. Pure (no Logos deps) and unit-testable with `cargo test`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::proxy::{build_client, ProxyConfig};

fn default_timeout() -> u64 {
    // Well under the 20s Logos RPC deadline. At 30 a single dead endpoint outlived the
    // protocol timeout, so the caller saw a transport failure rather than "this chain is
    // unreachable" — and behind a single-dispatch coordinator it took every other call with it.
    8
}

fn default_verified_timeout() -> u64 {
    // The verified path is a second hop (us -> proxy -> its provider) and a light client may
    // be walking headers, so it gets more room — still under the deadline.
    15
}

/// The verified leg's per-call budget, from the chain's `verifiedTimeoutSecs`. Clamped because
/// the value is user-supplied: 0 would time out instantly and an unbounded one never returns.
fn verified_budget(secs: u64) -> Duration {
    Duration::from_secs(secs.clamp(1, 60))
}

/// Below this a bound buys nothing — it cannot complete a handshake, and the protocol ABI
/// refuses a sub-millisecond timeout rather than clamping it. A caller asking for less is
/// REFUSED in words, never quietly given more than it asked for: a callee that outlives the
/// caller which set the bound is the defect this whole parameter exists to close.
const MIN_BUDGET: Duration = Duration::from_millis(50);

/// The wall time a chain's `timeoutSecs` already permits, and so the most a caller's deadline
/// may be widened to — it may only shorten this.
///
/// TWICE the configured value, because reqwest's BLOCKING client applies its client-level
/// timeout as two independent timers — send-until-headers, then a fresh one for the body — and
/// `post_rpc` does both. MEASURED: `ClientBuilder::timeout(1000ms)` against a node stalling
/// 600ms on headers and 600ms on the body SUCCEEDS at 1.21s. `0` means reqwest's own 30s.
fn configured_wall(timeout_secs: u64) -> Duration {
    let per_phase = if timeout_secs == 0 { 30 } else { timeout_secs };
    Duration::from_secs(per_phase.saturating_mul(2))
}

/// A caller's own wall budget for one call. `None`, or anything at or below zero, means "use
/// this chain's configured timeout" — exactly what every caller got before the parameter
/// existed, which is what keeps an un-rebuilt consumer working unchanged.
pub fn caller_deadline(ms: Option<i64>) -> Option<Duration> {
    ms.filter(|m| *m > 0).map(|m| Duration::from_millis(m as u64))
}

/// Render a transport failure with its cause chain.
///
/// reqwest's `Display` names only the stage that failed, so a refused connection, an expired
/// timeout and a dropped socket all read `error sending request for url (...)` and nothing else.
/// That is the whole message a caller gets, and it cannot be acted on. Walking `source()` appends
/// the reason the OS actually gave.
fn http_error(e: reqwest::Error, budget: Option<Duration>) -> RpcError {
    use std::error::Error;
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(cause) = src {
        s.push_str(&format!(": {cause}"));
        src = cause.source();
    }
    // MEASURED: a HEADERS expiry reads "error sending request for url (...)" — word for word
    // what a REFUSED connection reads — and a BODY expiry reads "error decoding response
    // body", word for word what a malformed body reads. Only `is_timeout` tells them apart.
    match (e.is_timeout(), budget) {
        (true, Some(b)) => RpcError::Timeout { budget_ms: b.as_millis(), detail: s },
        _ => RpcError::Http(s),
    }
}

/// How JSON-RPC for a chain should be routed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VerifiedProxyMode {
    /// Talk to the configured endpoint directly.
    #[default]
    Off,
    /// Route through the light-client proxy and REFUSE rather than fall back. There is no
    /// `preferred` mode on purpose: quietly answering from an unverified source when the user
    /// asked for verification is the failure this feature exists to prevent.
    Required,
}

/// Who wrote a chain's record. Reporting only — [`EthRpc::ensure_chain_config`] is what
/// actually refuses to overwrite. [`ChainConfigWire`] omits it and
/// [`ChainConfig::from_caller_json`] overwrites it, so a caller cannot declare its own write
/// to be a default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConfigSource {
    /// A caller wrote it — and the default on READ: a record already on disk was written by
    /// somebody, and calling it ours would license `init_defaults` to overwrite it.
    #[default]
    External,
    /// `init_defaults` wrote it from [`DEFAULT_ENDPOINTS`].
    #[serde(rename = "default")]
    Builtin,
}

/// Per-chain configuration. `endpoint` is the JSON-RPC URL; `proxy` /
/// `proxy_required` drive the fail-closed client construction. JSON is
/// camelCase (`proxyRequired`, `timeoutSecs`) to match the wallet backend.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainConfig {
    pub endpoint: String,
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(default)]
    pub proxy_required: bool,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Whether to route through the light-client verified proxy. Distinct from `proxy`
    /// above, which is a NETWORK proxy (SOCKS5h/Tor) answering a different question: that one
    /// hides who is asking, this one proves the answer.
    #[serde(default)]
    pub verified_proxy_mode: VerifiedProxyMode,
    #[serde(default = "default_verified_timeout")]
    pub verified_timeout_secs: u64,
    /// Absent on a `chains.json` written before this field existed → `external`.
    #[serde(default)]
    pub source: ConfigSource,
}

/// The endpoints `init_defaults` seeds, so a fresh device works with no UI app installed.
/// Measured live 2026-08-28. One per chain, not a list: a silent failover changes WHICH node
/// answered without the consumer knowing. All three are one operator — a reason `eth_rpc_ui`
/// and the SOCKS proxy exist, not a reason to ship an endpoint that does not work.
pub const DEFAULT_ENDPOINTS: &[(u64, &str)] = &[
    (1, "https://ethereum-rpc.publicnode.com"),
    (11155111, "https://ethereum-sepolia-rpc.publicnode.com"),
    (560048, "https://ethereum-hoodi-rpc.publicnode.com"),
];

impl ChainConfig {
    /// The built-in record for `endpoint`. Verified routing is OFF: it needs an archive node
    /// the public defaults are not, and `validate` refuses it beside `proxyRequired`.
    pub fn builtin(endpoint: &str) -> Self {
        ChainConfig {
            endpoint: endpoint.into(),
            proxy: None,
            proxy_required: false,
            timeout_secs: default_timeout(),
            verified_proxy_mode: VerifiedProxyMode::Off,
            verified_timeout_secs: default_verified_timeout(),
            source: ConfigSource::Builtin,
        }
    }

    /// Parse caller JSON, forcing `source`. The field stays deserializable so `chains.json`
    /// reads back, but a caller writing a record makes it theirs whatever it claims.
    pub fn from_caller_json(json: &str) -> std::result::Result<Self, serde_json::Error> {
        let mut cfg: ChainConfig = serde_json::from_str(json)?;
        cfg.source = ConfigSource::External;
        Ok(cfg)
    }

    /// `proxyRequired` together with verified routing is a contradiction we refuse rather
    /// than silently resolve: the verified proxy makes its own outbound connections and knows
    /// nothing about our SOCKS config, so honouring both is impossible and honouring either
    /// silently breaks the guarantee the other was asked for.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.proxy_required && self.verified_proxy_mode == VerifiedProxyMode::Required {
            return Err("proxyRequired and verifiedProxyMode=required cannot both be set: the \
                        verified proxy makes its own connections and cannot honour a SOCKS \
                        proxy configured here"
                .into());
        }
        Ok(())
    }
}

/// The wire form of a chain config, as a caller sends it to `set_chain_config`.
///
/// The verified fields are `Option` here and NOT in [`ChainConfig`], because on the wire an
/// omitted key must preserve what is stored: a sibling wallet that predates verified routing
/// sends `{endpoint, proxy, proxyRequired, timeoutSecs}` on every start, and under a whole-record
/// write its silence revoked the user's verified mode with the UI honestly reporting "off".
/// Only an explicit `"off"` may lower it.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainConfigWire {
    pub endpoint: String,
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(default)]
    pub proxy_required: bool,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub verified_proxy_mode: Option<VerifiedProxyMode>,
    #[serde(default)]
    pub verified_timeout_secs: Option<u64>,
}

impl ChainConfigWire {
    /// Resolve against what is stored: omitted verified fields keep the stored value, or the
    /// default where the chain is new.
    pub fn resolve(self, existing: Option<&ChainConfig>) -> ChainConfig {
        ChainConfig {
            endpoint: self.endpoint,
            proxy: self.proxy,
            proxy_required: self.proxy_required,
            timeout_secs: self.timeout_secs,
            verified_proxy_mode: self
                .verified_proxy_mode
                .or_else(|| existing.map(|c| c.verified_proxy_mode))
                .unwrap_or_default(),
            verified_timeout_secs: self
                .verified_timeout_secs
                .or_else(|| existing.map(|c| c.verified_timeout_secs))
                .unwrap_or_else(default_verified_timeout),
            // Not resolved from the wire: a caller writing a record makes it theirs.
            source: ConfigSource::External,
        }
    }
}

#[derive(Debug)]
pub enum RpcError {
    UnknownChain(u64),
    Proxy(String),
    Http(String),
    /// The budget ran out. Distinct from [`RpcError::Http`] because a caller retries a slow
    /// endpoint and REPLACES a refused one, and reqwest reports both through one type.
    Timeout { budget_ms: u128, detail: String },
    Rpc { code: i64, message: String },
    Parse(String),
    /// The verified path could not answer. NEVER downgraded to a direct call: a caller who
    /// asked for verification gets an error, not an unverified number.
    VerifiedProxy(String),
    /// A read the verified route PROVES was asked for through an explicit url, which cannot
    /// prove anything. Refused rather than answered in the clear.
    VerifiedBypass(String),
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcError::UnknownChain(id) => write!(f, "no configuration for chain {id}"),
            RpcError::Proxy(e) => write!(f, "proxy: {e}"),
            RpcError::Http(e) => write!(f, "http: {e}"),
            RpcError::Timeout { budget_ms, detail } => {
                write!(f, "no answer within {budget_ms}ms: {detail}")
            }
            RpcError::Rpc { code, message } => write!(f, "rpc error {code}: {message}"),
            RpcError::VerifiedProxy(e) => write!(f, "verified proxy: {e}"),
            RpcError::VerifiedBypass(m) => write!(
                f,
                "{m} is proof-backed on this chain's verified route: refused through an \
                 explicit url, which cannot prove it (use raw_rpc)"
            ),
            RpcError::Parse(e) => write!(f, "parse: {e}"),
        }
    }
}

impl RpcError {
    /// The machine discriminator. The RPC path was the last consumer-facing refusal on this
    /// module without one, while `unready`, `config_status` and `verified_proxy_status` all
    /// grew one so callers would stop matching on message text. `error` prose is unchanged.
    pub fn code(&self) -> &'static str {
        match self {
            RpcError::UnknownChain(_) => "unknown_chain",
            RpcError::Proxy(_) => "proxy",
            RpcError::Http(_) => "http",
            RpcError::Timeout { .. } => "timeout",
            RpcError::Rpc { .. } => "rpc",
            RpcError::Parse(_) => "parse",
            RpcError::VerifiedProxy(_) => "verified_proxy",
            RpcError::VerifiedBypass(_) => "verified_bypass",
        }
    }
}

type Result<T> = std::result::Result<T, RpcError>;

/// What the verified proxy can actually say about an answer.
///
/// A light client proves state against a header's stateRoot. It cannot prove a fee oracle's
/// opinion or that a broadcast was accepted — those are forwarded to its own execution
/// provider and come back on trust. Badging them "verified" would be a false claim on exactly
/// the numbers that decide what a transaction costs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifiedClass {
    Verified,
    Proxied,
}

pub fn verified_class(method: &str) -> VerifiedClass {
    match method {
        "eth_getBalance" | "eth_getTransactionCount" | "eth_getCode" | "eth_getStorageAt"
        | "eth_call" => VerifiedClass::Verified,
        _ => VerifiedClass::Proxied,
    }
}

/// The wire label for how an answer was obtained, so a UI can badge a proven balance
/// differently from a fee figure the proxy merely forwarded. `None` never touched the proxy.
pub fn route_label(class: Option<VerifiedClass>) -> &'static str {
    match class {
        Some(VerifiedClass::Verified) => "verified",
        Some(VerifiedClass::Proxied) => "proxied",
        None => "direct",
    }
}

/// The wire label for a chain's verified-proxy mode — what the `verified_proxy_mode_changed`
/// event carries and what `set_verified_proxy_mode` accepts, spelled once.
pub fn mode_label(mode: VerifiedProxyMode) -> &'static str {
    match mode {
        VerifiedProxyMode::Off => "off",
        VerifiedProxyMode::Required => "required",
    }
}

/// What a mutation actually moved. A setter that stores the same bytes must not wake every
/// subscriber, so the emit decision is a diff of the record, never "a setter was called".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChainChange {
    /// The stored record differs — added, removed, or any field.
    pub config: bool,
    /// The verified-proxy gate moved, carrying the mode now in force.
    pub mode: Option<VerifiedProxyMode>,
}

impl ChainChange {
    pub fn any(&self) -> bool {
        self.config || self.mode.is_some()
    }
}

/// Diff one chain's record across a mutation. An ABSENT record reads as `off`, so removing a
/// `required` chain reports the gate closing rather than reporting nothing — that is what a
/// consumer observes through `verified_proxy_status`, which treats no config as off.
pub fn diff_chain(before: Option<&ChainConfig>, after: Option<&ChainConfig>) -> ChainChange {
    let mode_of = |c: Option<&ChainConfig>| c.map(|c| c.verified_proxy_mode).unwrap_or_default();
    let (b, a) = (mode_of(before), mode_of(after));
    ChainChange { config: before != after, mode: (b != a).then_some(a) }
}

/// The JSON-RPC method each typed helper issues. Named once so a caller can ask
/// [`EthRpc::route_of`] about a helper without re-typing — and mistyping — the name.
pub mod methods {
    pub const CHAIN_ID: &str = "eth_chainId";
    pub const BLOCK_NUMBER: &str = "eth_blockNumber";
    pub const GET_BALANCE: &str = "eth_getBalance";
    pub const CALL: &str = "eth_call";
    pub const GET_TRANSACTION_COUNT: &str = "eth_getTransactionCount";
    pub const GAS_PRICE: &str = "eth_gasPrice";
    pub const FEE_HISTORY: &str = "eth_feeHistory";
    pub const ESTIMATE_GAS: &str = "eth_estimateGas";
    pub const SEND_RAW_TRANSACTION: &str = "eth_sendRawTransaction";
    pub const GET_TRANSACTION_RECEIPT: &str = "eth_getTransactionReceipt";
    pub const GET_TRANSACTION_BY_HASH: &str = "eth_getTransactionByHash";
}

/// Rewrite params for the verified leg ONLY.
///
/// What eth_rpc sends to real nodes must not change: `eth_feeHistory`'s blockCount is a hex
/// QUANTITY string per ethereum/execution-apis, and the proxy declaring it `u64` is the
/// non-conformant side. Coercing at the source would make a shared module violate the spec to
/// suit one consumer, and be wrong against any strict node. So the translation lives here,
/// where it is the proxy's dialect being accommodated.
///
/// Every rule below was MEASURED against a live sepolia light client, not inferred.
pub fn verified_params(method: &str, params: &Value) -> Value {
    let mut p = params.as_array().cloned().unwrap_or_default();
    match method {
        // A hex blockCount returns success:true with EMPTY arrays — no error, just nothing.
        "eth_feeHistory" => {
            if let Some(Value::String(h)) = p.first().cloned() {
                if let Some(n) = h.strip_prefix("0x").and_then(|x| u64::from_str_radix(x, 16).ok()) {
                    p[0] = json!(n);
                }
            }
        }
        // "pending" is REFUSED and unverifiable by construction: a light client proves against
        // a header's stateRoot and pending has no canonical header. "latest" is the only tag
        // that works — which is what makes nonce reservation load-bearing in verified mode.
        "eth_getTransactionCount" => {
            if p.len() >= 2 && p[1] == json!("pending") {
                p[1] = json!("latest");
            }
        }
        // Upstream extends these with a third positional `optimisticStateFetch`; a one-arg
        // eth_estimateGas is refused outright with "parameters missing".
        "eth_call" | "eth_estimateGas" | "eth_createAccessList" => {
            while p.len() < 2 {
                p.push(json!("latest"));
            }
            if p.len() == 2 {
                p.push(json!(false));
            }
        }
        _ => {}
    }
    Value::Array(p)
}

/// Normalise a verified-leg RESULT back to what a JSON-RPC node would have returned.
///
/// The proxy's encoding is not uniform with the wire format, and the differences are silent:
/// `eth_call` answers a BYTE ARRAY where a node answers a hex string, and `eth_blockNumber`
/// answers a JSON number where a node answers a hex quantity. A consumer decoding hex gets
/// garbage from the first and a type error from the second — with no error anywhere, because
/// both are perfectly valid JSON. Measured against mainnet, both directions.
///
/// Only shapes that are unambiguously the proxy's dialect are rewritten; anything already in
/// wire form passes through untouched.
pub fn normalize_verified_result(v: Value) -> Value {
    match &v {
        // A byte array — every element a small integer — is `bytes` the node would have hex'd.
        Value::Array(items)
            if !items.is_empty()
                && items.iter().all(|i| i.as_u64().is_some_and(|n| n <= 255)) =>
        {
            let hex: String =
                items.iter().map(|i| format!("{:02x}", i.as_u64().unwrap_or(0))).collect();
            Value::String(format!("0x{hex}"))
        }
        // A bare number where the wire carries a QUANTITY.
        Value::Number(n) if n.is_u64() => Value::String(format!("0x{:x}", n.as_u64().unwrap_or(0))),
        _ => v,
    }
}

/// Supplies the verified leg. Implemented in the Logos glue, which owns the module call, so
/// `rpc.rs` stays free of Logos dependencies and the routing DECISION stays unit-testable.
pub trait VerifiedRouter: Send + Sync {
    /// Dispatch through the proxy within `budget`. Passed per call, not configured on the
    /// router: a router holding its own copy is a second place the user's setting can go stale.
    /// `Err` is a refusal, never a licence to fall back.
    fn call(&self, chain_id: u64, method: &str, params: &Value, budget: Duration)
        -> std::result::Result<Value, String>;
}

/// The RPC client: a persisted map of chainId → [`ChainConfig`].
pub struct EthRpc {
    chains: HashMap<u64, ChainConfig>,
    store_path: Option<PathBuf>,
    verified: Option<std::sync::Arc<dyn VerifiedRouter>>,
}

impl EthRpc {
    pub fn new() -> Self {
        Self { chains: HashMap::new(), store_path: None, verified: None }
    }

    /// Open a store backed by `path` (a JSON file), loading any existing config.
    pub fn with_store(path: PathBuf) -> Self {
        let mut s = Self::new();
        s.store_path = Some(path);
        s.load();
        s
    }

    fn load(&mut self) {
        if let Some(p) = &self.store_path {
            if let Ok(txt) = std::fs::read_to_string(p) {
                if let Ok(m) = serde_json::from_str::<HashMap<String, ChainConfig>>(&txt) {
                    self.chains =
                        m.into_iter().filter_map(|(k, v)| k.parse::<u64>().ok().map(|id| (id, v))).collect();
                }
            }
        }
    }

    fn persist(&self) {
        if let Some(p) = &self.store_path {
            let m: HashMap<String, ChainConfig> =
                self.chains.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
            if let Ok(txt) = serde_json::to_string_pretty(&m) {
                if let Some(parent) = p.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(p, txt);
            }
        }
    }

    pub fn set_verified_router(&mut self, r: std::sync::Arc<dyn VerifiedRouter>) {
        self.verified = Some(r);
    }

    pub fn set_chain_config(&mut self, chain_id: u64, cfg: ChainConfig) -> std::result::Result<(), String> {
        cfg.validate()?;
        self.chains.insert(chain_id, cfg);
        self.persist();
        Ok(())
    }

    /// Apply a config as a CALLER sent it: omitted verified fields keep what is stored, so a
    /// wallet that never heard of verified routing cannot revoke it by silence. An incoming
    /// `proxyRequired` that now contradicts the stored mode is refused, not resolved —
    /// resolving it is what silently dropped the mode before.
    pub fn apply_chain_config(&mut self, chain_id: u64, wire: ChainConfigWire) -> std::result::Result<(), String> {
        let cfg = wire.resolve(self.chains.get(&chain_id));
        self.set_chain_config(chain_id, cfg)
    }

    /// Seed a chain only where it is ABSENT, per field. `chains.json` is shared with other
    /// wallets on this device: a blanket overwrite silently retunes theirs, and a blanket skip
    /// leaves a stale value we own. Returns the fields actually written.
    pub fn ensure_chain_config(&mut self, chain_id: u64, cfg: &ChainConfig) -> Vec<String> {
        let mut seeded = Vec::new();
        match self.chains.get_mut(&chain_id) {
            None => {
                self.chains.insert(chain_id, cfg.clone());
                seeded.push("*".to_string());
            }
            Some(existing) => {
                if existing.endpoint.trim().is_empty() && !cfg.endpoint.trim().is_empty() {
                    existing.endpoint = cfg.endpoint.clone();
                    seeded.push("endpoint".into());
                    // `source` only ever climbs: a caller's value makes the record theirs.
                    if cfg.source == ConfigSource::External {
                        existing.source = ConfigSource::External;
                    }
                }
            }
        }
        if !seeded.is_empty() {
            self.persist();
        }
        seeded
    }

    /// Seed every chain in [`DEFAULT_ENDPOINTS`], per field and only where ABSENT. Idempotent:
    /// a second call from any consumer finds each field present and writes nothing.
    /// Returns what each chain actually gained, so a caller can tell seeding from a no-op.
    pub fn init_defaults(&mut self) -> Vec<(u64, Vec<String>)> {
        DEFAULT_ENDPOINTS
            .iter()
            .map(|&(id, url)| (id, self.ensure_chain_config(id, &ChainConfig::builtin(url))))
            .collect()
    }

    /// Whether a config has been SET, per chain and rolled up. The roll-up is for DISPLAY: a
    /// store with chain 1 configured and 11155111 absent rolls up to `configured` while still
    /// needing seeding, so a consumer calls `init_defaults` unconditionally rather than gating.
    pub fn config_status(&self) -> Value {
        let mut ids: Vec<u64> = self.chains.keys().copied().collect();
        ids.extend(DEFAULT_ENDPOINTS.iter().map(|&(id, _)| id));
        ids.sort_unstable();
        ids.dedup();
        let chains: Vec<Value> = ids
            .iter()
            .map(|id| match self.chains.get(id) {
                Some(c) => json!({ "chainId": id, "state": "configured", "source": c.source,
                                   "endpoint": c.endpoint,
                                   "verifiedProxyMode": c.verified_proxy_mode }),
                None => json!({ "chainId": id, "state": "unconfigured", "source": "none" }),
            })
            .collect();
        let (state, source) = if self.chains.is_empty() {
            ("unconfigured", "none")
        } else if self.chains.values().any(|c| c.source == ConfigSource::External) {
            ("configured", "external")
        } else {
            ("configured", "default")
        };
        json!({ "ok": true, "state": state, "source": source, "chains": chains })
    }

    /// Overwrite ONLY the endpoint, creating the chain with defaults where it is absent.
    /// `chains.json` is shared with other wallets on this device, so a user retyping an
    /// endpoint must not silently reset their verified-proxy mode or timeouts.
    pub fn patch_chain_endpoint(&mut self, chain_id: u64, endpoint: &str) -> bool {
        let e = endpoint.trim();
        if e.is_empty() {
            return false;
        }
        // A user typing an endpoint owns that chain from here on, defaulted or not.
        match self.chains.get_mut(&chain_id) {
            Some(c) => {
                c.endpoint = e.to_string();
                c.source = ConfigSource::External;
            }
            None => {
                let c = ChainConfig { source: ConfigSource::External, ..ChainConfig::builtin(e) };
                self.chains.insert(chain_id, c);
            }
        }
        self.persist();
        true
    }

    /// Overwrite only the transport timeouts this module owns. Lowering a DEFAULT is useless
    /// without this: an existing chains.json already carries the old value and `load` prefers
    /// what is on disk.
    pub fn patch_chain_transport(&mut self, chain_id: u64, timeout_secs: Option<u64>, verified_timeout_secs: Option<u64>) -> bool {
        let Some(c) = self.chains.get_mut(&chain_id) else { return false };
        if let Some(t) = timeout_secs { c.timeout_secs = t; }
        if let Some(t) = verified_timeout_secs { c.verified_timeout_secs = t; }
        if timeout_secs.is_some() || verified_timeout_secs.is_some() {
            c.source = ConfigSource::External;
        }
        self.persist();
        true
    }

    pub fn set_verified_proxy_mode(&mut self, chain_id: u64, mode: VerifiedProxyMode) -> std::result::Result<(), String> {
        let Some(c) = self.chains.get_mut(&chain_id) else {
            return Err(format!("no configuration for chain {chain_id}"));
        };
        let previous = c.verified_proxy_mode;
        c.verified_proxy_mode = mode;
        if let Err(e) = c.validate() {
            c.verified_proxy_mode = previous;
            return Err(e);
        }
        c.source = ConfigSource::External;
        self.persist();
        Ok(())
    }

    pub fn get_chain_config(&self, chain_id: u64) -> Option<&ChainConfig> {
        self.chains.get(&chain_id)
    }

    /// How a call to `method` on `chain_id` is routed, without making it — the same decision
    /// [`Self::rpc_call_routed`] makes, so a caller holding an answer can label it rather than
    /// infer "verified" from the mode and badge a forwarded fee figure as proven.
    pub fn route_of(&self, chain_id: u64, method: &str) -> Option<VerifiedClass> {
        match self.chains.get(&chain_id) {
            Some(c) if c.verified_proxy_mode == VerifiedProxyMode::Required => {
                Some(verified_class(method))
            }
            _ => None,
        }
    }

    pub fn remove_chain_config(&mut self, chain_id: u64) -> bool {
        let removed = self.chains.remove(&chain_id).is_some();
        self.persist();
        removed
    }

    pub fn list_chains(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self.chains.keys().copied().collect();
        v.sort_unstable();
        v
    }

    /// Build a fail-closed client for `chain_id` and return it with the endpoint.
    fn client_for(&self, chain_id: u64) -> Result<(reqwest::blocking::Client, String)> {
        let c = self.chains.get(&chain_id).ok_or(RpcError::UnknownChain(chain_id))?;
        let pc = ProxyConfig::new(c.proxy.clone(), c.proxy_required, c.timeout_secs);
        let client = build_client(&pc).map_err(|e| RpcError::Proxy(e.to_string()))?;
        Ok((client, c.endpoint.clone()))
    }

    /// Issue a raw JSON-RPC call and return the `result` value (or an error).
    pub fn rpc_call(&self, chain_id: u64, method: &str, params: Value) -> Result<Value> {
        self.rpc_call_routed(chain_id, method, params).map(|(v, _)| v)
    }

    /// The routing decision, in the one place every typed method already funnels through.
    /// Returns the value and whether it is proof-backed, so a caller can report which it got
    /// rather than inferring it from the mode.
    pub fn rpc_call_routed(&self, chain_id: u64, method: &str, params: Value) -> Result<(Value, Option<VerifiedClass>)> {
        self.rpc_call_routed_within(chain_id, method, params, None)
    }

    /// [`Self::rpc_call_routed`] bounded by the CALLER's own wall budget. It may only shorten
    /// what the chain already permits, never lengthen it: `chains.json` is shared with other
    /// wallets on this device, so a caller must not be able to widen its own exposure.
    pub fn rpc_call_routed_within(
        &self,
        chain_id: u64,
        method: &str,
        params: Value,
        deadline: Option<Duration>,
    ) -> Result<(Value, Option<VerifiedClass>)> {
        let cfg = self.chains.get(&chain_id).ok_or(RpcError::UnknownChain(chain_id))?;
        if cfg.verified_proxy_mode == VerifiedProxyMode::Required {
            let router = self
                .verified
                .as_ref()
                .ok_or_else(|| RpcError::VerifiedProxy("no verified proxy is wired up".into()))?;
            let coerced = verified_params(method, &params);
            let configured = verified_budget(cfg.verified_timeout_secs);
            let budget = deadline.map_or(configured, |d| d.min(configured));
            if budget < MIN_BUDGET {
                return Err(RpcError::Timeout {
                    budget_ms: budget.as_millis(),
                    detail: "too little time left to ask the verified proxy".into(),
                });
            }
            // REFUSE on failure. Falling back would answer a request for a verified number
            // with an unverified one, which is worse than no answer at all.
            let v = router
                .call(chain_id, method, &coerced, budget)
                .map_err(RpcError::VerifiedProxy)?;
            return Ok((normalize_verified_result(v), Some(verified_class(method))));
        }
        let bound = deadline.map(|d| d.min(configured_wall(cfg.timeout_secs)));
        if let Some(b) = bound {
            if b < MIN_BUDGET {
                return Err(RpcError::Timeout {
                    budget_ms: b.as_millis(),
                    detail: format!("too little time left to ask chain {chain_id}"),
                });
            }
        }
        let (client, endpoint) = self.client_for(chain_id)?;
        // `bound` is None on the untouched path, so the request carries no per-request timeout
        // and the client-level one governs exactly as it did before this parameter existed.
        Self::post_rpc(&client, &endpoint, method, params, bound).map(|v| (v, None))
    }

    /// Like [`Self::rpc_call`] but POSTs to an explicit `url` instead of the
    /// chain's configured endpoint, while still using `chain_id`'s fail-closed
    /// proxied client. For off-chain JSON-RPC services tied to a chain — e.g. an
    /// ERC-4337 bundler (`eth_sendUserOperation`) — so they too go through
    /// net-proxy (a private send must not leak the user's IP to the bundler).
    pub fn rpc_call_url(&self, chain_id: u64, url: &str, method: &str, params: Value) -> Result<Value> {
        // The line: a chain set to `required` asked for proof-backed reads, and an arbitrary url
        // proves nothing — so exactly the methods the verified route PROVES are refused here.
        // Submissions and bundler methods are `Proxied` even on the verified leg, so allowing
        // them costs the guarantee nothing and keeps the one real caller working.
        if self.route_of(chain_id, method) == Some(VerifiedClass::Verified) {
            return Err(RpcError::VerifiedBypass(method.to_string()));
        }
        // Build the client from the chain's proxy config; ignore its endpoint.
        let (client, _endpoint) = self.client_for(chain_id)?;
        Self::post_rpc(&client, url, method, params, None)
    }

    /// POST a JSON-RPC request to `url` with `client` and unwrap the `result`.
    /// `budget`, when given, is ONE total deadline across headers AND body — unlike the
    /// client-level timeout, which arms a fresh timer for each phase. MEASURED against a node
    /// stalling 600ms then 600ms: `RequestBuilder::timeout(1000ms)` fails at 1.004s where
    /// `ClientBuilder::timeout(1000ms)` succeeds at 1.211s. So it is passed WHOLE — halving it
    /// per phase, which reads like the obvious thing to do, would buy half what was asked for.
    fn post_rpc(
        client: &reqwest::blocking::Client,
        url: &str,
        method: &str,
        params: Value,
        budget: Option<Duration>,
    ) -> Result<Value> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let mut req = client.post(url).json(&body);
        if let Some(b) = budget {
            req = req.timeout(b);
        }
        let resp = req.send().map_err(|e| http_error(e, budget))?;
        let v: Value = resp.json().map_err(|e| http_error(e, budget))?;
        if let Some(err) = v.get("error") {
            let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
            let message = err.get("message").and_then(Value::as_str).unwrap_or("").to_string();
            return Err(RpcError::Rpc { code, message });
        }
        v.get("result").cloned().ok_or_else(|| RpcError::Parse("response had no `result`".into()))
    }

    fn result_str(&self, chain_id: u64, method: &str, params: Value) -> Result<String> {
        let v = self.rpc_call(chain_id, method, params)?;
        match v {
            Value::String(s) => Ok(s),
            other => Ok(other.to_string()),
        }
    }

    // ── Typed helpers (all keyed by chain_id) ─────────────────────────────────

    /// `eth_chainId` round-trip; returns the node's chain id as a decimal.
    pub fn verify_chain_id(&self, chain_id: u64) -> Result<u64> {
        let s = self.result_str(chain_id, methods::CHAIN_ID, json!([]))?;
        parse_hex_u64(&s).ok_or_else(|| RpcError::Parse(format!("bad chainId: {s}")))
    }

    pub fn block_number(&self, chain_id: u64) -> Result<String> {
        self.result_str(chain_id, methods::BLOCK_NUMBER, json!([]))
    }

    pub fn get_balance(&self, chain_id: u64, address: &str) -> Result<String> {
        self.result_str(chain_id, methods::GET_BALANCE, json!([address, "latest"]))
    }

    /// `eth_call`; `call` is a `{to, data, ...}` object (used for ERC20 reads).
    pub fn call(&self, chain_id: u64, call: Value) -> Result<String> {
        self.result_str(chain_id, methods::CALL, json!([call, "latest"]))
    }

    /// [`Self::call`] bounded by the caller's own wall budget. Additive rather than widening
    /// the existing signature — the choice `raw_rpc_url` made beside `raw_rpc`.
    pub fn call_within(&self, chain_id: u64, call: Value, deadline: Option<Duration>)
        -> Result<String>
    {
        let (v, _) = self.rpc_call_routed_within(chain_id, methods::CALL, json!([call, "latest"]), deadline)?;
        Ok(match v {
            Value::String(s) => s,
            other => other.to_string(),
        })
    }

    pub fn get_transaction_count(&self, chain_id: u64, address: &str) -> Result<String> {
        self.result_str(chain_id, methods::GET_TRANSACTION_COUNT, json!([address, "pending"]))
    }

    pub fn gas_price(&self, chain_id: u64) -> Result<String> {
        self.result_str(chain_id, methods::GAS_PRICE, json!([]))
    }

    pub fn fee_history(&self, chain_id: u64, blocks: u64, reward_percentiles: Value) -> Result<Value> {
        let block_hex = format!("0x{blocks:x}");
        self.rpc_call(chain_id, methods::FEE_HISTORY, json!([block_hex, "latest", reward_percentiles]))
    }

    pub fn estimate_gas(&self, chain_id: u64, tx: Value) -> Result<String> {
        self.result_str(chain_id, methods::ESTIMATE_GAS, json!([tx]))
    }

    pub fn send_raw_transaction(&self, chain_id: u64, raw_hex: &str) -> Result<String> {
        self.result_str(chain_id, methods::SEND_RAW_TRANSACTION, json!([raw_hex]))
    }

    pub fn get_transaction_receipt(&self, chain_id: u64, hash: &str) -> Result<Value> {
        self.rpc_call(chain_id, methods::GET_TRANSACTION_RECEIPT, json!([hash]))
    }

    pub fn get_transaction_by_hash(&self, chain_id: u64, hash: &str) -> Result<Value> {
        self.rpc_call(chain_id, methods::GET_TRANSACTION_BY_HASH, json!([hash]))
    }
}

impl Default for EthRpc {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_hex_u64(s: &str) -> Option<u64> {
    let s = s.trim();
    let h = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    u64::from_str_radix(h, 16).ok()
}

#[cfg(test)]
mod tests {

    // ── the caller's deadline ────────────────────────────────────────────────────────
    //
    // The defect these exist for: a wallet read under a 3s grant against the 8s socket
    // timeout it had itself seeded, so it gave up ~5s before its own dependency would have
    // and rendered a Debug blob where this module's sentence was about to arrive.

    #[test]
    fn no_deadline_means_exactly_what_every_caller_got_before_the_parameter_existed() {
        // The whole compatibility story. An un-rebuilt consumer sends no slot at all and a
        // rebuilt one that has no budget sends null; both must reach the same code path.
        assert_eq!(caller_deadline(None), None);
        assert_eq!(caller_deadline(Some(0)), None, "0 is how this stack spells `leave it`");
        assert_eq!(caller_deadline(Some(-1)), None);
        assert_eq!(caller_deadline(Some(2500)), Some(Duration::from_millis(2500)));
    }

    #[test]
    fn a_caller_may_shorten_the_configured_bound_but_never_widen_it() {
        // chains.json is shared with every other wallet on the device. A caller that could
        // lengthen its own bound could hold this module's thread for as long as it liked.
        let configured = configured_wall(8);
        let shorter = Duration::from_millis(2_000);
        let longer = Duration::from_secs(600);
        assert_eq!(shorter.min(configured), shorter, "a shorter deadline wins");
        assert_eq!(longer.min(configured), configured, "a longer one does not");
    }

    #[test]
    fn the_configured_wall_is_twice_the_seconds_because_the_client_times_each_phase() {
        // MEASURED, not inferred: ClientBuilder::timeout(1000ms) against a node stalling
        // 600ms on headers and then 600ms on the body SUCCEEDS at 1.211s. A grant sized
        // against the bare `timeoutSecs` would be half of what the endpoint may actually take.
        assert_eq!(configured_wall(8), Duration::from_secs(16));
        assert_eq!(configured_wall(0), Duration::from_secs(60), "0 is reqwest's own 30s default");
    }

    #[test]
    fn a_deadline_too_small_to_buy_a_handshake_is_refused_rather_than_rounded_up() {
        // Rounding UP would recreate the original defect in miniature: the callee outliving
        // the caller that set the bound, and answering into a request nobody is waiting on.
        assert!(Duration::from_millis(10) < MIN_BUDGET);
        assert!(MIN_BUDGET <= Duration::from_millis(50));
    }

    #[test]
    fn a_timeout_is_its_own_kind_because_a_caller_acts_differently_on_it() {
        // reqwest reports both through one type, and the prose does not separate them: a
        // body-phase expiry reads "error decoding response body" exactly like a malformed
        // body, and a headers-phase expiry reads like a refused connection.
        let slow = RpcError::Timeout { budget_ms: 3000, detail: "stalled".into() };
        let down = RpcError::Http("error sending request for url (...)".into());
        assert_eq!(slow.code(), "timeout");
        assert_eq!(down.code(), "http");
        assert!(slow.to_string().contains("3000ms"), "the budget is named: {slow}");
    }

    #[test]
    fn every_refusal_carries_a_code_that_is_not_the_message() {
        for e in [
            RpcError::UnknownChain(1),
            RpcError::Proxy("p".into()),
            RpcError::Http("h".into()),
            RpcError::Timeout { budget_ms: 1, detail: "d".into() },
            RpcError::Rpc { code: -32000, message: "m".into() },
            RpcError::Parse("p".into()),
            RpcError::VerifiedProxy("v".into()),
            RpcError::VerifiedBypass("b".into()),
        ] {
            assert!(!e.code().is_empty(), "{e} has no code");
            assert!(!e.code().contains(' '), "a code is a token, not prose: {}", e.code());
        }
    }
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn cfg(endpoint: &str) -> ChainConfig {
        ChainConfig { endpoint: endpoint.into(), proxy: None, proxy_required: false, timeout_secs: 5,
            verified_proxy_mode: VerifiedProxyMode::Off, verified_timeout_secs: 15,
            source: ConfigSource::External }
    }

    #[test]
    fn config_store_roundtrip_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chains.json");
        {
            let mut r = EthRpc::with_store(path.clone());
            r.set_chain_config(1, cfg("https://eth.example")).unwrap();
            r.set_chain_config(10, cfg("https://op.example")).unwrap();
            assert_eq!(r.list_chains(), vec![1, 10]);
            assert!(r.remove_chain_config(10));
            assert_eq!(r.list_chains(), vec![1]);
        }
        // Reopen: chain 1 survives, chain 10 is gone.
        let r2 = EthRpc::with_store(path);
        assert_eq!(r2.list_chains(), vec![1]);
        assert_eq!(r2.get_chain_config(1).unwrap().endpoint, "https://eth.example");
    }

    #[test]
    fn camelcase_proxy_required_is_honored() {
        // Regression: the wallet backend sends camelCase; if this field doesn't
        // map, fail-closed silently fails OPEN.
        let c: ChainConfig = serde_json::from_str(r#"{"endpoint":"x","proxyRequired":true}"#).unwrap();
        assert!(c.proxy_required);
    }

    #[test]
    fn unknown_chain_errors() {
        let r = EthRpc::new();
        assert!(matches!(r.get_balance(999, "0x0"), Err(RpcError::UnknownChain(999))));
    }

    #[test]
    fn fail_closed_when_proxy_required_but_unset() {
        let mut r = EthRpc::new();
        r.set_chain_config(
            1,
            ChainConfig {
                endpoint: "https://eth.example".into(),
                proxy: None,
                proxy_required: true, // requires a proxy, but none configured
                timeout_secs: 5, verified_proxy_mode: VerifiedProxyMode::Off, verified_timeout_secs: 15,
                source: ConfigSource::External,
            },
        )
        .unwrap();
        // Must refuse via the proxy chokepoint — no request is attempted.
        match r.get_balance(1, "0x0000000000000000000000000000000000000000") {
            Err(RpcError::Proxy(_)) => {}
            other => panic!("expected fail-closed Proxy error, got {other:?}"),
        }
    }

    /// Minimal one-shot HTTP server returning a canned JSON-RPC body.
    fn mock_node(body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn parses_get_balance_against_mock_node() {
        let url = mock_node(r#"{"jsonrpc":"2.0","id":1,"result":"0x1234"}"#);
        let mut r = EthRpc::new();
        r.set_chain_config(1, cfg(&url)).unwrap();
        let bal = r.get_balance(1, "0x0000000000000000000000000000000000000000").unwrap();
        assert_eq!(bal, "0x1234");
    }

    #[test]
    fn verify_chain_id_decodes_hex() {
        let url = mock_node(r#"{"jsonrpc":"2.0","id":1,"result":"0xa"}"#);
        let mut r = EthRpc::new();
        r.set_chain_config(10, cfg(&url)).unwrap();
        assert_eq!(r.verify_chain_id(10).unwrap(), 10);
    }

    #[test]
    fn a_transport_failure_names_its_cause_and_not_just_the_stage() {
        // Nothing listens on port 1, so this fails at connect.
        let mut r = EthRpc::new();
        r.set_chain_config(1, cfg("http://127.0.0.1:1")).unwrap();
        let err = r.verify_chain_id(1).unwrap_err().to_string();
        assert!(err.starts_with("http: error sending request"), "got {err}");
        // reqwest's own Display ends at the url, so everything past it is the cause chain.
        // Without it a refused connection and an expired timeout are the same string, which is
        // what made an unreachable node undiagnosable from a CI log.
        let after_url = err.split_once(')').map(|(_, rest)| rest).unwrap_or("");
        assert!(after_url.contains(':'), "no cause chain in {err}");
    }

    #[test]
    fn surfaces_rpc_error() {
        let url = mock_node(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"boom"}}"#);
        let mut r = EthRpc::new();
        r.set_chain_config(1, cfg(&url)).unwrap();
        match r.gas_price(1) {
            Err(RpcError::Rpc { code, message }) => {
                assert_eq!(code, -32000);
                assert_eq!(message, "boom");
            }
            other => panic!("expected Rpc error, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod verified_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Records what the verified leg was asked for, so the coercion is asserted ON THE WIRE
    /// rather than by re-reading the function that performs it.
    struct SpyRouter {
        seen: Mutex<Vec<(String, Value)>>,
        budget: Mutex<Option<Duration>>,
        answer: std::result::Result<Value, String>,
    }
    impl SpyRouter {
        fn new(answer: std::result::Result<Value, String>) -> Arc<Self> {
            Arc::new(Self { seen: Mutex::new(vec![]), budget: Mutex::new(None), answer })
        }
        fn ok(v: Value) -> Arc<Self> { Self::new(Ok(v)) }
        fn failing() -> Arc<Self> { Self::new(Err("proxy is not running".into())) }
        fn last(&self) -> (String, Value) { self.seen.lock().unwrap().last().cloned().unwrap() }
        fn count(&self) -> usize { self.seen.lock().unwrap().len() }
        fn budget(&self) -> Option<Duration> { *self.budget.lock().unwrap() }
    }
    impl VerifiedRouter for SpyRouter {
        fn call(&self, _c: u64, m: &str, p: &Value, budget: Duration)
            -> std::result::Result<Value, String>
        {
            self.seen.lock().unwrap().push((m.to_string(), p.clone()));
            *self.budget.lock().unwrap() = Some(budget);
            self.answer.clone()
        }
    }

    fn cfg(mode: VerifiedProxyMode) -> ChainConfig {
        ChainConfig {
            // Deliberately unreachable: if routing ever falls through to the direct path the
            // test fails, rather than quietly passing against a real node.
            endpoint: "http://127.0.0.1:1/never".into(),
            proxy: None, proxy_required: false, timeout_secs: 1,
            verified_proxy_mode: mode, verified_timeout_secs: 1, source: ConfigSource::External,
        }
    }
    fn verified_rpc(router: Arc<SpyRouter>) -> EthRpc {
        let mut r = EthRpc::new();
        r.set_chain_config(1, cfg(VerifiedProxyMode::Required)).unwrap();
        r.set_verified_router(router);
        r
    }

    #[test]
    fn fee_history_block_count_becomes_a_number_on_the_verified_leg_only() {
        let spy = SpyRouter::ok(json!({ "baseFeePerGas": ["0x1"] }));
        let r = verified_rpc(spy.clone());
        r.fee_history(1, 4, json!([25])).unwrap();
        let (m, p) = spy.last();
        assert_eq!(m, "eth_feeHistory");
        assert_eq!(p[0], json!(4), "a hex blockCount returns success with EMPTY arrays");
        // The spec-conformant hex is untouched off the verified leg: verified_params is the
        // ONLY place this differs, so a real node still gets what the spec says.
        assert_eq!(verified_params("eth_getBalance", &json!(["0xa", "latest"]))[1], json!("latest"));
    }

    #[test]
    fn pending_becomes_latest_because_pending_is_unverifiable() {
        let spy = SpyRouter::ok(json!("0x7"));
        let r = verified_rpc(spy.clone());
        r.get_transaction_count(1, "0xabc").unwrap();
        assert_eq!(spy.last().1[1], json!("latest"), "a light client has no header for `pending`");
    }

    #[test]
    fn estimate_gas_and_call_gain_the_upstream_third_parameter() {
        let spy = SpyRouter::ok(json!("0x5208"));
        let r = verified_rpc(spy.clone());
        r.estimate_gas(1, json!({ "to": "0xabc" })).unwrap();
        let (_, p) = spy.last();
        assert_eq!(p.as_array().unwrap().len(), 3, "a one-arg estimate_gas is refused outright");
        assert_eq!(p[2], json!(false));
        r.call(1, json!({ "to": "0xabc" })).unwrap();
        assert_eq!(spy.last().1.as_array().unwrap().len(), 3);
    }

    #[test]
    fn a_failing_verified_leg_refuses_and_never_falls_back() {
        let spy = SpyRouter::failing();
        let r = verified_rpc(spy.clone());
        let e = r.get_balance(1, "0xabc").unwrap_err();
        assert!(matches!(e, RpcError::VerifiedProxy(_)), "got {e}");
        assert!(e.to_string().contains("proxy is not running"));
        assert_eq!(spy.count(), 1, "one attempt, and no retry against the endpoint");
    }

    #[test]
    fn a_byte_array_result_is_normalised_back_to_a_hex_string() {
        // eth_call through the proxy answers a BYTE ARRAY where a node answers "0x...".
        // Measured on mainnet: symbol() on WETH came back as [0,0,...,87,69,84,72,...].
        let spy = SpyRouter::ok(json!([0, 32, 87, 69, 84, 72]));
        let r = verified_rpc(spy);
        assert_eq!(r.call(1, json!({ "to": "0xabc" })).unwrap(), "0x00205745544 8".replace(" ", ""));
    }

    #[test]
    fn a_bare_number_result_is_normalised_back_to_a_hex_quantity() {
        // eth_blockNumber answers a JSON number through the proxy and a hex string directly.
        let spy = SpyRouter::ok(json!(11579774u64));
        let r = verified_rpc(spy);
        assert_eq!(r.block_number(1).unwrap(), "0xb0b17e");
    }

    #[test]
    fn a_result_already_in_wire_form_is_left_alone() {
        let spy = SpyRouter::ok(json!("0x1afd6402e6259dc342259"));
        let r = verified_rpc(spy);
        assert_eq!(r.get_balance(1, "0xabc").unwrap(), "0x1afd6402e6259dc342259");
        // …and a structured reply keeps its shape: feeHistory's arrays hold hex STRINGS, so
        // the byte-array rule must not fire on them.
        let spy2 = SpyRouter::ok(json!({ "baseFeePerGas": ["0x59bceec"], "oldestBlock": "0x1" }));
        let r2 = verified_rpc(spy2);
        let v = r2.fee_history(1, 4, json!([50])).unwrap();
        assert_eq!(v["baseFeePerGas"][0], json!("0x59bceec"));
    }

    #[test]
    fn only_proof_backed_reads_are_classed_verified() {
        for m in ["eth_getBalance", "eth_getTransactionCount", "eth_getCode",
                  "eth_getStorageAt", "eth_call"] {
            assert_eq!(verified_class(m), VerifiedClass::Verified, "{m}");
        }
        // A light client cannot prove a fee oracle's opinion or that a broadcast landed.
        for m in ["eth_gasPrice", "eth_maxPriorityFeePerGas", "eth_feeHistory",
                  "eth_estimateGas", "eth_sendRawTransaction", "eth_blockNumber"] {
            assert_eq!(verified_class(m), VerifiedClass::Proxied, "{m}");
        }
    }

    #[test]
    fn off_mode_never_touches_the_router() {
        let spy = SpyRouter::ok(json!("0x1"));
        let mut r = EthRpc::new();
        r.set_chain_config(1, cfg(VerifiedProxyMode::Off)).unwrap();
        r.set_verified_router(spy.clone());
        let _ = r.get_balance(1, "0xabc"); // fails on the dead endpoint, which is the point
        assert_eq!(spy.count(), 0, "off mode must not route");
    }

    #[test]
    fn the_configured_verified_timeout_is_what_the_verified_leg_is_given() {
        // It was persisted and read back but never reached the router, so every verified call
        // used the router's hard-coded 15s and a user lowering it was silently ignored.
        let spy = SpyRouter::ok(json!("0x1"));
        let mut r = verified_rpc(spy.clone());
        r.get_balance(1, "0xabc").unwrap();
        assert_eq!(spy.budget(), Some(Duration::from_secs(1)), "cfg() configures 1s");

        // And a later change is honoured on the very next call, with no cached copy to go stale.
        assert!(r.patch_chain_transport(1, None, Some(25)));
        r.get_balance(1, "0xabc").unwrap();
        assert_eq!(spy.budget(), Some(Duration::from_secs(25)));
    }

    #[test]
    fn an_unusable_verified_timeout_is_clamped_rather_than_honoured() {
        assert_eq!(verified_budget(0), Duration::from_secs(1), "0 would time out instantly");
        assert_eq!(verified_budget(9_999), Duration::from_secs(60));
        assert_eq!(verified_budget(default_verified_timeout()), Duration::from_secs(15));
    }

    #[test]
    fn a_socks_proxy_and_verified_routing_cannot_both_be_required() {
        let mut r = EthRpc::new();
        let bad = ChainConfig {
            endpoint: "https://x".into(),
            proxy: Some("socks5h://127.0.0.1:9050".into()),
            proxy_required: true, timeout_secs: 8,
            verified_proxy_mode: VerifiedProxyMode::Required, verified_timeout_secs: 15,
            source: ConfigSource::External,
        };
        assert!(bad.validate().is_err());
        assert!(r.set_chain_config(1, bad).is_err(), "the store must refuse the contradiction");
        assert!(r.get_chain_config(1).is_none());
    }

    #[test]
    fn ensure_seeds_an_absent_chain_but_never_clobbers_a_configured_endpoint() {
        let mut r = EthRpc::new();
        let mine = cfg(VerifiedProxyMode::Off);
        assert_eq!(r.ensure_chain_config(1, &mine), vec!["*".to_string()]);
        let theirs = ChainConfig { endpoint: "https://theirs".into(), ..mine.clone() };
        assert!(r.ensure_chain_config(1, &theirs).is_empty(), "an existing endpoint is the user's");
        assert_eq!(r.get_chain_config(1).unwrap().endpoint, "http://127.0.0.1:1/never");
    }

    #[test]
    fn patch_rewrites_the_timeouts_we_own_and_nothing_else() {
        let mut r = EthRpc::new();
        let mut c = cfg(VerifiedProxyMode::Off);
        c.timeout_secs = 30;
        c.endpoint = "https://mine".into();
        r.set_chain_config(1, c).unwrap();
        assert!(r.patch_chain_transport(1, Some(8), Some(15)));
        let got = r.get_chain_config(1).unwrap();
        assert_eq!((got.timeout_secs, got.verified_timeout_secs), (8, 15));
        assert_eq!(got.endpoint, "https://mine", "the user's endpoint is untouched");
        assert!(!r.patch_chain_transport(999, Some(8), None));
    }

    #[test]
    fn patching_the_endpoint_leaves_the_verified_settings_a_whole_set_would_have_reset() {
        let mut r = EthRpc::new();
        let mut c = cfg(VerifiedProxyMode::Required);
        c.endpoint = "https://old".into();
        c.verified_timeout_secs = 20;
        r.set_chain_config(1, c).unwrap();

        assert!(r.patch_chain_endpoint(1, "  https://new  "));
        let got = r.get_chain_config(1).unwrap();
        assert_eq!(got.endpoint, "https://new", "trimmed and written");
        assert_eq!(got.verified_proxy_mode, VerifiedProxyMode::Required, "set_chain_config resets this");
        assert_eq!(got.verified_timeout_secs, 20);

        // An absent chain is created with defaults rather than refused.
        assert!(r.patch_chain_endpoint(42, "https://fresh"));
        assert_eq!(r.get_chain_config(42).unwrap().verified_proxy_mode, VerifiedProxyMode::Off);
        // An empty endpoint is not a way to blank one.
        assert!(!r.patch_chain_endpoint(1, "   "));
        assert_eq!(r.get_chain_config(1).unwrap().endpoint, "https://new");
    }

    /// The measured repro: the user enables verified mode for mainnet, and on the next app
    /// start the sibling wallet pushes its own chain config — which predates verified routing
    /// and carries no `verifiedProxyMode`. A whole-record write read that silence as "off",
    /// and every later balance read went out in the clear with the UI honestly saying "off".
    #[test]
    fn a_sibling_wallet_omitting_verified_proxy_mode_does_not_turn_verified_mode_off() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chains.json");
        {
            let mut r = EthRpc::with_store(path.clone());
            let mut c = cfg(VerifiedProxyMode::Required);
            c.verified_timeout_secs = 20;
            r.set_chain_config(1, c).unwrap();
        }

        // Verbatim from wallet-backend-module's `eth_rpc_config` (config.rs:130-138).
        let sibling = r#"{"endpoint":"https://eth.llamarpc.com","proxy":null,
                          "proxyRequired":false,"timeoutSecs":30}"#;
        let mut r = EthRpc::with_store(path);
        r.apply_chain_config(1, serde_json::from_str(sibling).unwrap()).unwrap();

        let got = r.get_chain_config(1).unwrap();
        assert_eq!(got.endpoint, "https://eth.llamarpc.com", "the sibling still owns the endpoint");
        assert_eq!(got.timeout_secs, 30);
        assert_eq!(got.verified_proxy_mode, VerifiedProxyMode::Required,
                   "an omitted key must not revoke a security setting the user turned on");
        assert_eq!(got.verified_timeout_secs, 20, "and neither may it reset the verified timeout");
    }

    #[test]
    fn only_an_explicit_off_lowers_the_mode() {
        let mut r = EthRpc::new();
        r.set_chain_config(1, cfg(VerifiedProxyMode::Required)).unwrap();
        let wire = r#"{"endpoint":"https://x","verifiedProxyMode":"off","verifiedTimeoutSecs":9}"#;
        r.apply_chain_config(1, serde_json::from_str(wire).unwrap()).unwrap();
        let got = r.get_chain_config(1).unwrap();
        assert_eq!(got.verified_proxy_mode, VerifiedProxyMode::Off, "an explicit off is honoured");
        assert_eq!(got.verified_timeout_secs, 9);

        // A brand-new chain has nothing to preserve, so the omitted fields take the defaults.
        r.apply_chain_config(7, serde_json::from_str(r#"{"endpoint":"https://y"}"#).unwrap()).unwrap();
        let fresh = r.get_chain_config(7).unwrap();
        assert_eq!(fresh.verified_proxy_mode, VerifiedProxyMode::Off);
        assert_eq!(fresh.verified_timeout_secs, default_verified_timeout());
    }

    #[test]
    fn a_socks_requirement_arriving_later_is_refused_rather_than_dropping_the_mode() {
        let mut r = EthRpc::new();
        r.set_chain_config(1, cfg(VerifiedProxyMode::Required)).unwrap();
        let wire = r#"{"endpoint":"https://x","proxy":"socks5h://127.0.0.1:9050","proxyRequired":true}"#;
        assert!(r.apply_chain_config(1, serde_json::from_str(wire).unwrap()).is_err(),
                "resolving the contradiction silently is what dropped the mode before");
        assert_eq!(r.get_chain_config(1).unwrap().verified_proxy_mode, VerifiedProxyMode::Required);
    }

    #[test]
    fn the_route_label_separates_a_proven_read_from_a_forwarded_one() {
        let r = verified_rpc(SpyRouter::ok(json!("0x1")));
        assert_eq!(route_label(r.route_of(1, methods::GET_BALANCE)), "verified");
        // A receipt and a fee figure come from the proxy's own execution provider, on trust.
        assert_eq!(route_label(r.route_of(1, methods::GET_TRANSACTION_RECEIPT)), "proxied");
        assert_eq!(route_label(r.route_of(1, methods::GAS_PRICE)), "proxied");

        let mut off = EthRpc::new();
        off.set_chain_config(1, cfg(VerifiedProxyMode::Off)).unwrap();
        assert_eq!(route_label(off.route_of(1, methods::GET_BALANCE)), "direct");
        assert_eq!(route_label(off.route_of(999, methods::GET_BALANCE)), "direct");
    }

    #[test]
    fn the_route_a_caller_labels_with_is_the_one_the_call_actually_took() {
        let r = verified_rpc(SpyRouter::ok(json!("0x1")));
        for m in [methods::GET_BALANCE, methods::GAS_PRICE, methods::SEND_RAW_TRANSACTION] {
            let (_, class) = r.rpc_call_routed(1, m, json!([])).unwrap();
            assert_eq!(class, r.route_of(1, m), "{m}");
        }
    }

    #[test]
    fn the_url_escape_hatch_refuses_a_proof_backed_read_on_a_verified_chain() {
        let spy = SpyRouter::ok(json!("0x1"));
        let r = verified_rpc(spy.clone());
        for m in [methods::GET_BALANCE, methods::CALL, methods::GET_TRANSACTION_COUNT,
                  "eth_getCode", "eth_getStorageAt"] {
            match r.rpc_call_url(1, "https://any-node.example", m, json!([])) {
                Err(RpcError::VerifiedBypass(got)) => assert_eq!(got, m),
                other => panic!("expected a refusal for {m}, got {other:?}"),
            }
        }
        assert_eq!(spy.count(), 0, "refused outright — not quietly rerouted through the proxy");
    }

    #[test]
    fn the_url_escape_hatch_still_submits_on_a_verified_chain() {
        // railgun's only use: eth_sendUserOperation to a bundler. It is `Proxied` even on the
        // verified leg, so refusing it would buy the guarantee nothing and break the caller.
        // The url is dead on purpose: reaching a transport error proves the gate let it past.
        let r = verified_rpc(SpyRouter::ok(json!("0x1")));
        for m in ["eth_sendUserOperation", methods::SEND_RAW_TRANSACTION] {
            match r.rpc_call_url(1, "http://127.0.0.1:1/bundler", m, json!([])) {
                Err(RpcError::Http(_)) => {}
                other => panic!("expected {m} to be attempted, got {other:?}"),
            }
        }
        // And an `off` chain is unchanged: the hatch gates on the mode, not on the method.
        let mut off = EthRpc::new();
        off.set_chain_config(1, cfg(VerifiedProxyMode::Off)).unwrap();
        match off.rpc_call_url(1, "http://127.0.0.1:1/x", methods::GET_BALANCE, json!([])) {
            Err(RpcError::Http(_)) => {}
            other => panic!("expected the call to be attempted, got {other:?}"),
        }
    }

    #[test]
    fn switching_mode_on_a_socks_required_chain_is_refused_and_rolled_back() {
        let mut r = EthRpc::new();
        let mut c = cfg(VerifiedProxyMode::Off);
        c.proxy = Some("socks5h://1".into());
        c.proxy_required = true;
        r.set_chain_config(1, c).unwrap();
        assert!(r.set_verified_proxy_mode(1, VerifiedProxyMode::Required).is_err());
        assert_eq!(r.get_chain_config(1).unwrap().verified_proxy_mode, VerifiedProxyMode::Off,
                   "a refused switch must not leave the mode changed");
    }
}

/// The "ask, then initialize" convention: `config_status` reports, `init_defaults` seeds, and
/// neither may lower a setting a user chose.
#[cfg(test)]
mod defaults_tests {
    use super::*;

    fn seeded_map(v: Vec<(u64, Vec<String>)>) -> HashMap<u64, Vec<String>> {
        v.into_iter().collect()
    }

    fn snapshot(r: &EthRpc) -> String {
        let m: std::collections::BTreeMap<u64, String> = r
            .list_chains()
            .into_iter()
            .map(|id| (id, serde_json::to_string(r.get_chain_config(id).unwrap()).unwrap()))
            .collect();
        serde_json::to_string(&m).unwrap()
    }

    #[test]
    fn the_shipped_defaults_cover_the_wallets_three_chains_with_verified_routing_off() {
        let ids: Vec<u64> = DEFAULT_ENDPOINTS.iter().map(|&(id, _)| id).collect();
        assert_eq!(ids, vec![1, 11155111, 560048]);
        for &(_, url) in DEFAULT_ENDPOINTS {
            let c = ChainConfig::builtin(url);
            assert!(url.starts_with("https://"), "{url} must not be plaintext http");
            assert_eq!(c.verified_proxy_mode, VerifiedProxyMode::Off,
                       "verified routing needs an archive node the public defaults are not");
            assert_eq!(c.source, ConfigSource::Builtin);
            assert!(c.proxy.is_none() && !c.proxy_required);
            c.validate().expect("a shipped default must be representable");
        }
    }

    #[test]
    fn initializing_twice_writes_nothing_the_second_time() {
        let mut r = EthRpc::new();
        let first = seeded_map(r.init_defaults());
        for &(id, _) in DEFAULT_ENDPOINTS {
            assert_eq!(first[&id], vec!["*".to_string()], "chain {id} should be seeded whole");
        }
        let after_first = snapshot(&r);

        let second = seeded_map(r.init_defaults());
        for &(id, _) in DEFAULT_ENDPOINTS {
            assert!(second[&id].is_empty(), "chain {id} was rewritten by a second call");
        }
        assert_eq!(snapshot(&r), after_first, "a second init_defaults must not change a byte");
    }

    #[test]
    fn initialization_is_still_a_no_op_after_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chains.json");
        let stored = {
            let mut r = EthRpc::with_store(path.clone());
            r.init_defaults();
            snapshot(&r)
        };
        // Idempotence has to survive a process lifetime, not just one instance.
        let mut r = EthRpc::with_store(path);
        assert!(seeded_map(r.init_defaults()).values().all(|f| f.is_empty()));
        assert_eq!(snapshot(&r), stored);
    }

    #[test]
    fn a_user_endpoint_and_verified_mode_survive_initialization() {
        let mut r = EthRpc::new();
        let mut mine = ChainConfig::builtin("https://my-own-archive.example");
        mine.verified_proxy_mode = VerifiedProxyMode::Required;
        mine.verified_timeout_secs = 42;
        mine.timeout_secs = 3;
        mine.source = ConfigSource::External;
        r.set_chain_config(1, mine).unwrap();

        let seeded = seeded_map(r.init_defaults());
        assert!(seeded[&1].is_empty(), "a configured chain must not be touched");
        assert_eq!(seeded[&11155111], vec!["*".to_string()], "the absent chains still get seeded");

        let got = r.get_chain_config(1).unwrap();
        assert_eq!(got.endpoint, "https://my-own-archive.example");
        assert_eq!(got.verified_proxy_mode, VerifiedProxyMode::Required,
                   "seeding a default must never revoke verified routing the user turned on");
        assert_eq!((got.verified_timeout_secs, got.timeout_secs), (42, 3));
        assert_eq!(got.source, ConfigSource::External);
    }

    #[test]
    fn a_default_fills_an_absent_endpoint_without_touching_the_rest_of_the_record() {
        let mut r = EthRpc::new();
        let mut half = ChainConfig::builtin("");
        half.proxy = Some("socks5h://127.0.0.1:9050".into());
        half.proxy_required = true;
        half.source = ConfigSource::External;
        r.set_chain_config(11155111, half).unwrap();

        let seeded = seeded_map(r.init_defaults());
        assert_eq!(seeded[&11155111], vec!["endpoint".to_string()]);
        let got = r.get_chain_config(11155111).unwrap();
        assert_eq!(got.endpoint, "https://ethereum-sepolia-rpc.publicnode.com");
        assert_eq!(got.proxy.as_deref(), Some("socks5h://127.0.0.1:9050"),
                   "the fail-closed proxy policy is the user's, not ours to reset");
        assert!(got.proxy_required);
    }

    #[test]
    fn a_users_edit_relabels_a_defaulted_chain_as_theirs() {
        let mut r = EthRpc::new();
        r.init_defaults();
        assert_eq!(r.get_chain_config(1).unwrap().source, ConfigSource::Builtin);

        assert!(r.patch_chain_endpoint(1, "https://theirs.example"));
        assert_eq!(r.get_chain_config(1).unwrap().source, ConfigSource::External);
        // Every caller-facing write path promotes, so `default` can only ever mean untouched.
        r.set_verified_proxy_mode(11155111, VerifiedProxyMode::Required).unwrap();
        assert_eq!(r.get_chain_config(11155111).unwrap().source, ConfigSource::External);
        assert!(r.patch_chain_transport(560048, Some(12), None));
        assert_eq!(r.get_chain_config(560048).unwrap().source, ConfigSource::External);
    }

    #[test]
    fn a_caller_cannot_declare_its_own_write_to_be_a_default() {
        let mut r = EthRpc::new();
        let wire = r#"{"endpoint":"https://theirs","source":"default"}"#;
        r.apply_chain_config(1, serde_json::from_str(wire).unwrap()).unwrap();
        assert_eq!(r.get_chain_config(1).unwrap().source, ConfigSource::External,
                   "source is not on ChainConfigWire, so the key is ignored");
        // And so init_defaults leaves the endpoint alone.
        assert!(seeded_map(r.init_defaults())[&1].is_empty());
        assert_eq!(r.get_chain_config(1).unwrap().endpoint, "https://theirs");

        // The glue's ensure_chain_config path parses a bare ChainConfig, which DOES carry
        // `source` so chains.json reads back — from_caller_json is what forces it.
        let cfg = ChainConfig::from_caller_json(wire).unwrap();
        assert_eq!(cfg.source, ConfigSource::External);
        let mut g = EthRpc::new();
        assert_eq!(g.ensure_chain_config(1, &cfg), vec!["*".to_string()]);
        assert_eq!(g.get_chain_config(1).unwrap().source, ConfigSource::External);
        // The point of `source`: config_status must not report a caller's record as built-in.
        let s = g.config_status();
        assert_eq!(s["chains"][0]["source"], json!("external"));
        assert_eq!(s["source"], json!("external"));
    }

    #[test]
    fn a_chains_json_written_before_source_existed_reads_back_as_external() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chains.json");
        std::fs::write(&path, r#"{"1":{"endpoint":"https://legacy","timeoutSecs":30}}"#).unwrap();
        // Fail closed: a record already on disk was written by somebody.
        let r = EthRpc::with_store(path);
        assert_eq!(r.get_chain_config(1).unwrap().source, ConfigSource::External);
    }

    #[test]
    fn an_empty_store_reports_unconfigured_and_still_lists_what_it_could_seed() {
        let s = EthRpc::new().config_status();
        assert_eq!(s["ok"], json!(true));
        assert_eq!(s["state"], json!("unconfigured"));
        assert_eq!(s["source"], json!("none"));
        let rows = s["chains"].as_array().unwrap();
        assert_eq!(rows.len(), DEFAULT_ENDPOINTS.len());
        assert_eq!(rows[0]["chainId"], json!(1));
        assert_eq!(rows[0]["state"], json!("unconfigured"));
        assert_eq!(rows[0]["source"], json!("none"));
        assert!(rows[0].get("endpoint").is_none(), "no record means no endpoint to report");
    }

    #[test]
    fn config_status_separates_a_default_from_a_value_the_user_chose() {
        let mut r = EthRpc::new();
        r.init_defaults();
        let s = r.config_status();
        assert_eq!((&s["state"], &s["source"]), (&json!("configured"), &json!("default")));
        assert_eq!(s["chains"][0]["source"], json!("default"));
        assert_eq!(s["chains"][0]["endpoint"], json!("https://ethereum-rpc.publicnode.com"));
        assert_eq!(s["chains"][0]["verifiedProxyMode"], json!("off"));

        assert!(r.patch_chain_endpoint(1, "https://theirs.example"));
        let s = r.config_status();
        assert_eq!(s["source"], json!("external"), "one external chain makes the roll-up external");
        assert_eq!(s["chains"][0]["source"], json!("external"));
        assert_eq!(s["chains"][1]["source"], json!("default"), "the others are unchanged");
    }

    #[test]
    fn a_chain_outside_the_defaults_is_reported_but_never_seeded() {
        let mut r = EthRpc::new();
        r.set_chain_config(31337, ChainConfig::builtin("http://127.0.0.1:8545")).unwrap();
        r.init_defaults();
        let rows = r.config_status();
        let rows = rows["chains"].as_array().unwrap();
        assert_eq!(rows.len(), DEFAULT_ENDPOINTS.len() + 1);
        assert_eq!(rows[1]["chainId"], json!(31337), "the union is sorted by chainId");
        assert_eq!(rows[1]["state"], json!("configured"));
        assert_eq!(r.get_chain_config(31337).unwrap().endpoint, "http://127.0.0.1:8545");
    }
}

/// The emit decision, exercised at the same seam the glue uses: snapshot, mutate, diff. Every
/// case here is "a consumer is woken" or "a consumer is left alone".
#[cfg(test)]
mod change_tests {
    use super::*;

    fn cfg(mode: VerifiedProxyMode) -> ChainConfig {
        ChainConfig { endpoint: "https://mine".into(), proxy: None, proxy_required: false,
            timeout_secs: 8, verified_proxy_mode: mode, verified_timeout_secs: 15,
            source: ConfigSource::External }
    }

    /// What `EthRpcModuleImpl::write_diffed` does, minus the lock.
    fn diff_of(r: &mut EthRpc, chain: u64, f: impl FnOnce(&mut EthRpc)) -> ChainChange {
        let before = r.get_chain_config(chain).cloned();
        f(r);
        diff_chain(before.as_ref(), r.get_chain_config(chain))
    }

    #[test]
    fn a_new_chain_reports_its_record_but_no_gate_move() {
        let mut r = EthRpc::new();
        let c = diff_of(&mut r, 1, |r| { r.set_chain_config(1, cfg(VerifiedProxyMode::Off)).unwrap(); });
        assert_eq!(c, ChainChange { config: true, mode: None },
                   "a chain arriving off was already effectively off");
    }

    #[test]
    fn rewriting_the_same_record_reports_nothing() {
        let mut r = EthRpc::new();
        r.set_chain_config(1, cfg(VerifiedProxyMode::Off)).unwrap();
        let c = diff_of(&mut r, 1, |r| { r.set_chain_config(1, cfg(VerifiedProxyMode::Off)).unwrap(); });
        assert!(!c.any(), "an identical write must not wake every subscriber");
    }

    #[test]
    fn a_sibling_rewriting_the_same_config_on_every_start_reports_nothing() {
        let mut r = EthRpc::new();
        let wire = r#"{"endpoint":"https://mine","proxyRequired":false,"timeoutSecs":8,
                       "verifiedTimeoutSecs":15}"#;
        r.apply_chain_config(1, serde_json::from_str(wire).unwrap()).unwrap();
        let c = diff_of(&mut r, 1, |r| {
            r.apply_chain_config(1, serde_json::from_str(wire).unwrap()).unwrap();
        });
        assert!(!c.any());
    }

    #[test]
    fn turning_the_gate_on_reports_the_record_and_the_mode() {
        let mut r = EthRpc::new();
        r.set_chain_config(1, cfg(VerifiedProxyMode::Off)).unwrap();
        let c = diff_of(&mut r, 1, |r| {
            r.set_verified_proxy_mode(1, VerifiedProxyMode::Required).unwrap();
        });
        assert_eq!(c, ChainChange { config: true, mode: Some(VerifiedProxyMode::Required) });
        assert_eq!(mode_label(c.mode.unwrap()), "required");
    }

    #[test]
    fn setting_the_mode_to_what_it_already_is_reports_nothing() {
        let mut r = EthRpc::new();
        r.set_chain_config(1, cfg(VerifiedProxyMode::Required)).unwrap();
        let c = diff_of(&mut r, 1, |r| {
            r.set_verified_proxy_mode(1, VerifiedProxyMode::Required).unwrap();
        });
        assert!(!c.any(), "the wallet polls this gate; a no-op set must not look like a flip");
    }

    #[test]
    fn a_refused_mode_switch_reports_nothing() {
        let mut r = EthRpc::new();
        let mut c0 = cfg(VerifiedProxyMode::Off);
        c0.proxy_required = true;
        r.set_chain_config(1, c0).unwrap();
        let c = diff_of(&mut r, 1, |r| {
            assert!(r.set_verified_proxy_mode(1, VerifiedProxyMode::Required).is_err());
        });
        assert!(!c.any(), "a refusal changed nothing, so it announces nothing");
    }

    #[test]
    fn removing_a_required_chain_closes_the_gate() {
        let mut r = EthRpc::new();
        r.set_chain_config(1, cfg(VerifiedProxyMode::Required)).unwrap();
        let c = diff_of(&mut r, 1, |r| { assert!(r.remove_chain_config(1)); });
        assert_eq!(c, ChainChange { config: true, mode: Some(VerifiedProxyMode::Off) },
                   "no config reads as off, and a consumer gating on required must hear it");
    }

    #[test]
    fn removing_an_absent_chain_reports_nothing() {
        let mut r = EthRpc::new();
        let c = diff_of(&mut r, 7, |r| { assert!(!r.remove_chain_config(7)); });
        assert!(!c.any());
    }

    #[test]
    fn patching_an_endpoint_reports_the_record_only_when_it_moved() {
        let mut r = EthRpc::new();
        r.set_chain_config(1, cfg(VerifiedProxyMode::Required)).unwrap();
        let moved = diff_of(&mut r, 1, |r| { assert!(r.patch_chain_endpoint(1, "https://new")); });
        assert_eq!(moved, ChainChange { config: true, mode: None },
                   "an endpoint change does not move the gate");
        let same = diff_of(&mut r, 1, |r| { assert!(r.patch_chain_endpoint(1, "  https://new  ")); });
        assert!(!same.any(), "the trimmed value is what is stored, so this is a no-op");
        let refused = diff_of(&mut r, 1, |r| { assert!(!r.patch_chain_endpoint(1, "   ")); });
        assert!(!refused.any());
    }

    #[test]
    fn patching_the_same_timeouts_reports_nothing() {
        let mut r = EthRpc::new();
        r.set_chain_config(1, cfg(VerifiedProxyMode::Off)).unwrap();
        let same = diff_of(&mut r, 1, |r| { assert!(r.patch_chain_transport(1, Some(8), Some(15))); });
        assert!(!same.any());
        let moved = diff_of(&mut r, 1, |r| { assert!(r.patch_chain_transport(1, Some(9), None)); });
        assert_eq!(moved, ChainChange { config: true, mode: None });
    }

    #[test]
    fn ensure_reports_only_the_chain_it_actually_seeded() {
        let mut r = EthRpc::new();
        let mine = cfg(VerifiedProxyMode::Off);
        let seeded = diff_of(&mut r, 1, |r| { assert!(!r.ensure_chain_config(1, &mine).is_empty()); });
        assert_eq!(seeded, ChainChange { config: true, mode: None });
        let again = diff_of(&mut r, 1, |r| { assert!(r.ensure_chain_config(1, &mine).is_empty()); });
        assert!(!again.any());
    }

    /// `init_defaults` emits per chain off its own per-chain `seeded` list, so this asserts on
    /// that list rather than on a diff.
    #[test]
    fn seeding_defaults_reports_each_chain_once_and_never_again() {
        let mut r = EthRpc::new();
        let first = r.init_defaults();
        assert_eq!(first.iter().filter(|(_, w)| !w.is_empty()).count(), DEFAULT_ENDPOINTS.len());
        let second = r.init_defaults();
        assert!(second.iter().all(|(_, w)| w.is_empty()), "a second call announces nothing");
        for &(id, _) in DEFAULT_ENDPOINTS {
            assert_eq!(r.get_chain_config(id).unwrap().verified_proxy_mode, VerifiedProxyMode::Off,
                       "seeding never moves the gate, so it emits no mode event");
        }
    }

    #[test]
    fn the_record_is_durable_before_the_change_is_decided() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chains.json");
        let mut r = EthRpc::with_store(path.clone());
        r.set_chain_config(1, cfg(VerifiedProxyMode::Off)).unwrap();

        let before = r.get_chain_config(1).cloned();
        r.set_verified_proxy_mode(1, VerifiedProxyMode::Required).unwrap();
        // The glue emits here. Both the store and the on-disk file must already read the new
        // value, or a subscriber calling straight back gets the one it was woken about.
        let on_disk = std::fs::read_to_string(&path).unwrap();
        let change = diff_chain(before.as_ref(), r.get_chain_config(1));
        assert_eq!(change.mode, Some(VerifiedProxyMode::Required));
        assert!(on_disk.contains("\"required\""), "persisted before the event is decided");
    }
}
