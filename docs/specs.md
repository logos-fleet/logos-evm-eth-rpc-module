# `eth_rpc_module` — Specification & Reference

> Proxyable, fail-closed Ethereum JSON-RPC client for the Logos multi-chain EVM wallet.
> Per-chain config (endpoint + proxy policy); RPC calls keyed by `chainId`; `socks5h`/Tor-ready.

This document is the exhaustive reference for **`logos-evm-eth-rpc-module`**
(`github.com/logos-co/logos-evm-eth-rpc-module`). It is grounded entirely in the
repository source: `metadata.json`, `rust-lib/src/{lib.rs,glue.rs,rpc.rs,proxy.rs}`,
`rust-lib/Cargo.toml`, `flake.nix`, `CMakeLists.txt`, and the executable doc-test under
`doctests/`.

---

## 1. Purpose & place in the system

The "EVM wallet" is a multi-chain Ethereum wallet built as a set of **Logos modules** —
process-isolated plugins that talk over a Logos transport (QtRO / plain) via a typed RPC
bridge. A module exposes methods (here, a Rust trait) that other modules or the
`logoscore` daemon invoke through a generated client.

`eth_rpc_module` is the wallet's **multi-chain JSON-RPC transport**. It is a thin,
privacy-hardened gateway between the rest of the wallet and the public Ethereum-family
RPC endpoints:

- It **stores per-chain configuration** (RPC endpoint + proxy policy), keyed by
  `chainId`, persisted to disk. Callers route by `chainId` alone — they never pass a URL.
- It exposes **chainId-keyed JSON-RPC calls** (`eth_blockNumber`, `eth_getBalance`,
  `eth_call`, `eth_sendRawTransaction`, …) plus a `raw_rpc` escape hatch.
- Every outbound request is built through a **single fail-closed chokepoint**
  (`src/proxy.rs`): a chain configured with `proxyRequired` and no usable proxy
  **refuses to send** rather than leaking traffic in the clear. This is the wallet's
  privacy guarantee. The chokepoint is an inlined copy of the canonical
  `logos-evm-net-proxy` crate.
- It is declared `concurrency: "multi"` (see §9): its 12 RPC methods are blocking
  network round-trips, so the module opts into concurrent dispatch — one slow call no
  longer stalls the others.

### Where it sits in the wallet dependency graph

```mermaid
flowchart TD
    UI["logos-evm-wallet-ui<br/>(QtRO + QML app)"]
    BE["logos-evm-wallet-backend-module<br/>(coordinator + tx builder, alloy)"]
    KS["logos-evm-keystore-module"]
    UNI["logos-evm-uniswap-module<br/>(concurrency: multi)"]
    TL["logos-evm-token-list-module"]
    ETH["eth_rpc_module<br/>(THIS REPO — concurrency: multi)"]
    NP["logos-evm-net-proxy<br/>(fail-closed HTTP, inlined as src/proxy.rs)"]
    NODES["Public JSON-RPC endpoints<br/>(per chainId)"]

    UI --> BE
    BE --> KS
    BE --> ETH
    BE --> TL
    BE --> UNI
    UNI --> ETH
    ETH -. "vendored copy" .-> NP
    ETH --> NODES

    classDef this fill:#1f6feb,stroke:#0d419d,color:#fff;
    class ETH this;
```

`eth_rpc_module` is a **leaf** of the wallet's outbound network surface: it is driven by
`wallet_backend_module` (which pushes chain config down into it and reads balances/gas
through it) and by `uniswap_module` (which issues Multicall3 batches through it). It calls
**no other Logos module** — its only outbound dependency is the network, reached through
the vendored net-proxy chokepoint.

---

## 2. Module identity (`metadata.json`)

| Field | Value | Meaning |
|-------|-------|---------|
| `name` | `eth_rpc_module` | Module id used by `logoscore`/`lgpm`/callers. |
| `version` | `1.0.0` | Module version. |
| `description` | *Proxyable, fail-closed Ethereum JSON-RPC client…* | |
| `author` | `Logos Core Team` | |
| `type` | `core` | Core (headless) module, not a UI plugin. |
| `interface` | `cdylib` | Rust-first cdylib module (no C++ author code). |
| `concurrency` | `multi` | Opts into concurrent handler dispatch (§9). |
| `category` | `wallet` | |
| `main` | `eth_rpc_module_plugin` | Built plugin name. |
| `dependencies` | `[]` | **No Logos-module dependencies** (network is its only dep). |
| `include` / `capabilities` | `[]` | None. |
| `codegen.rust` | `{ crate: "rust-lib", trait: "EthRpcModule", source: "src/glue.rs" }` | The builder derives the module's `.lidl` contract from the `EthRpcModule` trait. |
| `nix` | empty `external_libraries` / `packages` / `cmake` | No external system libraries; TLS is pure-Rust `rustls`. |

The public API of the module is exactly the `EthRpcModule` trait in `src/glue.rs`
(§5). `dependencies: []` is accurate: this module never calls another module.

---

## 3. Overall architecture

The crate has two clean layers:

1. **Crypto-free, Logos-free core** (`rpc.rs` + `proxy.rs`) — plain Rust, unit-tested
   with `cargo test --no-default-features`. No `unsafe`, no Logos runtime.
2. **Logos glue** (`glue.rs`) — present only under the default `logos_module` feature.
   It implements the generated `EthRpcModule` trait, wraps the core in a `RwLock`,
   marshals JSON in/out, and registers the module with the host via the generated
   provider scaffold.

```mermaid
flowchart TB
    subgraph Host["Logos host / logoscore daemon"]
        TR["Logos transport<br/>(QtRO / plain) — C ABI dispatch"]
    end

    subgraph Crate["eth_rpc_module cdylib"]
        subgraph Glue["glue.rs (feature: logos_module)"]
            GEN["generated/provider_gen.rs<br/>(install, RustModuleContext,<br/>logos_module_dispatch C ABI)"]
            IMPL["EthRpcModuleImpl<br/>impl EthRpcModule"]
            STATE["RwLock&lt;Option&lt;EthRpc&gt;&gt;<br/>with_rpc (read) / with_rpc_mut (write)"]
            JSON["JSON marshalling<br/>err / ok_result / parse_json"]
        end
        subgraph Core["core (Logos-free, cargo-testable)"]
            RPC["rpc.rs — EthRpc<br/>chainId → ChainConfig map<br/>+ persisted store (chains.json)"]
            PROXY["proxy.rs — build_client<br/>fail-closed chokepoint<br/>(only reqwest::Client ctor)"]
        end
    end

    NET["reqwest blocking client<br/>(rustls-tls + socks5h)"]
    NODE["JSON-RPC endpoint<br/>(per-chain)"]

    TR -->|"logos_module_dispatch(method, args_json)"| GEN
    GEN -->|"&self, parsed args"| IMPL
    IMPL --> STATE
    STATE --> RPC
    JSON -. used by .- IMPL
    RPC -->|"client_for(chainId)"| PROXY
    PROXY -->|"Ok(client) or fail-closed Err"| NET
    NET --> NODE

    classDef core fill:#196c2e,stroke:#0f5323,color:#fff;
    class RPC,PROXY core;
```

