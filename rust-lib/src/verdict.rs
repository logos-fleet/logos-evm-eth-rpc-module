//! The verified-proxy state machine: what state one chain's verified leg is in, whether a
//! verified call would answer now, and what the user has to do about it.
//!
//! [`evaluate`] owns the ORDER: `modules_state` first, the `status()` probe only if the registry
//! did not positively say the module is not loaded.
//!
//! [`GateCache`] owns the other half — how long an answer may be reused, and what a config
//! change invalidates.
//!
//! Pure — a `status()` snapshot or a `modules_state` answer in, a [`Verdict`] out; the cache
//! takes `now` as a parameter. All of it lives outside `glue.rs` so it is testable with
//! `cargo test --no-default-features`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// A `running` proxy with no head yet is starting up for this long, then it is a fault.
pub const HEAD_GRACE_SECS: i64 = 30;
/// How far the verified head may lag the proxy's own clock before it is not tracking.
///
/// Both are derived from the proxy's 1000ms keep-alive beat and `kHeadEveryBeats = 5`: a
/// healthy head lands within ~10s, so these are 3x and 6x headroom. They cannot be scaled off
/// the payload — `statusSnapshot()` emits `keepAlive` (the mode) and not the interval.
pub const HEAD_STALE_SECS: i64 = 60;

/// One chain's verified-leg verdict. `state` and `action` are closed sets, and `action` is an
/// independent field: `unhealthy` alone maps to two different actions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub state: &'static str,
    pub usable: bool,
    pub message: String,
    pub action: &'static str,
    pub detail: String,
}

impl Verdict {
    fn of(state: &'static str, action: &'static str, message: &str) -> Self {
        Self { state, usable: false, message: message.into(), action, detail: String::new() }
    }

