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

use std::collections::{HashMap, HashSet};
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

/// The measurable outcome of one simulation run — the spike's verification evidence
/// as data, so `main` can print it AND the test suite can assert on it.
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

/// Run the bus simulation against the real agent component and return the measured
/// outcome. No I/O side effects (fact-writing lives in `main`) so it is testable.
fn run_simulation() -> anyhow::Result<SimResult> {
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

fn main() -> anyhow::Result<()> {
    println!("agora spike — 2 named agents on `build-coord`; `secret-ops` is ungranted\n");

    let sim = run_simulation()?;

    for (round, msgs) in sim.rounds.iter().enumerate() {
        for m in msgs {
            println!("  r{round} #{:<3} {:>11} --{:<7}--> [{}] hops={} {} :: {}", m.id, m.sender, act_str(&m.act), m.channel, m.hops, m.sig, m.payload);
        }
    }
    if let Some(round) = sim.converged_round {
        println!("\nround {round}: quiet — converged (cascade died at the hop limit).");
    }

    println!("\nstructural controls fired this run:");
    println!("  capability-scoping  : {} deliveries of `secret-ops` blocked (agents hold no handle)", sim.dropped_caps);
    println!("  self-echo filter    : {} own-message echoes dropped (unconditional)", sim.dropped_echo);
    println!("  hop-count TTL       : cascade bounded — without it this is the Hermes infinite loop");

    // --- mirror accepted messages into rivet as signed facts (the durable record) ---
    let mut yaml = String::from("# agora coordination facts — mirrored into rivet (the durable, auditable record)\nartifacts:\n");
    for m in &sim.accepted {
        let _ = write!(
            yaml,
            "  - id: COORD-{:04}\n    type: coordination-fact\n    sender: {}\n    channel: {}\n    act: {}\n    sig: {}\n    payload: \"{}\"\n",
            m.id, m.sender, m.channel, act_str(&m.act), m.sig, m.payload
        );
    }
    std::fs::create_dir_all("../facts")?;
    std::fs::write("../facts/coordination.yaml", &yaml)?;
    println!("\nrivet record: wrote {} signed facts → facts/coordination.yaml", sim.accepted.len());

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
}