### Internal pieces

| File | Type / fn | Role |
|------|-----------|------|
| `lib.rs` | crate root | Declares `mod proxy; mod rpc;` and `#[cfg(feature="logos_module")] mod glue;`. Re-exports `ChainConfig`, `EthRpc`, `RpcError`. |
| `rpc.rs` | `EthRpc` | The RPC client: `HashMap<u64, ChainConfig>` + optional JSON store path; per-chain typed RPC helpers. |
| `rpc.rs` | `ChainConfig` | Per-chain config struct (camelCase serde). |
| `rpc.rs` | `ConfigSource` / `DEFAULT_ENDPOINTS` | Who wrote a chain's record, and the built-in public endpoints `init_defaults` seeds (§5.6). |
| `rpc.rs` | `RpcError` | Error enum: `UnknownChain`, `Proxy`, `Http`, `Rpc{code,message}`, `Parse`. |
| `proxy.rs` | `ProxyConfig` | Outbound policy: `proxy`, `proxy_required`, `timeout_secs`. |
| `proxy.rs` | `build_client` | **The only** `reqwest::Client` constructor; fails closed. |
| `proxy.rs` | `ProxyError` | `ProxyRequiredButUnset`, `ProxyUnusable`, `Build`. |
| `glue.rs` | `EthRpcModule` (trait) | The module's public contract (24 methods + hook). |
| `glue.rs` | `EthRpcModuleImpl` | The implementation, holding `rpc: RwLock<Option<EthRpc>>`. |
| `glue.rs` | `with_rpc` / `with_rpc_mut` | Read-lock / write-lock helpers (§9). |
| `glue.rs` | `logos_module_install` | `#[no_mangle]` install hook → `install::<EthRpcModuleImpl>()`. |
| *generated* | `generated/provider_gen.rs` | `include!`d by `glue.rs`; provides `install`, `RustModuleContext`, and the C-ABI dispatch surface. **Generated by the module builder from the trait at build time — not checked in** (`.gitignore` excludes `rust-lib/generated/`). |

---

## 4. Communication with dependencies (data flow)

`eth_rpc_module` has **no Logos-module dependencies**; its only "dependency" is the
network, reached through the inlined net-proxy chokepoint. The representative flow below
shows a caller (`wallet_backend_module` or `uniswap_module`, or the `logoscore` CLI)
driving this module, and the module reaching a JSON-RPC node.

```mermaid
sequenceDiagram
    autonumber
    participant Caller as wallet_backend / uniswap / logoscore
    participant TR as Logos transport (consumer side)
    participant Glue as glue.rs (EthRpcModuleImpl, &self)
    participant State as RwLock&lt;Option&lt;EthRpc&gt;&gt;
    participant Proxy as proxy.rs build_client (fail-closed)
    participant Node as JSON-RPC endpoint

    Note over Caller,Glue: One-time config push (write path)
    Caller->>TR: set_chain_config(1, {endpoint, proxyRequired,...})
    TR->>Glue: logos_module_dispatch("set_chain_config", [1, "{...}"])
    Glue->>State: with_rpc_mut (WRITE lock)
    State-->>Glue: rpc.set_chain_config + persist(chains.json)
    Glue-->>Caller: true

    Note over Caller,Node: Per-call read path (concurrency: multi — many run at once)
    Caller->>TR: get_balance(1, "0x..addr")
    TR->>Glue: logos_module_dispatch("get_balance", [1, "0x..addr"])
    Glue->>State: with_rpc (READ lock — shared, overlaps other reads)
    State->>Proxy: client_for(1) → build_client(ProxyConfig)
    alt proxy_required && no usable proxy
        Proxy-->>Glue: Err(ProxyRequiredButUnset)  ❌ fail-closed
        Glue-->>Caller: {"ok":false,"error":"proxy: proxy required but none configured ..."}
    else proxy OK (or not required)
        Proxy-->>State: reqwest blocking client
        State->>Node: POST eth_getBalance [addr,"latest"]
        Node-->>State: {"jsonrpc":"2.0","result":"0x1234"}
        State-->>Glue: Ok("0x1234")
        Glue-->>Caller: {"ok":true,"result":"0x1234"}
    end
```

For `concurrency: "multi"`, the result may be returned to the caller via a **pending
sentinel** that the *consumer's* transport resolves transparently — see §9. The author
code in this repo (`glue.rs`) is unaware of that; it just returns the JSON string.

### How specific callers drive it

- **`wallet_backend_module`** pushes chain config (`set_chain_config`) at startup, then
  fans out per-chain `get_balance` / `verify_chain_id` and uses `gas_price`,
  `fee_history`, `estimate_gas`, `get_transaction_count`, `send_raw_transaction`,
  `get_transaction_receipt` across the send pipeline (build → sign → broadcast → record).
- **`uniswap_module`** issues a single Multicall3 batch as an `eth_call` via this module's
  `call(chainId, {to, data})` (and reads block/fee data) to price V2/V3/V4 pools.

---

## 5. Full API reference (`EthRpcModule`)

The public contract is the `EthRpcModule` trait in `src/glue.rs`. All structured values
cross the bridge as **JSON strings**. Every method returns one of two JSON envelopes
(except the three that return a bare `bool`):

```jsonc
// success
{ "ok": true,  ...payload... }
// failure
{ "ok": false, "error": "<message>" }
```

Parameter types are the cdylib/LIDL primitives: `chain_id` and `blocks` are `i64`;
addresses, hashes, and all JSON blobs are `String`. `chain_id` is internally cast to
`u64`; `blocks` is clamped with `.max(0)`.

> **Calling convention via `logoscore`:** `logoscore call eth_rpc_module <method> <args...>`.
> A `@file.json` argument loads file content as the argument. Type auto-detection makes
> bare integers `int`, etc. (see the doc-test in §8 for concrete invocations).

### 5.1 Configuration methods

#### `set_chain_config(chain_id: i64, config_json: String) -> bool`
Store (insert/replace) the configuration for a chain and persist it.
- **`chain_id`** — EIP-155 chain id (e.g. `1` mainnet, `10` Optimism).
- **`config_json`** — a JSON object matching `ChainConfig` (§6):
  `{ "endpoint": "...", "proxy"?: "...", "proxyRequired"?: bool, "timeoutSecs"?: u64,
  "verifiedProxyMode"?: "off"|"required", "verifiedTimeoutSecs"?: u64 }`.