    fn detailed(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    pub fn disabled() -> Self {
        Self::of("disabled", "none", "Verification is off. Balances come from the configured endpoint.")
    }

    fn ready() -> Self {
        Self { usable: true, ..Self::of("ready", "none", "Verified.") }
    }

    fn syncing() -> Self {
        Self::of("syncing", "wait", "The verified proxy is starting up.")
    }

    fn missing() -> Self {
        Self::of("missing", "install_or_load", "The Verified Proxy module is not running.")
    }

    fn unconfigured() -> Self {
        Self::of("unconfigured", "open_verified_proxy", "The verified proxy has not been set up yet.")
    }

    fn stopped() -> Self {
        Self::of("stopped", "open_verified_proxy", "The verified proxy is not started.")
    }

    fn wrong_chain(theirs: i64, ours: u64) -> Self {
        Self::of("wrong_chain", "open_verified_proxy", &format!(
            "The verified proxy is on chain {theirs}, not {ours}. It serves one chain at a time."))
    }

    /// Keep-alive off is a distinct fault: over 5 idle minutes on sepolia the verified head
    /// went BACKWARDS 39 blocks, and no restart fixes a configuration choice.
    fn keep_alive_off() -> Self {
        Self::of("unhealthy", "open_verified_proxy",
            "The verified proxy has keep-alive turned off, so its verified head drifts.")
    }

    fn crashed() -> Self {
        Self::of("unhealthy", "restart_or_reload", "The Verified Proxy module stopped unexpectedly.")
    }

    fn not_tracking() -> Self {
        Self::of("unhealthy", "restart_or_reload",
            "The verified proxy is running but not tracking the chain.")
    }

    /// The wire shape every consumer reads. Every key is always present; `detail` is `""`
    /// rather than absent when there is nothing to add.
    pub fn to_json(&self, chain_id: i64, mode_required: bool) -> Value {
        json!({
            "ok": true,
            "chainId": chain_id,
            "mode": if mode_required { "required" } else { "off" },
            "state": self.state,
            "usable": self.usable,
            "blocking": mode_required && !self.usable,
            "message": self.message,
            "action": self.action,
            "detail": self.detail,
        })
    }
}

/// Classify a `verified_proxy_module.status()` snapshot. `status` is declared `-> {tstr: any}`,
/// not `-> result`, so `snapshot` is the object itself and never a `{success,value}` envelope.
pub fn classify_status(chain_id: u64, snapshot: &Value) -> Verdict {
    let last_error = snapshot.get("lastError").and_then(Value::as_str).unwrap_or("");
    let Some(state) = snapshot.get("state").and_then(Value::as_str) else {
        return Verdict::not_tracking().detailed("status carried no state");
    };

    match state {
        "uninitialized" => Verdict::unconfigured(),
        "configured" | "stopped" => Verdict::stopped().detailed(format!("state is '{state}'")),
        "starting" | "stopping" => Verdict::syncing().detailed(format!("state is '{state}'")),
        "error" | "degraded" => Verdict::not_tracking()
            .detailed(if last_error.is_empty() { format!("state is '{state}'") } else { last_error.into() }),
        "running" => classify_running(chain_id, snapshot),
        // Forward-compat: an unrecognised state is named, never a panic and never `ready`.
        other => Verdict::not_tracking().detailed(format!("unrecognised state '{other}'")),
    }
}

fn classify_running(chain_id: u64, snapshot: &Value) -> Verdict {
    // A proxy synced to a DIFFERENT chain answers confidently and wrongly.
    let theirs = snapshot.get("chainId").and_then(Value::as_i64).unwrap_or(0);
    if theirs != chain_id as i64 {
        return Verdict::wrong_chain(theirs, chain_id);
    }
    if snapshot.get("keepAlive").and_then(Value::as_str) == Some("off") {
        return Verdict::keep_alive_off().detailed("keepAlive is 'off'");
    }

    let started_at = snapshot.get("startedAt").and_then(Value::as_i64).unwrap_or(0);
    let uptime = snapshot.get("uptimeSeconds").and_then(Value::as_i64).unwrap_or(0);
    let head_at = snapshot.pointer("/head/updatedAt").and_then(Value::as_i64).unwrap_or(0);
    let head_empty = snapshot.pointer("/head/blockNumber").and_then(Value::as_str)
        .unwrap_or("").is_empty();

    if head_empty {
        // Past the grace this is the restart trap: a second start() sets Running while
        // runOnce() has already cleared the head, and it never refills.
        return if uptime < HEAD_GRACE_SECS {
            Verdict::syncing().detailed(format!("no verified head yet, up {uptime}s"))
        } else {
            Verdict::not_tracking().detailed(format!("no verified head after {uptime}s"))
        };
    }

    // Entirely inside the snapshot's own epoch clock, so our clock's skew cannot matter.
    let lag = (started_at + uptime) - head_at;
    if started_at > 0 && head_at > 0 && lag > HEAD_STALE_SECS {
        return Verdict::not_tracking().detailed(format!("verified head is {lag}s stale"));
    }
    Verdict::ready()
}

/// The module this gate is about.
pub const PROXY_MODULE: &str = "verified_proxy_module";

/// What `modules_state` is able to say about [`PROXY_MODULE`] right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Readiness {
    /// Positively absent or `unloaded` — the only answer allowed to skip the probe.
    NotLoaded(String),
    /// Present in some live state. Loaded is not configured, started, synced or on our chain.
    Loaded,
    /// No information. Never a reason to conclude anything.
    Unknown,
}

/// Read a `modules_state.list_modules()` listing. An EMPTY or `partial` one is NO information,
/// not negative information: a host older than liblogos 84564f0 embeds modules_state unfed, and
/// its empty answer is indistinguishable from "nothing is loaded" — see the spec's §5.5 table.
pub fn classify_readiness(listing: Option<&Value>) -> Readiness {
    let listing = listing.filter(|l| !l.is_null());
    let Some(modules) = listing.and_then(|l| l.get("modules")).and_then(Value::as_array) else {
        return Readiness::Unknown;
    };
    // A listing with no `partial` flag is a shape we do not recognise, so it reads as partial.
    let complete = listing.and_then(|l| l.get("partial")).and_then(Value::as_bool) == Some(false);
    if modules.is_empty() || !complete {
        return Readiness::Unknown;
    }
    let record = modules
        .iter()
        .find(|m| m.get("module").and_then(Value::as_str) == Some(PROXY_MODULE));
    let Some(record) = record else {
        return Readiness::NotLoaded(
            "modules_state lists every module the host knows, and this is not one of them".into());
    };
    match record.get("state").and_then(Value::as_str) {
        Some("unloaded") => Readiness::NotLoaded("modules_state says 'unloaded'".into()),
        // `error` included: a crashed module may already have been restarted, and only the probe
        // knows. Forward-compat rule — an unrecognised state is never negative information.
        Some(_) => Readiness::Loaded,
        None => Readiness::Unknown,
    }
}

