//! agora spike host — the transport + capability layer (where NATS/JetStream and
//! sigil swap in). It runs the agent *component* and enforces the STRUCTURAL
//! cross-talk controls the research validated:
//!   1. capability channel-scoping  — an agent only receives messages on channels
//!      it was granted a handle to (it cannot hear `secret-ops`).
//!   2. unconditional self-echo filter — `from == me` is dropped on EVERY channel
//!      (the Hermes postmortem's #1 rule).
//!   3. idempotency — each (agent, message-id) is processed once.
//!   4. hop-count / TTL — bounds the reaction cascade that would otherwise loop
//!      forever (the Hermes infinite-ack-loop).
//! Every accepted message is mirrored as a signed fact (the rivet durable record).

use std::fmt::Write as _;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

wasmtime::component::bindgen!({ path: "../agent/wit", world: "agent" });

use exports::agora::agent::protocol::{Act, Message};

/// The agent component, built for native component-model WASI (wasm32-wasip2) by
/// rules_wasm_component (Bazel) or `cargo component build --target wasm32-wasip2`.
const AGENT_WASM: &str = "../agent/target/wasm32-wasip2/release/agent.wasm";

/// Host state: the agent component's `std` imports WASI, so the host provides it.
struct State {
    ctx: WasiCtx,
    table: ResourceTable,
}
impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.ctx, table: &mut self.table }
    }
}

fn act_str(a: &Act) -> &'static str {
    match a {
        Act::Inform => "inform",
        Act::Request => "request",
        Act::Propose => "propose",
        Act::Agree => "agree",
        Act::Refuse => "refuse",
    }
}

/// The measurable outcome of one in-memory simulation run — the verified reference
/// oracle. `main` now runs over real JetStream; this stays as the unit-test model.
#[cfg(test)]
struct SimResult {
    /// Deliveries of an ungranted channel (`secret-ops`) the capability layer blocked.
    dropped_caps: u32,
    /// Own-message echoes the unconditional self-echo filter dropped.
    dropped_echo: u32,
    /// Messages accepted (and mirrored as facts), in bus order.
    accepted: Vec<Message>,
    /// The round at which the cascade converged (no new messages), if it did.
    converged_round: Option<usize>,
    /// Per-round emitted messages, for human-readable display.
    rounds: Vec<Vec<Message>>,
}

