//! eth_rpc_module — a proxyable, fail-closed Ethereum JSON-RPC client.
//!
//! Stores per-chain configuration (endpoint + proxy policy) keyed by chainId, so
//! callers route by chainId alone. Every outbound request is built through the
//! single [`proxy`] chokepoint. The crypto-free RPC core (`rpc`), the proxy
//! chokepoint (`proxy`) and the verified-proxy state machine (`verdict`) are plain
//! Rust, unit-tested with `cargo test --no-default-features`; the Logos glue is
//! behind the default `logos_module` feature.

mod proxy;
mod rpc;
mod verdict;

pub use rpc::{caller_deadline, diff_chain, methods, mode_label, route_label, ChainChange, ChainConfig,
              ChainConfigWire, ConfigSource, EthRpc, RpcError, VerifiedClass, VerifiedProxyMode,
              DEFAULT_ENDPOINTS};
pub use verdict::{classify_modules_state, hop_budget, classify_readiness, classify_status, evaluate,
                  evaluate_with, GateCache, GateProbe, Readiness, Verdict, HEALTH_TTL,
                  PROXY_MODULE, READY_TTL};

#[cfg(feature = "logos_module")]
mod glue;