/// The gate's three outbound calls, behind a trait so the ORDER they run in is testable without
/// the Logos runtime.
pub trait GateProbe {
    /// `modules_state`, asked first and answered from the host's own registry.
    fn readiness(&self) -> Readiness;
    /// `verified_proxy_module.status()` — the expensive one.
    fn proxy_status(&self) -> std::result::Result<Value, String>;
    /// `modules_state.module_record(PROXY_MODULE)`, only ever after a failed probe.
    fn module_record(&self) -> Option<Value>;
}

/// One chain's verdict, `modules_state` FIRST. A positive "not loaded" skips the probe, which
/// against an absent module is a guaranteed timeout on every poll; anything short of positive
/// falls through and probes, because failing dark on an unfed registry is the worse error.
pub fn evaluate(chain_id: u64, probes: &dyn GateProbe) -> Verdict {
    evaluate_with(chain_id, probes.readiness(), probes)
}

/// The same gate with the readiness already in hand, for a caller that memoizes it and has to
/// know whether the short-circuit fired.
pub fn evaluate_with(chain_id: u64, readiness: Readiness, probes: &dyn GateProbe) -> Verdict {
    if let Readiness::NotLoaded(why) = readiness {
        return Verdict::missing().detailed(why);
    }
    match probes.proxy_status() {
        Ok(snapshot) => classify_status(chain_id, &snapshot),
        Err(e) => classify_modules_state(probes.module_record().as_ref(), &e),
    }
}

/// Refine a FAILED `status()` probe with what the host thinks. Only ever reached after the probe
/// failed, so it can sharpen the reason but never veto a proxy we could reach.
pub fn classify_modules_state(record: Option<&Value>, probe_error: &str) -> Verdict {
    let state = record
        .filter(|r| !r.is_null())
        .and_then(|r| r.get("state"))
        .and_then(Value::as_str);
    let Some(state) = state else {
        return Verdict::missing()
            .detailed(format!("status() did not answer ({probe_error}); modules_state could not confirm it"));
    };
    let reason = record.and_then(|r| r.get("reason")).and_then(Value::as_str).unwrap_or("");

    match state {
        "unloaded" => Verdict::missing().detailed("modules_state says 'unloaded'"),
        "error" => Verdict::crashed()
            .detailed(if reason.is_empty() { "modules_state says 'error'".into() } else { reason.to_string() }),
        "loading" | "loaded" => Verdict::syncing().detailed(format!("modules_state says '{state}'")),
        "ready" => Verdict::not_tracking()
            .detailed(format!("modules_state says 'ready' but status() did not answer ({probe_error})")),
        // Forward-compat rule: fall back to "not loaded", never error out.
        other => Verdict::missing().detailed(format!("modules_state reported unrecognised state '{other}'")),
    }
}

// ── the memo ──────────────────────────────────────────────────────────────────────────

/// How long `modules_state`'s answer is reused. Short — a module can load at any moment — and
/// separate from `HEALTH_TTL` because readiness is a HOST fact, not a per-chain one: a wallet
/// polling three chains shares one lookup instead of taking three.
pub const READY_TTL: Duration = Duration::from_secs(2);

/// How long a verdict is reused: long enough that a burst of reads (and a polling UI) costs one
/// probe, short enough that a proxy falling over is noticed within seconds.
pub const HEALTH_TTL: Duration = Duration::from_secs(5);