- An **omitted** `verifiedProxyMode` / `verifiedTimeoutSecs` preserves what is stored; only an
  explicit `"off"` lowers the mode. `chains.json` is shared, and a sibling wallet that predates
  verified routing must not revoke a user's security setting by silence.
- **Returns** `true` on success; `false` if `config_json` fails to parse **or** the
  context is not yet ready (`EthRpc` not initialized).
- **Lock:** WRITE (one of only two write-lock methods).

```bash
logoscore call eth_rpc_module set_chain_config 1 @chain_ok.json
# chain_ok.json: { "endpoint": "http://127.0.0.1:8599", "proxyRequired": false }
# -> true
```

#### `get_chain_config(chain_id: i64) -> String`
Return the stored config for a chain.
- **Success:** `{ "ok": true, "config": { endpoint, proxy, proxyRequired, timeoutSecs } }`
  (the `ChainConfig` serialized camelCase).
- **Error:** `{ "ok": false, "error": "no config for chain <id>" }`, or the
  not-initialized error if context isn't ready.
- **Lock:** READ.

#### `remove_chain_config(chain_id: i64) -> bool`
Delete a chain's config and persist.
- **Returns** `true` if a config existed and was removed; `false` if absent or context not ready.
- **Lock:** WRITE.

#### `list_chains() -> String`
List configured chain ids (sorted ascending).
- **Success:** `{ "ok": true, "chains": [1, 10, ...] }`.
- **Error:** not-initialized error if context not ready.
- **Lock:** READ.

```bash
logoscore call eth_rpc_module list_chains
# -> {"ok":true,"chains":[1,9]}
```

### 5.2 RPC methods (all keyed by `chain_id`)

All 12 methods below take a READ lock and perform a blocking JSON-RPC round-trip through
the fail-closed client. On any failure they return `{ "ok": false, "error": "<message>" }`,
where the message is the `Display` of the underlying `RpcError` (§6.2): e.g.
`no configuration for chain <id>`, `proxy: proxy required but none configured ...`,
`http: <reqwest error>`, `rpc error <code>: <message>`, `parse: <detail>`.

Every success envelope also carries **`route`** — how the answer was obtained:

| `route` | Meaning |
|---------|---------|
| `"verified"` | Proof-backed by the light client (`eth_getBalance`, `eth_getTransactionCount`, `eth_getCode`, `eth_getStorageAt`, `eth_call`). |
| `"proxied"` | Routed through the verified proxy, but **forwarded to its own execution provider on trust** — receipts, `eth_feeHistory`, `eth_gasPrice`, `eth_estimateGas`, broadcasts. |
| `"direct"` | Not routed through the proxy at all (the chain's mode is `off`, or the call used `raw_rpc_url`). |

A UI must not badge `"proxied"` as verified: a light client proves state against a header's
stateRoot and can prove neither a fee oracle's opinion nor that a broadcast was accepted.

#### `verify_chain_id(chain_id: i64) -> String`
`eth_chainId` round-trip; decodes the hex result to a decimal number.
- **Success:** `{ "ok": true, "chainId": <u64> }`.
- Useful as a liveness/correctness check that the configured endpoint actually serves the
  expected chain.

```bash
logoscore call eth_rpc_module verify_chain_id 1
# -> {"ok":true,"chainId":1}
```

#### `block_number(chain_id: i64) -> String`
`eth_blockNumber`. **Success:** `{ "ok": true, "result": "0x<hex>" }` (raw hex string).

#### `get_balance(chain_id: i64, address: String) -> String`
`eth_getBalance([address, "latest"])`.
- **`address`** — 20-byte hex account address (`0x…`; the doc-test also passes it without
  the `0x` prefix and the node accepts it).
- **Success:** `{ "ok": true, "result": "0x<wei-hex>" }`.

```bash
logoscore call eth_rpc_module get_balance 1 0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266
# -> {"ok":true,"result":"0x1234"}
```

#### `call(chain_id: i64, call_json: String) -> String`
`eth_call([call, "latest"])` — used for ERC-20 / contract reads (and Multicall3 batches).
- **`call_json`** — a JSON object, typically `{ "to": "0x…", "data": "0x…" }`.
- **Success:** `{ "ok": true, "result": "0x<returndata-hex>" }`.
- **Error:** `{ "ok": false, "error": "<parse error>" }` if `call_json` is not valid JSON.

#### `get_transaction_count(chain_id: i64, address: String) -> String`
`eth_getTransactionCount([address, "pending"])` — the account nonce **including pending**.
**Success:** `{ "ok": true, "result": "0x<nonce-hex>" }`.

#### `gas_price(chain_id: i64) -> String`
`eth_gasPrice`. **Success:** `{ "ok": true, "result": "0x<wei-hex>" }`.

#### `fee_history(chain_id: i64, blocks: i64, reward_percentiles_json: String) -> String`
`eth_feeHistory([<blocks hex>, "latest", reward_percentiles])` — EIP-1559 fee estimation.
- **`blocks`** — number of blocks to look back (clamped to ≥ 0, encoded as `0x<hex>`).
- **`reward_percentiles_json`** — a JSON array of percentiles, e.g. `[10, 50, 90]`.
- **Success:** `{ "ok": true, "result": <feeHistory object> }` (the full node object:
  `baseFeePerGas`, `gasUsedRatio`, `reward`, `oldestBlock`).
- **Error:** parse error if `reward_percentiles_json` is invalid JSON.

#### `estimate_gas(chain_id: i64, tx_json: String) -> String`
`eth_estimateGas([tx])`.
- **`tx_json`** — a partial tx object (`{ from, to, value, data, ... }`).
- **Success:** `{ "ok": true, "result": "0x<gas-hex>" }`.

#### `send_raw_transaction(chain_id: i64, raw_hex: String) -> String`
`eth_sendRawTransaction([raw_hex])` — broadcast a signed raw transaction.
- **`raw_hex`** — the RLP-encoded signed tx (`0x…`).
- **Success:** `{ "ok": true, "hash": "0x<txhash>" }` (note: `hash`, not `result`).

#### `get_transaction_receipt(chain_id: i64, hash_hex: String) -> String`
`eth_getTransactionReceipt([hash])`.
- **Success:** `{ "ok": true, "result": <receipt object | null> }` (full node object; `null`
  while pending/unknown).

#### `get_transaction_by_hash(chain_id: i64, hash_hex: String) -> String`
`eth_getTransactionByHash([hash])`.
- **Success:** `{ "ok": true, "result": <tx object | null> }`.

#### `raw_rpc(chain_id: i64, method: String, params_json: String) -> String`
Escape hatch for **any** standard JSON-RPC method on a configured chain.
- **`method`** — the JSON-RPC method name, e.g. `"eth_getLogs"`.
- **`params_json`** — a JSON **array** of params.
- **Success:** `{ "ok": true, "result": <whatever the node returns> }`.
- **Error:** parse error if `params_json` isn't valid JSON; otherwise the usual RPC errors.

```bash
logoscore call eth_rpc_module raw_rpc 1 eth_getCode '["0x...addr","latest"]'
```

### 5.3 Lifecycle hook (not externally callable)

#### `on_context_ready(&self, ctx: &RustModuleContext)`
Fired once by the host after it stamps the module context and before the first inbound
dispatch (the Rust analog of C++ `onContextReady`). The impl builds the persisted store:

```rust
let path = Path::new(&ctx.instance_persistence_path).join("chains.json");
*self.rpc.write().unwrap() = Some(EthRpc::with_store(path));
```

`RustModuleContext` (from the generated scaffold) carries `module_path`, `instance_id`,
and `instance_persistence_path`. Before this fires, every method short-circuits to the
not-initialized error (`with_rpc`) or `false` (`with_rpc_mut`).

### 5.4 Return-shape summary

| Method | Lock | Success shape |
|--------|------|---------------|
| `set_chain_config` | W | `bool` |
| `get_chain_config` | R | `{ ok, config }` |
| `remove_chain_config` | W | `bool` |
| `list_chains` | R | `{ ok, chains:[u64] }` |
| `verify_chain_id` | R | `{ ok, chainId:u64 }` |
| `block_number` | R | `{ ok, result:"0x…" }` |
| `get_balance` | R | `{ ok, result:"0x…" }` |
| `call` | R | `{ ok, result:"0x…" }` |
| `get_transaction_count` | R | `{ ok, result:"0x…" }` |
| `gas_price` | R | `{ ok, result:"0x…" }` |
| `fee_history` | R | `{ ok, result:{…} }` |
| `estimate_gas` | R | `{ ok, result:"0x…" }` |
| `send_raw_transaction` | R | `{ ok, hash:"0x…" }` |
| `get_transaction_receipt` | R | `{ ok, result:{…}|null }` |
| `get_transaction_by_hash` | R | `{ ok, result:{…}|null }` |
| `raw_rpc` | R | `{ ok, result:<any> }` |
| `config_status` | R | `{ ok, state, source, chains:[…] }` |
| `init_defaults` | W | `{ ok, applied, seeded:{…} }` |

Every RPC success shape also carries `route: "verified"|"proxied"|"direct"` (see §5.2).

All "R/W" failures use `{ ok:false, error:"…" }` except the three `bool` methods, which
return `false`.

### 5.5 The verified-proxy gate (`verified_proxy_status`, `verdict.rs`)

`verified_proxy_status(chain_id) -> { ok, chainId, mode, state, usable, blocking, message,
action, detail }`. Every key is always present; `detail` is `""`, never absent. `state` is one
of `disabled | ready | syncing | missing | unconfigured | stopped | wrong_chain | unhealthy`
and `action` one of `none | wait | install_or_load | open_verified_proxy | restart_or_reload`.
`ok` is false only when this module has no context — `missing` is an answer, not an error.

**Evaluation order (`verdict::evaluate`) — `modules_state` first.**

1. Mode is not `required` → `disabled`, nothing is called.
2. Cached verdict younger than `HEALTH_TTL` (5s) → reuse it.
3. **Readiness**: `modules_state.list_modules()`, 750ms, cached `READY_TTL` (2s) and NOT per
   chain. A positive "not loaded" returns `missing` here and the probe never runs.
4. **Probe**: `verified_proxy_module.status()`, `PROBE_BUDGET` 1500ms → `classify_status`.
5. **Refine**: only if step 4 FAILED, `modules_state.module_record("verified_proxy_module")`,
   750ms → `classify_modules_state`. It can sharpen the reason, never veto a reachable proxy.

Step 3 is the whole point of the order: with verified mode required and the proxy module not
loaded, every uncached evaluation used to pay 1500ms + 750ms, and a UI polling on the same 5s
cadence as `HEALTH_TTL` missed the cache nearly every time.

**What is cached, and for how long (`verdict::GateCache`).** The TTLs are a policy over
`evaluate`, held apart from it so both are testable with `cargo test --no-default-features`.

| Verdict | Cached? | Why |
|---------|---------|-----|
| from step 2/4/5 (the probe ran) | `HEALTH_TTL` | 1500ms + 750ms is exactly what a cache is for. |
| `missing` from step 3 (short-circuit) | **no** | Free to recompute — the listing under it is already memoized for `READY_TTL`, so caching it again would only extend it. |

`GateCache::invalidate(chain_id)` drops **both** the chain's verdict and the host readiness
snapshot, and every config mutator (§5.1) calls it. Dropping only the verdict left a retry
reading a 2s-old "not loaded" and answering `missing` anyway.

**Worst case for "I just installed/loaded the proxy — retry".**

| Retry path | Told `missing` for up to |
|------------|--------------------------|
| any config mutator, then read the status | 0 — both caches are dropped |
| status poll alone, previous verdict was the step-3 short-circuit | `READY_TTL` — 2s |
| status poll alone, previous verdict was probed (registry unfed or `Loaded`) | `HEALTH_TTL` — 5s |

The second row was `READY_TTL + HEALTH_TTL` (~7s) before the short-circuited verdict stopped
being cached. Nothing here can fail open: a stale `Loaded` or `Unknown` still falls through to
the probe, and only a *negative* answer is refused a cache.

**What may short-circuit (`verdict::classify_readiness`).** Only a POSITIVE statement:

| Listing | Readiness | Effect |
|---------|-----------|--------|
| complete (`partial:false`), non-empty, proxy not in it | `NotLoaded` | `missing`, no probe |
| complete, non-empty, proxy record `state:"unloaded"` | `NotLoaded` | `missing`, no probe |
| complete, non-empty, proxy in any other state | `Loaded` | probe anyway |
| empty `modules` (whatever `partial` says) | `Unknown` | probe |
| `partial:true`, or no `partial` field at all | `Unknown` | probe |
| call failed / null / not a listing | `Unknown` | probe |

An empty or `partial` listing is NO information, not negative information. A host older than
liblogos `84564f0` (#189) embeds `modules_state` with nothing feeding it, so it answers
`{"modules":[],"partial":true}` — indistinguishable from "nothing is loaded". Concluding
`missing` there would blank a working wallet, which is worse than the latency being avoided.
Artifact-level check for any host: `strings <liblogos>/lib/liblogos_core.dylib | grep -c
modules_state`; zero means no feed. `list_modules` and not `is_ready`, because a bool cannot
separate "not loaded" from "the registry has nothing to say" and carries no `partial`.

`Loaded` is not `usable`: loaded says nothing about configured, started, synced, or on the
right chain, so it is always followed by the probe.

**No dependency.** `modules_state` is reached by an UNTYPED call and stays out of
`metadata.json` `dependencies`, exactly as `verified_proxy_module` does, so no consumer of this
module inherits a closure from the gate.

### 5.6 Initialization convention (`config_status`, `init_defaults`)

The module is usable with **no external configuration**: a consumer asks whether a config has
been set, and if not seeds the built-in public endpoints. Neither method performs network I/O
or calls another module, so both are cheap enough for a consumer's context-ready path.

Two independent questions, kept apart by the `state` field — never by matching the message:

| question | `state` |
|---|---|
| has `on_context_ready` run? | `unready` (and `ok:false`) |
| has a config been *set*? | `unconfigured` / `configured` |

#### `config_status() -> String`
Whether a config has been set, per chain and rolled up.
- **Success:** `{ ok:true, state:"configured"|"unconfigured", source:"external"|"default"|"none",
  chains:[ { chainId, state, source, endpoint?, verifiedProxyMode? } ] }`.
- `chains` is the **union** of the stored chains and `DEFAULT_ENDPOINTS`, sorted by `chainId`;
  a chain with no record is listed as `state:"unconfigured", source:"none"` and carries no
  `endpoint`.
- Module-level `state` is `configured` if **any** chain is; `source` is `external` if **any**
  chain is. This roll-up is for **display only, not a gate**: a store with chain 1 configured
  and 11155111 absent rolls up to `configured` while still needing seeding.
- **Error:** `{ ok:false, state:"unready", error:"eth_rpc not initialized (context not ready)" }`.
- **Lock:** READ.

#### `init_defaults() -> String`
Seed the built-in endpoints, per chain and per **field**, only where absent.
- **Success:** `{ ok:true, applied:bool, seeded:{ "<chainId>": ["*"|"endpoint", …] } }`.
  `"*"` means the whole record was written; `[]` means the chain was already there.
  `applied` is `true` iff any chain was written.
- **Idempotent, and idempotent across restarts.** A second call — from any consumer — writes
  nothing and answers `applied:false`, which is **not an error**. Two consumers racing both
  succeed; exactly one sees `applied:true`.
- Implemented as [`EthRpc::ensure_chain_config`] in a loop, so it is idempotent **per chain**
  and a consumer may call it unconditionally rather than gating on `config_status`.
- Every chain it writes has its memoized verified-proxy verdict invalidated (§5.5).
- **Error:** the same `state:"unready"` refusal as above.
- **Lock:** WRITE.

#### The built-in endpoints (`DEFAULT_ENDPOINTS`, `rpc.rs`)

| Chain | id | Endpoint |
|---|---|---|
| Ethereum | `1` | `https://ethereum-rpc.publicnode.com` |
| Sepolia | `11155111` | `https://ethereum-sepolia-rpc.publicnode.com` |
| Hoodi | `560048` | `https://ethereum-hoodi-rpc.publicnode.com` |

Measured live 2026-08-28. **One endpoint per chain, not a list**: a silent failover changes
*which* node answered without the consumer knowing, and picking a different one is
`eth_rpc_ui`'s job. `verifiedProxyMode` is `off` on every seeded record — verified routing
needs an archive node these are not, and `validate()` refuses it beside `proxyRequired`.

> **All three are one operator.** publicnode therefore sees the traffic of every
> default-configured wallet. That is a reason `eth_rpc_ui` and the SOCKS `proxy` setting
> exist — not a reason to ship an endpoint that does not work.

#### Never downgrade

`init_defaults` can only ever **fill an absent slot**: an absent record, or an absent field of
a record already there. It writes over nothing, whatever the record's `source`. `chains.json`
is shared with other wallets on the device, so this is the same discipline `ChainConfigWire`
already applies to `set_chain_config` (§6.1).

`source` records who wrote a chain, so a UI can tell a value the user chose from one chosen
for them. It upgrades **one way**, `default` → `external`: every caller-facing write path
(`set_chain_config`, `patch_chain_endpoint`, `set_verified_proxy_mode`, `patch_chain_transport`)
sets `external`. It gates nothing — `ensure_chain_config` is what actually refuses to
overwrite. No caller can send it: `ChainConfigWire` has no such field, and the seeding path
that does parse a whole `ChainConfig` forces `external` (`ChainConfig::from_caller_json`).
The field stays deserializable only so `chains.json` reads back.

---

---

### 5.7 Events (`EthRpcModuleEvents`)

Declared as the companion trait `EthRpcModuleEvents` in `src/glue.rs`; the builder derives the
`.lidl` event declarations from it and generates the `emit_*` free functions.

| event | payload | when |
|---|---|---|
| `chain_config_changed` | `chain_id: i64` | that chain's stored record was added, removed or altered |
| `verified_proxy_mode_changed` | `chain_id: i64`, `mode: String` | the verified-proxy gate for that chain moved; `mode` is the value now in force (`"off"` / `"required"`) |

**Granularity.** Per chain, not per field and not one global "something changed". A consumer
redraws or re-reads a whole chain row anyway, so per-field events would only add noise; a
single global event would make a three-chain consumer re-read all three because one moved.
Neither event carries the config itself — this module is the single owner of that record, and
a copy on the event plane is a second place it can go stale. `verified_proxy_mode_changed` is
a **refinement** of `chain_config_changed`, not an alternative: a mode change emits both, so a
consumer that only redraws configuration subscribes to one event and a wallet that only gates
subscribes to the other. The mode is the one field carried inline, because it is the one a
wallet branches on before deciding whether to ask anything else.

**Which methods emit.** Every mutator, and only on a real change:

| method | emits |
|---|---|
| `set_chain_config` | `chain_config_changed`; also the mode event when the wire moved the mode |
| `remove_chain_config` | `chain_config_changed`; and `verified_proxy_mode_changed(id, "off")` when the removed chain was `required` |
| `set_verified_proxy_mode` | both, on an accepted change |
| `patch_chain_endpoint`, `patch_chain_transport`, `ensure_chain_config` | `chain_config_changed` |
| `init_defaults` | `chain_config_changed` per chain it actually seeded |

Every read method — `get_chain_config`, `list_chains`, `config_status`, `verified_proxy_status`
and the RPC calls — is silent.

**Two invariants, both tested (`rpc.rs`, `mod change_tests`):**

1. *Emit only on an actual change.* The decision is `diff_chain(before, after)` over the stored
   record, taken inside the write lock — never "a setter was called". A sibling wallet that
   pushes the same config on every start, a `set_verified_proxy_mode` to the mode already in
   force, a re-`patch` of the same endpoint, a refused switch, a second `init_defaults`: all
   silent. Without this the wallet would be woken by the sibling's harmless startup write.
2. *State before event.* The write lock is released before the emit, and the store persists
   inside the mutator, so a subscriber calling straight back into `get_chain_config` or
   `verified_proxy_status` reads the new value. Emitting under the lock would let a subscriber
   read back the value it was just told had changed.

An absent record counts as mode `off` on both sides of the diff — the same reading
`verified_proxy_status` gives it — so removing a `required` chain closes the gate loudly, and
seeding a fresh chain (whose builtin record is `off`) moves no gate and emits no mode event.

## 6. Configuration & data model

### 6.1 `ChainConfig` (`rpc.rs`)

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainConfig {
    pub endpoint: String,                 // JSON-RPC URL (required)
    #[serde(default)]
    pub proxy: Option<String>,            // e.g. "socks5h://127.0.0.1:9050"; None = no proxy
    #[serde(default)]
    pub proxy_required: bool,             // JSON: "proxyRequired" — fail-closed switch
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,                // JSON: "timeoutSecs" — default 8
    #[serde(default)]
    pub verified_proxy_mode: VerifiedProxyMode,  // JSON: "verifiedProxyMode" — off | required
    #[serde(default = "default_verified_timeout")]
    pub verified_timeout_secs: u64,       // JSON: "verifiedTimeoutSecs" — default 15
    #[serde(default)]
    pub source: ConfigSource,             // JSON: "source" — external | default (§5.6)
}
```

| JSON field | Type | Default | Meaning |
|------------|------|---------|---------|
| `endpoint` | string | — (required) | RPC URL the requests POST to. |
| `proxy` | string? | absent → `None` | Proxy URL. Schemes: `socks5h`, `socks5`, `http`, `https`. `socks5h` resolves DNS through the proxy (Tor-preferred). |
| `proxyRequired` | bool | `false` | If `true`, requests must traverse a proxy; with none usable the client fails closed. |
| `timeoutSecs` | u64 | `8` | Per-request timeout. `0` leaves reqwest's default. |
| `verifiedProxyMode` | `"off"` \| `"required"` | `"off"` | `required` routes through the light-client proxy and REFUSES rather than falling back (§5.2). Contradicts `proxyRequired`, which `validate()` rejects. |
| `verifiedTimeoutSecs` | u64 | `15` | Budget for one call on the verified leg — a second hop, so more room than `timeoutSecs`. Read per call from this record and **clamped to 1..=60**: 0 would time out instantly, and an unbounded value never returns. |
| `source` | `"external"` \| `"default"` | absent → `"external"` | Who wrote the record (§5.6). Reporting only, and **a caller cannot declare its own write to be a default**: `ChainConfigWire` omits the field, and the one path that parses a whole `ChainConfig` from caller JSON (`ensure_chain_config`) goes through `ChainConfig::from_caller_json`, which forces `external`. Absent on a pre-existing `chains.json` reads as `external` — a record already on disk was written by somebody. |

> **camelCase is load-bearing.** The wallet backend emits `proxyRequired` / `timeoutSecs`.
> A regression test (`camelcase_proxy_required_is_honored`) asserts the serde mapping holds
> — if it broke, fail-closed would silently fail **open**.

### 6.2 `RpcError` (`rpc.rs`) and its `Display` strings

| Variant | `Display` (becomes the `error` field) |
|---------|----------------------------------------|
| `UnknownChain(id)` | `no configuration for chain <id>` |
| `Proxy(e)` | `proxy: <e>` (e.g. `proxy: proxy required but none configured (fail-closed: refusing to send in the clear)`) |
| `Http(e)` | `http: <reqwest error>` |
| `Rpc { code, message }` | `rpc error <code>: <message>` (node-returned JSON-RPC error) |
| `Parse(e)` | `parse: <detail>` (bad response shape / bad hex) |
| `VerifiedProxy(e)` | `verified proxy: <e>` — the verified leg could not answer. Never downgraded to a direct call. |
| `VerifiedBypass(m)` | `<m> is proof-backed on this chain's verified route: refused through an explicit url, which cannot prove it (use raw_rpc)` |

### 6.3 `ProxyConfig` / `ProxyError` (`proxy.rs`)

`ProxyConfig { proxy: Option<String>, proxy_required: bool, timeout_secs: u64 }` is built
per call from the chain's `ChainConfig` in `EthRpc::client_for`. Errors:

| `ProxyError` | Message |
|--------------|---------|
| `ProxyRequiredButUnset` | `proxy required but none configured (fail-closed: refusing to send in the clear)` |
| `ProxyUnusable(s)` | `proxy URL is invalid or unsupported: <s>` (also fires for unsupported schemes) |
| `Build(s)` | `failed to build HTTP client: <s>` |

### 6.4 Persisted state — `chains.json`

State is a single JSON file at `<instance_persistence_path>/chains.json`, written by
`EthRpc::persist` (pretty-printed) and read by `EthRpc::load`. Shape is a string-keyed map
(chainId stringified) of `ChainConfig`:

```json
{
  "1":  { "endpoint": "https://eth.example",  "proxy": null, "proxyRequired": false, "timeoutSecs": 30 },
  "10": { "endpoint": "https://op.example",   "proxy": "socks5h://127.0.0.1:9050", "proxyRequired": true, "timeoutSecs": 30, "source": "external" }
}
```

- Keys that don't parse as `u64` are silently dropped on load.
- The parent directory is created on first write (`create_dir_all`).
- The file is rewritten in full on every `set_chain_config` / `remove_chain_config`.
- Config survives a daemon restart (proven by `config_store_roundtrip_persists`).
- The absence of this file is the only signal that nothing has been configured, which is what
  makes `init_defaults` idempotent **across process lifetimes** and not merely within one
  (`initialization_is_still_a_no_op_after_a_restart`).

---

## 7. The fail-closed proxy chokepoint (security invariant)

`src/proxy.rs::build_client` is **the only constructor of a `reqwest::Client` in the
crate** — a comment notes a unit test asserts `reqwest::blocking::Client::builder` appears
only there. Every outbound request therefore inherits its policy. Logic:

```
has_proxy = proxy is Some and non-empty (trimmed)
if has_proxy:
    validate scheme ∈ {socks5h, socks5, http, https}   else ProxyUnusable
    builder.proxy(Proxy::all(p))
else:
    if proxy_required:  return Err(ProxyRequiredButUnset)   # FAIL CLOSED
    else:               builder.no_proxy()
if timeout_secs > 0: builder.timeout(timeout_secs)
builder.build()
```

**Invariants:**

1. **Fail-closed:** a chain with `proxyRequired = true` and no usable proxy never sends a
   request — `client_for` returns `Err(RpcError::Proxy(...))` before any network I/O.
   (`fail_closed_when_proxy_required_but_unset` in `rpc.rs`,
   `fail_closed_when_required_and_unset` in `proxy.rs`.)
2. **Scheme allow-list:** only `socks5h`/`socks5`/`http`/`https` are accepted; anything
   else (`ftp://…`) is `ProxyUnusable`.
3. **DNS privacy:** `socks5h://` resolves DNS through the proxy (Tor-ready), preventing DNS
   leaks; the wallet backend is expected to configure this scheme.
4. **Pure-Rust TLS:** `reqwest` uses `rustls-tls` (no OpenSSL), so the nix build has no
   external system library dependency (`metadata.json` `nix.external_libraries: []`).

This is the wallet's **privacy chokepoint**, inherited from `logos-evm-net-proxy`. The
file is an **inlined copy** of that canonical crate (the module builder only stages a
module's `rust-lib`, so a sibling `path` dep isn't visible in the nix sandbox). The two
must be kept in sync; the canonical crate remains the audited reference + standalone test
harness.

---

## 8. Build, run & test

### 8.1 Core unit tests (no Logos runtime)

The `rpc` + `proxy` cores are plain Rust and testable without nix:

```bash
cd rust-lib
cargo test --no-default-features    # exercises rpc + proxy (mock node + fail-closed)
```

Tests present (all in-source `#[cfg(test)]`):

| Test | What it proves |
|------|----------------|
| `config_store_roundtrip_persists` | `set`/`remove`/`list` + persistence across reopen. |
| `camelcase_proxy_required_is_honored` | camelCase serde mapping (fail-closed can't fail open). |
| `unknown_chain_errors` | `get_balance` on an unconfigured chain → `UnknownChain`. |
| `fail_closed_when_proxy_required_but_unset` | no request sent when proxy required but unset. |
| `parses_get_balance_against_mock_node` | real HTTP round-trip against a one-shot mock node. |
| `verify_chain_id_decodes_hex` | `eth_chainId` hex → decimal. |
| `surfaces_rpc_error` | node JSON-RPC error surfaces as `RpcError::Rpc{code,message}`. |
| `fail_closed_when_required_and_unset` (proxy) | `ProxyRequiredButUnset`. |
| `ok_when_not_required_and_unset` (proxy) | clear-net allowed when not required. |
| `rejects_unsupported_scheme` (proxy) | unsupported proxy scheme rejected. |

The initialization convention (§5.6) has its own module, `rpc::defaults_tests`:

| Test | What it proves |
|------|----------------|
| `the_shipped_defaults_cover_the_wallets_three_chains_with_verified_routing_off` | the table is 1 / 11155111 / 560048, all `https`, all `verifiedProxyMode: off`, all representable. |
| `initializing_twice_writes_nothing_the_second_time` | first call seeds `["*"]` per chain; the second seeds nothing and the store is byte-identical. |
| `initialization_is_still_a_no_op_after_a_restart` | idempotence survives a process lifetime, via `chains.json`. |
| `a_user_endpoint_and_verified_mode_survive_initialization` | a configured chain keeps its endpoint, verified mode and both timeouts; the absent chains are still seeded. |
| `a_default_fills_an_absent_endpoint_without_touching_the_rest_of_the_record` | per-FIELD seeding: an empty endpoint is filled, the user's `proxy`/`proxyRequired` are not reset. |
| `a_users_edit_relabels_a_defaulted_chain_as_theirs` | all four caller-facing write paths promote `source` to `external`. |
| `a_caller_cannot_declare_its_own_write_to_be_a_default` | `source` from a caller is ignored on **both** caller paths — `apply_chain_config` (wire) and `from_caller_json` (seeding) — so a caller cannot license us to overwrite it, nor have `config_status` label its record built-in. |
| `a_chains_json_written_before_source_existed_reads_back_as_external` | fail closed on a pre-existing store. |
| `an_empty_store_reports_unconfigured_and_still_lists_what_it_could_seed` | the `unconfigured` / `source:"none"` shape. |
| `config_status_separates_a_default_from_a_value_the_user_chose` | per-chain `source`, and one external chain making the roll-up external. |
| `a_chain_outside_the_defaults_is_reported_but_never_seeded` | the union is reported; `init_defaults` touches only its own table. |

### 8.2 Build / package via nix

```bash
nix build .#install      # -> result/modules/eth_rpc_module/  (installable module dir)
nix build .#lgx          # -> result/*.lgx  (packaged module, used by the doc-test)
```

The `flake.nix` delegates entirely to `logos-module-builder.lib.mkLogosModule { src,
configFile = ./metadata.json, flakeInputs }`. The builder:
- derives the `.lidl` contract from the `EthRpcModule` trait (`codegen.rust`),
- generates `rust-lib/generated/provider_gen.rs` (the C-ABI dispatch + `install` +
  `RustModuleContext`) in **multi** mode (because `concurrency: "multi"`),
- compiles the cdylib and stages the `eth_rpc_module_plugin` + `metadata.json`.

`CMakeLists.txt` is the thin C++ wrapper: it includes `LogosModule.cmake` (from
`$LOGOS_MODULE_BUILDER_ROOT`), copies `metadata.json`, and calls
`logos_module(NAME eth_rpc_module)`.

### 8.3 Drive it via `logoscore` (the executable doc-test)

`doctests/eth-rpc-module-runtime.test.yaml` is the canonical end-to-end exercise
(rendered output in `doctests/outputs/eth-rpc-module-runtime.md`; run with
`doctests/run.sh`). It needs **no external network** — it stands up a local Python mock
JSON-RPC node. Flow:

1. Build `logoscore` and `lgpm`. Because this module is `concurrency: "multi"`, the daemon
   must be built against **logos-protocol ≥ 0.2** (so it resolves deferred replies on the
   caller's behalf — §9). The spec pins protocol via `--override-input`.
2. `nix build .#lgx`, seed the capability module, install with
   `lgpm --modules-dir ./modules --allow-unsigned install --file …`.
3. Start the mock node (`mock_node.py`, port 8599) returning canned
   `eth_chainId=0x1`, `eth_getBalance=0x1234`, `eth_blockNumber=0x10`.
4. Start the daemon (`logoscore -D -m ./modules`), `load-module eth_rpc_module`.
5. Drive it:
   ```bash
   logoscore call eth_rpc_module set_chain_config 1 @chain_ok.json   # -> true
   logoscore call eth_rpc_module list_chains                         # -> {...,"chains":[1]}
   logoscore call eth_rpc_module verify_chain_id 1                   # -> {...,"chainId":1}
   logoscore call eth_rpc_module get_balance 1 <addr>               # -> {...,"result":"0x1234"}
   ```
6. **Fail-closed demonstration:** configure chain 9 with `proxyRequired: true` and no
   proxy, then `get_balance 9 <addr>` → the response contains `proxy required`, proving the
   module refuses to send.
7. Stop the daemon (`logoscore stop`) and assert `status` is `not_running`.

> The doc-test wraps `verify_chain_id` / `get_balance` in a retry loop on transient
> `RPC_FAILED` — the first call that drives a blocking round-trip can hit a transport race
> on loaded CI runners; the calls are pure reads, so retrying is safe.

---

## 9. Concurrency — what `concurrency: "multi"` means here

Every RPC method on this module is a **blocking network round-trip** (up to `timeoutSecs`,
default 30s). Under the default single-instance dispatch, one slow call would serialize
behind the instance lock and stall every other caller. Declaring `concurrency: "multi"` in
`metadata.json` opts the module into **concurrent handler dispatch**.

### 9.1 The `&self` + `RwLock` read-lock pattern (this repo)

The multi contract makes the generated `EthRpcModule` trait take **`&self`** and require
**`Send + Sync + 'static`**. So the implementation cannot use `&mut self`; instead the
mutable state lives behind a lock:

```rust
struct EthRpcModuleImpl { rpc: RwLock<Option<EthRpc>> }
```

- `with_rpc(f)` takes a **read** lock — the 14 RPC handlers (`verify_chain_id`,
  `block_number`, `get_balance`, `call`, `get_transaction_count`, `gas_price`,
  `fee_history`, `estimate_gas`, `send_raw_transaction`, `get_transaction_receipt`,
  `get_transaction_by_hash`, `raw_rpc`, plus `get_chain_config`, `list_chains`). Many
  readers hold it simultaneously, so their blocking round-trips **overlap**.
- `with_rpc_mut(f)` takes a **write** lock — only the **two** config mutators
  (`set_chain_config`, `remove_chain_config`) take it. They are rare and exclusive.
- `client_for` builds a fresh `reqwest::blocking::Client` per call, so concurrent reads
  never share a mutable client.

This is the textbook many-readers/few-writers shape: reads are concurrent, the occasional
config change briefly excludes them.

### 9.2 The generated multi-mode scaffold

In multi mode the generated `provider_gen.rs` stores the instance as a shared
`Arc<dyn Any + Send + Sync>`; the instance `Mutex` guards **construction only**. Each
`logos_module_dispatch` clones the `Arc` and runs the handler on `&self` with **no lock
held**, so calls to one module overlap. No new C ABI is added — concurrency rides on the
existing synchronous `logos_module_dispatch`, which a multi module's host may now call
concurrently from multiple worker threads.

### 9.3 The pending-sentinel round-trip (consumer side)

The producer (this module) stays simple: it returns a JSON string synchronously. The
*consumer's* transport makes it concurrent and transparent:

1. The host dispatches the call on a worker thread; while it's in flight the transport may
   return a **pending sentinel** (a result keyed by the protocol's `pendingCallKey()`)
   instead of blocking the bridge.
2. When the real result is ready, it is delivered as a **completion event** carrying the
   call id.
3. The consumer transport's `resolveDeferred` matches the completion event to the pending
   call and hands the **real** result back to the caller — so the caller's
   `get_balance(...)` simply returns the value, never seeing the sentinel.

This is why the doc-test must build `logoscore` against **logos-protocol ≥ 0.2**: that's
where deferred-reply resolution lives. The mechanism is entirely transport-level —
**neither this module's author code nor its callers' code is aware of it**. (For QtRO this
is implemented in `logos-protocol`'s `qt_remote/remote_transport.cpp`; for the plain
transport in `plain_logos_object.*`.)

### 9.4 Safety summary

- Handlers run on `&self`; the author owns thread-safety via interior mutability
  (here, the `RwLock`).
- Reads (RPC calls) overlap; writes (config changes) are exclusive.
- A per-call fresh HTTP client means no shared mutable network state.
- An old (non-multi-aware) host still loads and forwards the module unmodified — it just
  won't get the concurrency benefit.

---

## 10. Security & invariants (recap)

- **Fail-closed privacy:** `proxyRequired` + no usable proxy ⇒ no request is sent
  (§7). Enforced at the single `build_client` chokepoint; covered by tests at both the
  proxy and rpc layers, and demonstrated live in the doc-test.
- **Single egress point:** `build_client` is the only `reqwest::Client` constructor; all
  traffic inherits its policy.
- **DNS-through-proxy:** `socks5h` is supported and preferred (Tor-ready), avoiding DNS
  leaks.
- **camelCase fidelity:** the serde rename guarantees the backend's `proxyRequired` maps to
  the fail-closed switch — a dedicated regression test guards against a silent fail-open.
- **No key material:** this module never sees private keys (that is `keystore_module`); it
  only broadcasts the already-signed `raw_hex` via `send_raw_transaction`.
- **Persisted, not secret:** `chains.json` holds only endpoints + proxy policy; no secrets.
- **Pure-Rust TLS:** `rustls`, no OpenSSL, no external system lib in the nix closure.

---

## 11. File map

| Path | Purpose |
|------|---------|
| `metadata.json` | Module identity, `concurrency: "multi"`, `codegen.rust` trait pointer. |
| `flake.nix` | Nix build via `logos-module-builder.lib.mkLogosModule`. |
| `CMakeLists.txt` | Thin C++ wrapper (`logos_module(NAME eth_rpc_module)`). |
| `rust-lib/Cargo.toml` | Crate manifest; `reqwest` (rustls + socks + blocking), `logos-rust-sdk` (optional, default feature). |
| `rust-lib/src/lib.rs` | Crate root; module wiring + re-exports. |
| `rust-lib/src/glue.rs` | `EthRpcModule` trait (public API) + `EthRpcModuleImpl` + install hook. |
| `rust-lib/src/rpc.rs` | `EthRpc` client core, `ChainConfig`, `RpcError`, persistence, RPC helpers + tests. |
| `rust-lib/src/proxy.rs` | Fail-closed `build_client` chokepoint (inlined net-proxy) + tests. |
| `rust-lib/generated/provider_gen.rs` | **Generated at build** (multi-mode C-ABI dispatch, `install`, `RustModuleContext`); not checked in. |
| `doctests/eth-rpc-module-runtime.test.yaml` | Executable end-to-end doc-test (logoscore + mock node + fail-closed). |
| `doctests/outputs/eth-rpc-module-runtime.md` | Rendered doc-test walkthrough. |
| `doctests/run.sh` | Runs the doc-test via the shared `logos-doctest` CLI. |
| `.github/workflows/doctests.yml` | CI: runs the doc-test on Ubuntu + macOS, publishes a report. |