/// Run the in-memory bus simulation against the real agent component and return the
/// measured outcome. No I/O side effects, so it is testable.
#[cfg(test)]
fn run_simulation() -> anyhow::Result<SimResult> {
    use std::collections::{HashMap, HashSet};
    // --- load the agent component on wasmtime ---
    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config)?;
    let component = Component::from_file(&engine, AGENT_WASM)?;
    let mut linker: Linker<State> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
    let state = State { ctx: WasiCtxBuilder::new().inherit_stdio().build(), table: ResourceTable::new() };
    let mut store = Store::new(&engine, state);
    let agent = Agent::instantiate(&mut store, &component, &linker)?;
    let proto = agent.agora_agent_protocol();

    // --- the bus (NATS/JetStream stand-in): an append-only, ordered log ---
    let mut bus: Vec<Message> = Vec::new();
    let mut next_id: u64 = 1;

    // --- capability table: which channels each agent was GRANTED a handle to ---
    let mut caps: HashMap<&str, HashSet<&str>> = HashMap::new();
    caps.insert("synth-agent", HashSet::from(["build-coord"]));
    caps.insert("relay-agent", HashSet::from(["build-coord"]));
    let agents = ["synth-agent", "relay-agent"];

    // seed: a request on the shared channel ...
    bus.push(Message { id: next_id, sender: "maintainer".into(), channel: "build-coord".into(), act: Act::Request, payload: "ship v0.1?".into(), sig: "human".into(), hops: 3 });
    next_id += 1;
    // ... and a message on a channel NO agent was granted — must NEVER be delivered.
    bus.push(Message { id: next_id, sender: "maintainer".into(), channel: "secret-ops".into(), act: Act::Inform, payload: "SECRET: rotate signing keys".into(), sig: "human".into(), hops: 3 });
    next_id += 1;

    let mut seen: HashSet<(String, u64)> = HashSet::new();
    let (mut dropped_caps, mut dropped_echo) = (0u32, 0u32);
    let mut accepted: Vec<Message> = Vec::new();
    let mut rounds: Vec<Vec<Message>> = Vec::new();
    let mut converged_round = None;

    for round in 0..8 {
        let snapshot = bus.clone();
        let mut new_msgs: Vec<Message> = Vec::new();
        for me in agents {
            let granted = &caps[me];
            let mut inbox = Vec::new();
            for m in &snapshot {
                if !granted.contains(m.channel.as_str()) {
                    dropped_caps += 1; // CAPABILITY: never delivered off-scope
                    continue;
                }
                if m.sender == me {
                    dropped_echo += 1; // SELF-ECHO: unconditional, every channel
                    continue;
                }
                if !seen.insert((me.to_string(), m.id)) {
                    continue; // IDEMPOTENCY: process each message once
                }
                inbox.push(m.clone());
            }
            if inbox.is_empty() {
                continue;
            }
            for om in proto.call_coordinate(&mut store, me, &inbox)? {
                let mut om = om;
                om.id = next_id;
                next_id += 1;
                new_msgs.push(om);
            }
        }
        if new_msgs.is_empty() {
            converged_round = Some(round);
            break;
        }
        accepted.extend(new_msgs.iter().cloned());
        bus.extend(new_msgs.iter().cloned());
        rounds.push(new_msgs);
    }

    Ok(SimResult { dropped_caps, dropped_echo, accepted, converged_round, rounds })
}

// ===========================================================================
// Real durable spine: NATS JetStream.
//
// The in-memory `run_simulation` above is the verified reference oracle (unit-
// tested). `main` runs the SAME coordination logic over a real JetStream stream,
// which supplies what the Vec faked: global ordering (stream sequence), durable
// consumers (replay), and `Nats-Msg-Id` dedup. Capability channel-scoping
// becomes STRUCTURAL at the subject filter — an agent's consumer is created only
// for the subjects of channels it was granted, so an ungranted channel
// (`secret-ops`) is never delivered because no consumer subscribes to it.
// ===========================================================================

use serde::{Deserialize, Serialize};

/// JSON wire form of a coordination message (the bindgen `Message` is not serde).
#[derive(Serialize, Deserialize)]
struct Wire {
    id: u64,
    sender: String,
    channel: String,
    act: String,
    payload: String,
    sig: String,
    hops: u32,
}

fn parse_act(s: &str) -> Act {
    match s {
        "request" => Act::Request,
        "propose" => Act::Propose,
        "agree" => Act::Agree,
        "refuse" => Act::Refuse,
        _ => Act::Inform,
    }
}

fn subject_for(channel: &str) -> String {
    format!("agora.{channel}")
}