/// What is left of a caller's budget once the gate has spent its own, or `None` when too
/// little remains to be worth asking.
///
/// The gate runs on ITS OWN constants and is never shortened by a caller. A probe truncated
/// to fit one caller's budget answers `Unknown`, and `verdict_at` then CACHES that for
/// `HEALTH_TTL` and serves it to every other caller and to the UI's own poll — one impatient
/// reader would turn verified routing off for everybody. So the gate is paid for OUT of the
/// budget, not squeezed into it.
pub fn hop_budget(budget: Duration, gate_spent: Duration, floor: Duration) -> Option<Duration> {
    let left = budget.saturating_sub(gate_spent);
    (left >= floor).then_some(left)
}

/// One verdict per chain and ONE readiness snapshot for the host.
#[derive(Default)]
pub struct GateCache {
    health: Mutex<HashMap<u64, (Instant, Verdict)>>,
    ready: Mutex<Option<(Instant, Readiness)>>,
}

impl GateCache {
    pub fn verdict(&self, chain_id: u64, probes: &dyn GateProbe) -> Verdict {
        self.verdict_at(Instant::now(), chain_id, probes)
    }

    /// `now` is a parameter so every TTL here is testable without sleeping.
    pub fn verdict_at(&self, now: Instant, chain_id: u64, probes: &dyn GateProbe) -> Verdict {
        if let Some(v) = self.cached(now, chain_id) {
            return v;
        }
        let readiness = self.readiness_at(now, probes);
        // A `missing` the readiness short-circuit produced costs nothing to recompute — the
        // listing behind it is itself memoized — so caching it on TOP of that would only keep
        // telling a user who has just loaded the proxy that it is absent.
        let cacheable = !matches!(readiness, Readiness::NotLoaded(_));
        let v = evaluate_with(chain_id, readiness, probes);
        if cacheable {
            if let Ok(mut c) = self.health.lock() {
                c.insert(chain_id, (now, v.clone()));
            }
        }
        v
    }

    fn cached(&self, now: Instant, chain_id: u64) -> Option<Verdict> {
        let c = self.health.lock().ok()?;
        c.get(&chain_id)
            .filter(|(at, _)| now.saturating_duration_since(*at) < HEALTH_TTL)
            .map(|(_, v)| v.clone())
    }

    /// `modules_state`'s answer, reused for `READY_TTL`.
    fn readiness_at(&self, now: Instant, probes: &dyn GateProbe) -> Readiness {
        if let Ok(c) = self.ready.lock() {
            if let Some((at, r)) = c.as_ref() {
                if now.saturating_duration_since(*at) < READY_TTL {
                    return r.clone();
                }
            }
        }
        let r = probes.readiness();
        if let Ok(mut c) = self.ready.lock() {
            *c = Some((now, r.clone()));
        }
        r
    }

