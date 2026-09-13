# logos-evm-eth-rpc-module

A Logos `core` module (Rust, rust-first cdylib): a **proxyable, fail-closed
Ethereum JSON-RPC client** for the Logos multi-chain EVM wallet.

It stores configuration **per chain** (endpoint + proxy policy), so callers route
by `chainId` alone. Every outbound request is built through a single fail-closed
chokepoint (`src/proxy.rs`): if a chain is configured with `proxyRequired` and no
usable proxy, the request is **refused** rather than sent in the clear. Uses
`reqwest` with `rustls-tls` + `socks` (`socks5h://` resolves DNS through the
proxy — Tor-ready).

## Contract (`EthRpcModule`)

Config: `set_chain_config(chainId, {endpoint, proxy?, proxyRequired?, timeoutSecs?})`,
`get_chain_config`, `remove_chain_config`, `list_chains`. Calls keyed by chainId:
`verify_chain_id`, `block_number`, `get_balance`, `call`, `get_transaction_count`,
`gas_price`, `fee_history`, `estimate_gas`, `send_raw_transaction`,
`get_transaction_receipt`, `get_transaction_by_hash`, `raw_rpc`.

## Events

| event | when |
|---|---|
| `chain_config_changed(chainId)` | that chain's stored record was added, removed or altered |
| `verified_proxy_mode_changed(chainId, mode)` | the verified-proxy gate for that chain moved; `mode` is `"off"` or `"required"`, the value now in force |

**Every method that changes persisted state emits; every reader is silent; and a
call that changes nothing emits nothing** — the decision is a diff of the stored
record either side of the write, never "a setter was called". Re-writing the
config a sibling wallet pushes on every start, or setting the mode to what it
already is, wakes nobody. Both events fire *after* the record is durable, so a
subscriber that reads back on the event cannot see the value it was woken about.

`verified_proxy_mode_changed` is a refinement of `chain_config_changed`, not an
alternative: a mode change emits both. It exists because the mode is what a
wallet **gates** on — `eth_wallet_backend` polled `verified_proxy_status` on
every gate check, and the setting is now edited by a separate app
(`eth_rpc_ui`), so without an event a wallet can only learn of a change by
asking again. The payload carries the direction so the wallet need not re-read
to know which way the gate went; `verified_proxy_status(chainId)` still answers
whether a verified call would *succeed*, which depends on the proxy and not on
this module.

Removing a chain's config reports `"off"`: no configuration is what
`verified_proxy_status` already treats as off, so a consumer gating on
`required` has to hear it.

## Working with no configuration at all

The module ships defaults, so a consumer needs no UI app installed to work. Ask
`config_status()` — `{ ok, state, source, chains }`, where `state` is `unready`
(context not ready — ask again), `unconfigured`, or `configured` — then call
`init_defaults()`, which seeds these per chain and per **field**, only where absent:

| Chain | id | Endpoint |
|---|---|---|
| Ethereum | `1` | `https://ethereum-rpc.publicnode.com` |
| Sepolia | `11155111` | `https://ethereum-sepolia-rpc.publicnode.com` |
| Hoodi | `560048` | `https://ethereum-hoodi-rpc.publicnode.com` |

`init_defaults` is idempotent — including across restarts — so it may be called
unconditionally; `applied: false` is not an error. It writes over nothing: an endpoint,
verified mode or proxy policy already stored is left alone, and a record's `source` moves
one way only, `default` → `external`, as soon as any caller writes to it.

> **All three defaults are one operator.** publicnode sees the traffic of every
> default-configured wallet. Set your own endpoint (or a SOCKS proxy) in `eth_rpc_ui` if
> that matters to you — the defaults exist so the wallet works, not because they are private.

## Build & test

```bash
cd rust-lib && cargo test --no-default-features   # rpc + proxy cores (mock node + fail-closed)
nix build .#install                                # -> result/modules/eth_rpc_module/
```

> `src/proxy.rs` is an inlined copy of the canonical `logos-evm-net-proxy` crate
> (the module builder only stages a module's `rust-lib`, so a sibling path dep
> isn't visible in the nix sandbox). Keep the two in sync.