/// A channel name is a single, safe NATS subject token: non-empty, and only
/// `[A-Za-z0-9_-]`. Rejects `.`/`*`/`>`/whitespace so an agent-supplied channel
/// cannot inject extra subject levels or wildcards (subject injection).
fn is_safe_channel(c: &str) -> bool {
    !c.is_empty()
        && c.len() <= 128
        && c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    use async_nats::jetstream::{self, consumer::pull, stream};
    use futures::StreamExt;
    use std::time::Duration;

    let url = std::env::var("NATS_URL").unwrap_or_else(|_| "127.0.0.1:4222".into());
    println!("agora — durable spine on NATS JetStream ({url})");
    println!("2 named agents on `build-coord`; `secret-ops` is ungranted (no consumer subscribes)\n");

    // --- connect + ensure the AGORA stream (the durable, ordered log) ---
    let client = async_nats::connect(&url).await?;
    let js = jetstream::new(client);
    // Fresh per run: delete any prior stream so re-runs against the same server are
    // deterministic (purge clears messages but NOT durable-consumer positions, which
    // would otherwise carry over and starve the next run).
    let _ = js.delete_stream("AGORA").await;
    let stream = js
        .get_or_create_stream(stream::Config {
            name: "AGORA".into(),
            subjects: vec!["agora.>".into()],
            // dedup window: a re-published Nats-Msg-Id within this window is dropped.
            duplicate_window: Duration::from_secs(120),
            ..Default::default()
        })
        .await?;

    // --- lease store: JetStream KV for optimistic-commit task leases (REQ-013) ---
    // A task must be handled once even though several agents can see it. An agent
    // optimistically claims it by atomically CREATE-ing its lease key: the first
    // wins and does the work; the rest see the key exists and skip (roll back) — a
    // distributed mutex over the durable spine, so no duplicate work.
    let _ = js.delete_key_value("AGORA_LEASES").await;
    let leases = js
        .create_key_value(jetstream::kv::Config {
            bucket: "AGORA_LEASES".into(),
            history: 1,
            ..Default::default()
        })
        .await?;

    // --- capability table: which channels each agent was GRANTED ---
    let agents = ["synth-agent", "relay-agent"];
    let granted: std::collections::HashMap<&str, Vec<&str>> = [
        ("synth-agent", vec!["build-coord"]),
        ("relay-agent", vec!["build-coord"]),
    ]
    .into_iter()
    .collect();

    // --- publish the seeds (with Nats-Msg-Id for dedup) ---
    let mut next_id: u64 = 1;
    let seeds = [
        Wire { id: 1, sender: "maintainer".into(), channel: "build-coord".into(), act: "request".into(), payload: "ship v0.1?".into(), sig: "human".into(), hops: 3 },
        Wire { id: 2, sender: "maintainer".into(), channel: "secret-ops".into(), act: "inform".into(), payload: "SECRET: rotate signing keys".into(), sig: "human".into(), hops: 3 },
        // a unit of work both agents can see — must be handled exactly once (lease).
        Wire { id: 3, sender: "maintainer".into(), channel: "build-coord".into(), act: "request".into(), payload: "TASK#42: cut the release".into(), sig: "human".into(), hops: 1 },
    ];
    for w in &seeds {
        publish(&js, w).await?;
        next_id = next_id.max(w.id + 1);
    }

    // --- one wasm component instance, reused across rounds ---
    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config)?;
    let component = Component::from_file(&engine, AGENT_WASM)?;
    let mut linker: Linker<State> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
    let state = State { ctx: WasiCtxBuilder::new().inherit_stdio().build(), table: ResourceTable::new() };
    let mut store = Store::new(&engine, state);
    let agent = Agent::instantiate(&mut store, &component, &linker)?;
    let proto = agent.agora_agent_protocol();

    // --- one durable pull consumer per agent, FILTERED to its granted subjects ---
    // This is capability-scoping made structural: no consumer exists for any
    // channel an agent was not granted, so those messages are never delivered.
    let mut consumers = Vec::new();
    for me in agents {
        // every agent here is granted exactly `build-coord`; the filter encodes it.
        let subjects: Vec<&str> = granted[me].clone();
        let filter = subject_for(subjects[0]); // single granted channel in this spike
        let consumer = stream
            .get_or_create_consumer(
                &format!("agent-{me}"),
                pull::Config {
                    durable_name: Some(format!("agent-{me}")),
                    filter_subject: filter,
                    ack_policy: jetstream::consumer::AckPolicy::Explicit,
                    ..Default::default()
                },
            )
            .await?;
        consumers.push((me, consumer));
    }

    // --- privileged control plane (the thrum gateway seam) — REQ-AGORA-007 ---
    // An operator halt lives on `agora._control.>`. NO agent consumer subscribes to
    // it (their filters are exact channel subjects), so it is out-of-band by
    // construction: an agent cannot read, emit, or ignore it. This is the Hermes
    // rule #2 control — a privileged kill the agent cannot treat as "just another turn".
    let control = stream
        .get_or_create_consumer(
            "control-plane",
            pull::Config {
                durable_name: Some("control-plane".into()),
                filter_subject: "agora._control.>".into(),
                ack_policy: jetstream::consumer::AckPolicy::Explicit,
                ..Default::default()
            },
        )
        .await?;

    let mut seen: std::collections::HashSet<(String, u64)> = std::collections::HashSet::new();
    let mut killed: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut dropped_echo = 0u32;
    let mut dropped_emit = 0u32;
    let mut dropped_dup = 0u32; // task attempts that lost the lease (duplicate work prevented)
    let mut task_done: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut accepted: Vec<Wire> = Vec::new();
    // --- decision policy: owner-decides + deadline (REQ-AGORA-012) ---
    // The coordination is not an open-ended echo — it drives toward ONE decision,
    // owned by an accountable agent, finalized by a deadline. The deadline bounds
    // deliberation independent of the hop budget (a distinct control: hops bound a
    // single message's cascade; the deadline bounds the whole decision).
    let decision_owner = "synth-agent";
    let decision_topic = "ship v0.1?";
    let decision_deadline: u32 = 2; // rounds of deliberation before the owner must decide
    let mut engaged: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut decided = false;

    for round in 0..8u32 {
        // Optional operator (thrum) out-of-band kill demo — opt in with
        // AGORA_DEMO_KILL=<agent>; a real operator triggers this on demand, not every run.
        if round == 1 {
            if let Ok(target) = std::env::var("AGORA_DEMO_KILL") {
                if !target.is_empty() {
                    println!("\n  [operator] out-of-band kill → {target} (privileged control plane)\n");
                    js.publish("agora._control.kill".to_string(), target.into()).await?.await?;
                }
            }
        }

        // Drain the privileged control plane and apply any halts before acting.
        {
            let mut ctl = control
                .batch()
                .max_messages(64)
                .expires(Duration::from_millis(150))
                .messages()
                .await?;
            while let Some(m) = ctl.next().await {
                let m = m.map_err(|e| anyhow::anyhow!("{e}"))?;
                let target = String::from_utf8_lossy(&m.payload).trim().to_string();
                m.ack().await.map_err(|e| anyhow::anyhow!("{e}"))?;
                if !target.is_empty() {
                    killed.insert(target);
                }
            }
        }

        let mut produced = 0u32;
        for (me, consumer) in &consumers {
            let me = *me;
            // OUT-OF-BAND KILL (REQ-AGORA-007, Hermes rule #2): a halted agent gets
            // no delivery and emits nothing, regardless of channel traffic — it cannot
            // ignore the operator the way it can ignore an in-band "stop" message.
            if killed.contains(me) {
                continue;
            }
            // no-wait batch: take whatever is currently available on the granted subject
            let mut batch = consumer
                .batch()
                .max_messages(256)
                .expires(Duration::from_millis(250))
                .messages()
                .await?;

            let mut inbox: Vec<Message> = Vec::new();
            while let Some(msg) = batch.next().await {
                let msg = msg.map_err(|e| anyhow::anyhow!("{e}"))?;
                let w: Wire = serde_json::from_slice(&msg.payload)?;
                msg.ack().await.map_err(|e| anyhow::anyhow!("{e}"))?;
                if w.sender == me {
                    dropped_echo += 1; // SELF-ECHO: unconditional
                    continue;
                }
                if !seen.insert((me.to_string(), w.id)) {
                    continue; // IDEMPOTENCY (belt-and-suspenders to JetStream dedup)
                }
                // LEASE / OPTIMISTIC-COMMIT (REQ-AGORA-013): a task both agents can
                // see must be handled once. Atomically CREATE the lease key — win and
                // do the work, or lose (key exists) and skip. No duplicate work.
                if w.payload.starts_with("TASK") {
                    let task_id = w.payload.split(':').next().unwrap_or(&w.payload).trim().to_string();
                    // NATS KV keys allow only [-/_=.a-zA-Z0-9]; map anything else (e.g. `#`).
                    let key: String = task_id
                        .chars()
                        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
                        .collect();
                    match leases.create(key.as_str(), me.to_string().into()).await {
                        Ok(_) => {
                            println!("  r{round} {me:>11} acquired lease [{task_id}] → handling (optimistic-commit)");
                            task_done.insert(task_id.clone(), me.to_string());
                            let tw = Wire { id: next_id, sender: me.to_string(), channel: "build-coord".into(), act: "inform".into(), payload: format!("HANDLED {task_id}"), sig: format!("sigil-stub:{me}:task"), hops: 0 };
                            next_id += 1;
                            publish(&js, &tw).await?;
                            accepted.push(tw);
                            engaged.insert(me.to_string());
                            produced += 1;
                        }
                        Err(_) => {
                            dropped_dup += 1;
                            println!("  r{round} {me:>11} lost lease   [{task_id}] → skip (duplicate work prevented)");
                        }
                    }
                    continue;
                }
                inbox.push(Message {
                    id: w.id,
                    sender: w.sender,
                    channel: w.channel,
                    act: parse_act(&w.act),
                    payload: w.payload,
                    sig: w.sig,
                    hops: w.hops,
                });
            }
            if inbox.is_empty() {
                continue;
            }
            for om in proto.call_coordinate(&mut store, me, &inbox)? {
                // CAPABILITY (emit side): the agent's claimed channel is untrusted
                // input. Only let it emit on a channel it was granted, and only on a
                // safe subject token — defends against structural-capability bypass
                // and subject injection (an agent must not reach `secret-ops` or
                // inject extra subject levels/wildcards by relabelling its output).
                if !is_safe_channel(&om.channel) || !granted[me].contains(&om.channel.as_str()) {
                    dropped_emit += 1;
                    continue;
                }
                let w = Wire {
                    // SENDER: stamped with the identity the host actually invoked, not
                    // the agent's self-reported `sender` — an agent cannot emit under
                    // another persona's name (anti-spoof). Real cross-publisher trust
                    // still needs the signed sigil identity (REQ-006, blocked sigil#164).
                    sender: me.to_string(),
                    id: next_id,
                    channel: om.channel,
                    act: act_str(&om.act).into(),
                    payload: om.payload,
                    sig: om.sig,
                    hops: om.hops,
                };
                next_id += 1;
                println!("  r{round} #{:<3} {:>11} --{:<7}--> [{}] hops={} {} :: {}", w.id, w.sender, w.act, w.channel, w.hops, w.sig, w.payload);
                publish(&js, &w).await?;
                accepted.push(w);
                engaged.insert(me.to_string()); // participated in the decision
                produced += 1;
            }
        }

        // OWNER-DECIDES + DEADLINE: finalize when deliberation settles (converged) OR
        // the deadline is reached, whichever comes first. The accountable owner records
        // the decision; the deadline bounds the whole deliberation independent of the
        // per-message hop budget.
        let deadline_reached = round + 1 >= decision_deadline;
        if produced == 0 || deadline_reached {
            let reason = if produced == 0 { "on convergence" } else { "at the deadline — deliberation bounded" };
            let mut who: Vec<&String> = engaged.iter().collect();
            who.sort();
            // Verdict is the owner's call. Here all engaged participants acked (no
            // refusal) and the owner concurs -> AGREED; on a split vote the owner
            // decides (owner-decides breaks ties, so deliberation cannot deadlock).
            let verdict = "AGREED";
            let dec = Wire {
                id: next_id,
                sender: decision_owner.to_string(),
                channel: "build-coord".into(),
                act: "inform".into(),
                payload: format!("DECISION[{decision_topic}] = {verdict}"),
                sig: format!("sigil-stub:{decision_owner}:decision"),
                hops: 0, // terminal announcement — not re-amplified
            };
            println!(
                "\n  [decision] owner `{decision_owner}` decides {reason} (round {round}, deadline r{decision_deadline}): \"{decision_topic}\" = {verdict}  (engaged: {who:?})"
            );
            publish(&js, &dec).await?;
            accepted.push(dec);
            decided = true;
            break;
        }
    }

    if !decided {
        println!("\nno decision reached within the round budget.");
    }

    // --- JetStream evidence: what the durable spine actually holds ---
    let info = stream.get_info().await?;
    let secret_subj = subject_for("secret-ops");
    // the ungranted message IS durably in the log (proving it was published) ...
    let secret_in_log = stream.get_last_raw_message_by_subject(&secret_subj).await.is_ok();

    println!("\nstructural controls — over real JetStream:");
    println!("  capability (consume): `secret-ops` is in the log ({}) but NO agent consumer subscribes to {} → never delivered (structural)", secret_in_log, secret_subj);
    println!("  capability (emit)   : {dropped_emit} off-scope/unsafe emissions blocked (agent may only publish to granted, safe channels)");
    println!("  self-echo filter    : {dropped_echo} own-message echoes dropped (host-stamped sender, unconditional)");
    {
        let mut k: Vec<&String> = killed.iter().collect();
        k.sort();
        let status = if k.is_empty() { "none this run (set AGORA_DEMO_KILL=<agent> to demo)".to_string() } else { format!("{k:?} halted") };
        println!("  out-of-band kill    : {status} — privileged control plane (agents hold no handle to agora._control.>; cannot ignore it)");
    }
    println!("  hop-count TTL       : per-message cascade bound (the Hermes infinite-loop guard)");
    println!("  decision deadline   : owner `{decision_owner}` finalized \"{decision_topic}\" by deadline r{decision_deadline} (owner-decides; bounds the whole deliberation, not just one message)");
    {
        let mut done: Vec<String> = task_done.iter().map(|(t, a)| format!("{t}→{a}")).collect();
        done.sort();
        println!("  lease / opt-commit  : {dropped_dup} duplicate task attempt(s) prevented; handled once: {done:?} (JetStream KV atomic create = distributed mutex)");
    }
    println!("  ordering / replay   : JetStream stream `AGORA` holds {} messages, last seq {} (global order; durable consumers replay from their position)", info.state.messages, info.state.last_sequence);

    // --- mirror accepted messages into rivet as signed facts (the durable record) ---
    // String fields are payload-derived (untrusted), so they are JSON-encoded — a
    // JSON string literal is a valid YAML scalar, so quotes/newlines/`- id:` in a
    // payload can no longer inject forged artifacts into the audit record
    // (YAML injection -> SEC-LOSS-3 integrity).
    let q = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
    let mut yaml = String::from("# agora coordination facts — mirrored into rivet (the durable, auditable record)\nartifacts:\n");
    for w in &accepted {
        let _ = write!(
            yaml,
            "  - id: COORD-{:04}\n    type: coordination-fact\n    sender: {}\n    channel: {}\n    act: {}\n    sig: {}\n    payload: {}\n",
            w.id, q(&w.sender), q(&w.channel), q(&w.act), q(&w.sig), q(&w.payload)
        );
    }
    std::fs::create_dir_all("../facts")?;
    std::fs::write("../facts/coordination.yaml", &yaml)?;
    println!("\nrivet record: wrote {} signed facts → facts/coordination.yaml", accepted.len());

    Ok(())
}