    /// Drop what a config write can have invalidated: the chain's verdict AND the readiness
    /// snapshot under it. Dropping only the verdict left a retry after "install or load the
    /// proxy" reading a 2s-old "not loaded" and answering `missing` anyway.
    pub fn invalidate(&self, chain_id: u64) {
        if let Ok(mut c) = self.health.lock() {
            c.remove(&chain_id);
        }
        if let Ok(mut c) = self.ready.lock() {
            *c = None;
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn the_gate_is_paid_for_out_of_the_budget_rather_than_squeezed_into_it() {
        let floor = Duration::from_millis(50);
        assert_eq!(
            hop_budget(Duration::from_millis(3000), Duration::from_millis(400), floor),
            Some(Duration::from_millis(2600)),
            "what the gate spent comes off the top"
        );
    }

    #[test]
    fn a_budget_the_gate_has_already_eaten_refuses_rather_than_asking_with_nothing_left() {
        // Asking with 3ms left is how a truncated probe caches Unknown for HEALTH_TTL and
        // turns verified routing off for every other reader, including the UI's own poll.
        let floor = Duration::from_millis(50);
        assert_eq!(hop_budget(Duration::from_millis(400), Duration::from_millis(397), floor), None);
        assert_eq!(hop_budget(Duration::from_millis(10), Duration::from_secs(9), floor), None,
                   "and an overspent gate saturates rather than wrapping");
    }
    use super::*;

    /// A perfect snapshot, in the exact shape `ProxyRuntime::statusSnapshot()` emits.
    fn running(chain_id: i64) -> Value {
        json!({
            "state": "running", "network": "sepolia", "chainId": chain_id,
            "startedAt": 1_700_000_000i64, "uptimeSeconds": 120,
            "lastError": "",
            "head": { "blockNumber": "0x5f5e100", "updatedAt": 1_700_000_115i64 },
            "keepAlive": "on",
        })
    }

    #[test]
    fn a_running_proxy_on_our_chain_with_a_fresh_head_is_the_only_usable_verdict() {
        let v = classify_status(11155111, &running(11155111));
        assert_eq!((v.state, v.usable, v.action), ("ready", true, "none"));
        assert_eq!(v.detail, "", "detail is empty, never absent");
    }

    #[test]
    fn a_proxy_on_another_chain_answers_confidently_and_wrongly_so_it_is_refused() {
        let v = classify_status(11155111, &running(1));
        assert_eq!((v.state, v.usable, v.action), ("wrong_chain", false, "open_verified_proxy"));
        assert!(v.message.contains("chain 1") && v.message.contains("11155111"), "{}", v.message);
    }

    #[test]
    fn an_empty_head_is_start_up_inside_the_grace_and_a_fault_outside_it() {
        let mut early = running(1);
        early["head"]["blockNumber"] = json!("");
        early["uptimeSeconds"] = json!(12);
        assert_eq!(classify_status(1, &early).state, "syncing");
        assert_eq!(classify_status(1, &early).action, "wait");

        // The restart trap: start-stop-start leaves Running with a head that never refills.
        let mut trapped = early.clone();
        trapped["uptimeSeconds"] = json!(300);
        let v = classify_status(1, &trapped);
        assert_eq!((v.state, v.usable, v.action), ("unhealthy", false, "restart_or_reload"));
        assert!(v.detail.contains("300s"), "{}", v.detail);
    }

    #[test]
    fn a_frozen_head_is_caught_even_though_the_heartbeat_stays_green() {
        // noteHeadProbe discards a FAILED probe, so blockNumber freezes while eth_syncing
        // keeps answering. Only updatedAt tells the difference.
        let mut stale = running(1);
        stale["head"]["updatedAt"] = json!(1_700_000_000i64 - 80);
        let v = classify_status(1, &stale);
        assert_eq!((v.state, v.usable), ("unhealthy", false));
        assert!(v.detail.contains("stale"), "{}", v.detail);
    }

    #[test]
    fn staleness_is_measured_against_the_proxys_own_clock_not_ours() {
        // startedAt is 20 years in the future; a verdict computed against our clock would
        // call this fresh head impossibly stale.
        let mut skewed = running(1);
        skewed["startedAt"] = json!(2_300_000_000i64);
        skewed["head"]["updatedAt"] = json!(2_300_000_115i64);
        assert_eq!(classify_status(1, &skewed).state, "ready");
    }

    #[test]
    fn keep_alive_off_is_its_own_fault_because_no_restart_fixes_it() {
        let mut drifting = running(1);
        drifting["keepAlive"] = json!("off");
        let v = classify_status(1, &drifting);
        assert_eq!((v.state, v.usable, v.action), ("unhealthy", false, "open_verified_proxy"));
    }

    #[test]
    fn the_lifecycle_states_map_to_their_own_verdicts() {
        let cases = [
            ("uninitialized", "unconfigured", "open_verified_proxy"),
            ("configured", "stopped", "open_verified_proxy"),
            ("stopped", "stopped", "open_verified_proxy"),
            ("starting", "syncing", "wait"),
            ("stopping", "syncing", "wait"),
            ("degraded", "unhealthy", "restart_or_reload"),
            ("error", "unhealthy", "restart_or_reload"),
        ];
        for (wire, state, action) in cases {
            let v = classify_status(1, &json!({ "state": wire, "lastError": "" }));
            assert_eq!((v.state, v.action), (state, action), "{wire}");
            assert!(!v.usable, "{wire}");
        }
    }

    #[test]
    fn an_unrecognised_state_is_named_and_never_panics_or_passes() {
        let v = classify_status(1, &json!({ "state": "quantum" }));
        assert_eq!((v.state, v.usable), ("unhealthy", false));
        assert!(v.detail.contains("quantum"), "{}", v.detail);
        // A snapshot with no state at all is the same class of unknown.
        assert_eq!(classify_status(1, &json!({})).state, "unhealthy");
    }

    #[test]
    fn a_last_error_is_carried_into_detail() {
        let v = classify_status(1, &json!({ "state": "degraded", "lastError": "heartbeat failed x3" }));
        assert_eq!(v.detail, "heartbeat failed x3");
    }

    #[test]
    fn modules_state_only_ever_sharpens_the_reason_a_failed_probe_already_gave() {
        // No modules_state at all, or a null record: exactly what the failed probe implied.
        for record in [None, Some(&Value::Null)] {
            let v = classify_modules_state(record, "not reachable");
            assert_eq!((v.state, v.action), ("missing", "install_or_load"));
            assert!(v.detail.contains("not reachable"), "{}", v.detail);
        }

        let cases = [
            ("unloaded", "missing", "install_or_load"),
            ("loading", "syncing", "wait"),
            ("loaded", "syncing", "wait"),
            ("error", "unhealthy", "restart_or_reload"),
            ("ready", "unhealthy", "restart_or_reload"),
            ("stopping", "missing", "install_or_load"),
        ];
        for (wire, state, action) in cases {
            let rec = json!({ "module": "verified_proxy_module", "state": wire });
            let v = classify_modules_state(Some(&rec), "not reachable");
            assert_eq!((v.state, v.action), (state, action), "{wire}");
            assert!(!v.usable, "{wire}");
        }
    }

    #[test]
    fn a_crashed_module_reports_the_hosts_reason() {
        let rec = json!({ "state": "error", "reason": "exited with signal 11" });
        let v = classify_modules_state(Some(&rec), "timed out");
        assert_eq!(v.message, "The Verified Proxy module stopped unexpectedly.");
        assert_eq!(v.detail, "exited with signal 11");
    }

    #[test]
    fn blocking_is_exactly_required_and_not_usable() {
        let ready = classify_status(1, &running(1));
        assert_eq!(ready.to_json(1, true)["blocking"], json!(false));
        assert_eq!(ready.to_json(1, false)["blocking"], json!(false));

        let broken = classify_status(1, &json!({ "state": "stopped" }));
        assert_eq!(broken.to_json(1, true)["blocking"], json!(true));
        // Verification off: not usable, but chain data must still show.
        assert_eq!(broken.to_json(1, false)["blocking"], json!(false));
        assert_eq!(Verdict::disabled().to_json(1, false)["blocking"], json!(false));
    }

    #[test]
    fn every_verdict_stays_inside_the_two_closed_sets() {
        const STATES: [&str; 8] = ["disabled", "ready", "syncing", "missing", "unconfigured",
                                   "stopped", "wrong_chain", "unhealthy"];
        const ACTIONS: [&str; 5] = ["none", "wait", "install_or_load", "open_verified_proxy",
                                    "restart_or_reload"];
        let mut all = vec![Verdict::disabled(), classify_status(1, &running(1))];
        for wire in ["uninitialized", "configured", "stopped", "starting", "stopping",
                     "degraded", "error", "quantum", "running"] {
            all.push(classify_status(9, &json!({ "state": wire, "keepAlive": "on" })));
        }
        for wire in ["unloaded", "loading", "loaded", "ready", "error", "quantum"] {
            all.push(classify_modules_state(Some(&json!({ "state": wire })), "x"));
        }
        for v in all {
            assert!(STATES.contains(&v.state), "{}", v.state);
            assert!(ACTIONS.contains(&v.action), "{}", v.action);
            assert!(!v.message.is_empty(), "{}", v.state);
        }
    }

    #[test]
    fn the_wire_shape_always_carries_every_key() {
        let j = Verdict::disabled().to_json(11155111, false);
        for k in ["ok", "chainId", "mode", "state", "usable", "blocking", "message", "action", "detail"] {
            assert!(j.get(k).is_some(), "missing {k}");
        }
        assert_eq!(j["mode"], json!("off"));
        assert_eq!(j["ok"], json!(true), "a reachable outcome is ok:true even when unusable");
    }

    // ── the ordering: modules_state first, the probe only when it has to run ──────────────

    /// A complete listing always carries a second module, so "not empty" and "the proxy is in
    /// it" stay independent.
    fn listing(proxy_state: Option<&str>, partial: bool) -> Value {
        let mut mods = vec![json!({ "module": "eth_rpc_module", "state": "ready" })];
        if let Some(st) = proxy_state {
            mods.push(json!({ "module": PROXY_MODULE, "state": st }));
        }
        json!({ "modules": mods, "partial": partial, "seq": 7 })
    }

    /// Stands in for the two host modules and COUNTS what the gate actually calls.
    struct Spy {
        listing: std::cell::RefCell<Option<Value>>,
        status: std::result::Result<Value, String>,
        record: Option<Value>,
        probes: std::cell::Cell<usize>,
        records: std::cell::Cell<usize>,
        reads: std::cell::Cell<usize>,
    }

    impl Spy {
        fn new(listing: Option<Value>) -> Self {
            Self {
                listing: listing.into(),
                status: Ok(running(1)),
                record: None,
                probes: 0.into(),
                records: 0.into(),
                reads: 0.into(),
            }
        }
        /// The proxy is installed and loaded between two polls.
        fn loads(&self) {
            *self.listing.borrow_mut() = Some(listing(Some("ready"), false));
        }
    }

    impl GateProbe for Spy {
        fn readiness(&self) -> Readiness {
            self.reads.set(self.reads.get() + 1);
            classify_readiness(self.listing.borrow().as_ref())
        }
        fn proxy_status(&self) -> std::result::Result<Value, String> {
            self.probes.set(self.probes.get() + 1);
            self.status.clone()
        }
        fn module_record(&self) -> Option<Value> {
            self.records.set(self.records.get() + 1);
            self.record.clone()
        }
    }

    #[test]
    fn a_proxy_the_host_says_is_not_loaded_costs_zero_probes() {
        // Exactly the frozen case: required mode, no proxy, a UI polling on the cache cadence.
        for absent in [None, Some("unloaded")] {
            let spy = Spy::new(Some(listing(absent, false)));
            let v = evaluate(1, &spy);
            assert_eq!((v.state, v.action), ("missing", "install_or_load"), "{absent:?}");
            assert_eq!(spy.probes.get(), 0, "the 1500ms status probe must not run");
            assert_eq!(spy.records.get(), 0, "and neither must the refinement");
            assert!(!v.detail.is_empty(), "the verdict still says why");
        }
    }

    #[test]
    fn a_registry_with_nothing_to_say_still_probes() {
        // Every one of these is indistinguishable from a working host whose feed is missing.
        let cases = [
            ("unfed: empty and complete", json!({ "modules": [], "partial": false, "seq": 0 })),
            ("unfed: empty and partial", json!({ "modules": [], "partial": true, "seq": 0 })),
            ("partial, proxy not in it", listing(None, true)),
            ("no partial flag", json!({ "modules": [{ "module": "x", "state": "ready" }] })),
            ("not a listing at all", json!({ "ok": true })),
            ("null", Value::Null),
        ];
        for (name, l) in cases {
            let spy = Spy::new(Some(l));
            assert_eq!(evaluate(1, &spy).state, "ready", "{name}");
            assert_eq!(spy.probes.get(), 1, "{name}");
        }
        // modules_state absent or erroring answers None, and that is not negative either.
        let spy = Spy::new(None);
        assert_eq!(evaluate(1, &spy).state, "ready");
        assert_eq!(spy.probes.get(), 1);
    }

    #[test]
    fn loaded_is_not_configured_started_or_synced_so_it_is_still_probed() {
        for state in ["loading", "loaded", "ready", "stopping", "error", "quantum"] {
            assert_eq!(classify_readiness(Some(&listing(Some(state), false))), Readiness::Loaded,
                       "{state}");
        }
        // A registry saying 'ready' cannot make a stopped proxy usable.
        let mut spy = Spy::new(Some(listing(Some("ready"), false)));
        spy.status = Ok(json!({ "state": "stopped" }));
        let v = evaluate(1, &spy);
        assert_eq!((v.state, v.usable, v.action), ("stopped", false, "open_verified_proxy"));
        assert_eq!(spy.probes.get(), 1);
    }

    // ── the memo: what may be reused, and for how long ───────────────────────────────────

    #[test]
    fn a_verdict_the_probe_paid_for_is_reused_inside_health_ttl() {
        let spy = Spy::new(Some(listing(Some("ready"), false)));
        let cache = GateCache::default();
        let t0 = Instant::now();
        assert_eq!(cache.verdict_at(t0, 1, &spy).state, "ready");
        assert_eq!(cache.verdict_at(t0 + Duration::from_secs(4), 1, &spy).state, "ready");
        assert_eq!(spy.probes.get(), 1, "the 1500ms probe must not run twice inside 5s");
        cache.verdict_at(t0 + HEALTH_TTL, 1, &spy);
        assert_eq!(spy.probes.get(), 2, "and must run again once the verdict is stale");
    }

    #[test]
    fn a_missing_verdict_the_probe_paid_for_is_cached_like_any_other() {
        // The registry had nothing to say, so the full 1500ms + 750ms ran. Recomputing THAT on
        // every poll is what HEALTH_TTL exists to prevent.
        let mut spy = Spy::new(Some(listing(None, true)));
        spy.status = Err("timed out".into());
        let cache = GateCache::default();
        let t0 = Instant::now();
        assert_eq!(cache.verdict_at(t0, 1, &spy).state, "missing");
        assert_eq!(cache.verdict_at(t0 + Duration::from_secs(4), 1, &spy).state, "missing");
        assert_eq!(spy.probes.get(), 1);
    }

    #[test]
    fn a_proxy_that_has_just_loaded_is_seen_within_ready_ttl_not_ready_plus_health() {
        // A `missing` from the short-circuit used to be cached for HEALTH_TTL on top of the
        // READY_TTL listing that produced it, so a user who installed the proxy and hit retry
        // was told it was absent for up to seven seconds.
        let spy = Spy::new(Some(listing(None, false)));
        let cache = GateCache::default();
        let t0 = Instant::now();
        assert_eq!(cache.verdict_at(t0, 1, &spy).state, "missing");
        assert_eq!(cache.verdict_at(t0 + Duration::from_secs(1), 1, &spy).state, "missing");
        assert_eq!((spy.reads.get(), spy.probes.get()), (1, 0), "and it stays free to recompute");

        spy.loads();
        assert_eq!(cache.verdict_at(t0 + READY_TTL, 1, &spy).state, "ready",
                   "a missing verdict must not outlive the listing it rests on");
    }

    #[test]
    fn invalidating_a_chain_drops_the_host_readiness_that_verdict_rested_on() {
        let spy = Spy::new(Some(listing(None, false)));
        let cache = GateCache::default();
        let t0 = Instant::now();
        assert_eq!(cache.verdict_at(t0, 1, &spy).state, "missing");

        // The config write a retry makes, in the same instant: clearing only the verdict left
        // the two-second-old "not loaded" to answer it again.
        spy.loads();
        cache.invalidate(1);
        assert_eq!(cache.verdict_at(t0, 1, &spy).state, "ready");
        assert_eq!(spy.reads.get(), 2);
    }

    #[test]
    fn one_readiness_lookup_serves_every_chain_a_wallet_polls() {
        let spy = Spy::new(Some(listing(Some("ready"), false)));
        let cache = GateCache::default();
        let t0 = Instant::now();
        for chain in [1u64, 5, 9] {
            cache.verdict_at(t0, chain, &spy);
        }
        assert_eq!(spy.reads.get(), 1, "readiness is a host fact, not a per-chain one");
        assert_eq!(spy.probes.get(), 3, "the verdict is not: each chain gets its own");
    }

    #[test]
    fn a_failed_probe_is_still_refined_by_the_record() {
        let mut spy = Spy::new(Some(listing(Some("loaded"), false)));
        spy.status = Err("timed out".into());
        spy.record = Some(json!({ "state": "error", "reason": "exited with signal 11" }));
        let v = evaluate(1, &spy);
        assert_eq!((v.state, v.action), ("unhealthy", "restart_or_reload"));
        assert_eq!(v.detail, "exited with signal 11");
        assert_eq!((spy.probes.get(), spy.records.get()), (1, 1));
    }
}