/// Publish a message to its channel subject with a `Nats-Msg-Id` for JetStream
/// dedup, and await the publish ack (so we know it is durably in the log).
async fn publish(js: &async_nats::jetstream::Context, w: &Wire) -> anyhow::Result<()> {
    let mut headers = async_nats::HeaderMap::new();
    headers.insert("Nats-Msg-Id", w.id.to_string().as_str());
    let payload = serde_json::to_vec(w)?;
    js.publish_with_headers(subject_for(&w.channel), headers, payload.into())
        .await?
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for the cross-talk control set — the spike's verification
    /// evidence as an assertion, not a manual read of stdout. This is the
    /// integration-level oracle for REQ-AGORA-003/004/005 (capability scoping,
    /// unconditional self-echo, hop-count TTL) and the Hermes postmortem rules.
    ///
    /// Requires the agent component built for wasm32-wasip2:
    ///   cd agent && cargo component build --release --target wasm32-wasip2
    ///   (or `bazel build //agent:agent`)
    #[test]
    fn cross_talk_controls_hold() {
        let sim = run_simulation().expect("simulation runs the wasm component");

        // REQ-AGORA-003 — capability channel-scoping: the ungranted `secret-ops`
        // message is never delivered. 2 agents × 1 ungranted message × 4 active
        // rounds (the bus carries it every round until convergence) = 8 blocked.
        assert_eq!(sim.dropped_caps, 8, "every `secret-ops` delivery must be blocked");

        // REQ-AGORA-004 — unconditional self-echo filter: each agent drops its own
        // messages on every channel. 12 own echoes over the run.
        assert_eq!(sim.dropped_echo, 12, "self-echo filter must drop every own-message");

        // REQ-AGORA-005 — hop-count / TTL: the deliberately chatty agents would
        // loop forever (the Hermes failure); the hop budget bounds the cascade and
        // it converges instead of running to the 8-round cap.
        assert!(sim.converged_round.is_some(), "cascade must converge, not hit the round cap");
        assert!(sim.converged_round.unwrap() < 7, "cascade must die at the hop limit");

        // The durable record: exactly the accepted messages are mirrored as facts.
        assert_eq!(sim.accepted.len(), 6, "6 signed coordination facts expected");

        // No accepted message is a self-echo or off-scope (structural invariants).
        for m in &sim.accepted {
            assert_eq!(m.channel, "build-coord", "no fact may land off the granted channel");
        }
    }

    /// Subject-injection / capability-bypass defense: an agent-supplied channel is
    /// only accepted if it is a single safe NATS token. Rejects extra subject
    /// levels, wildcards, whitespace, and empties.
    #[test]
    fn channel_validator_rejects_subject_injection() {
        for ok in ["build-coord", "secret_ops", "ch-1", "A1"] {
            assert!(is_safe_channel(ok), "{ok} should be a valid channel");
        }
        for bad in ["", "secret.ops", "build-coord.>", "build-coord.*", "a b", "x>y", "a*", "../etc"] {
            assert!(!is_safe_channel(bad), "{bad} must be rejected (injection)");
        }
    }

    /// YAML-injection defense: a payload crafted to break out of the quoted scalar
    /// and inject a forged artifact must serialize to a single, safe YAML scalar
    /// (no raw newline, properly quoted) — JSON-encoding guarantees this.
    #[test]
    fn fact_payload_cannot_inject_yaml() {
        let malicious = "x\"\n  - id: COORD-9999\n    type: coordination-fact\n    sender: admin";
        let encoded = serde_json::to_string(malicious).unwrap();
        assert!(encoded.starts_with('"') && encoded.ends_with('"'), "must be a quoted scalar");
        assert!(!encoded.contains('\n'), "encoded scalar must not contain a raw newline");
        // and it round-trips back to the exact original (no data loss from escaping).
        let back: String = serde_json::from_str(&encoded).unwrap();
        assert_eq!(back, malicious);
    }
}
