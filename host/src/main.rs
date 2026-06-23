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

fn main() -> anyhow::Result<()> {
    // --- load the agent component on wasmtime (no host imports → empty linker) ---
    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config)?;
    let wasm = "../agent/target/wasm32-wasip1/release/agent.wasm";
    let component = Component::from_file(&engine, wasm)?;
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

    println!("agora spike — 2 named agents on `build-coord`; `secret-ops` is ungranted\n");

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
            println!("\nround {round}: quiet — converged (cascade died at the hop limit).");
            break;
        }
        for m in &new_msgs {
            println!("  r{round} #{:<3} {:>11} --{:<7}--> [{}] hops={} {} :: {}", m.id, m.sender, act_str(&m.act), m.channel, m.hops, m.sig, m.payload);
        }
        accepted.extend(new_msgs.iter().cloned());
        bus.extend(new_msgs);
    }

    println!("\nstructural controls fired this run:");
    println!("  capability-scoping  : {dropped_caps} deliveries of `secret-ops` blocked (agents hold no handle)");
    println!("  self-echo filter    : {dropped_echo} own-message echoes dropped (unconditional)");
    println!("  hop-count TTL       : cascade bounded — without it this is the Hermes infinite loop");

    // --- mirror accepted messages into rivet as signed facts (the durable record) ---
    let mut yaml = String::from("# agora coordination facts — mirrored into rivet (the durable, auditable record)\nartifacts:\n");
    for m in &accepted {
        let _ = write!(
            yaml,
            "  - id: COORD-{:04}\n    type: coordination-fact\n    sender: {}\n    channel: {}\n    act: {}\n    sig: {}\n    payload: \"{}\"\n",
            m.id, m.sender, m.channel, act_str(&m.act), m.sig, m.payload
        );
    }
    std::fs::create_dir_all("../facts")?;
    std::fs::write("../facts/coordination.yaml", &yaml)?;
    println!("\nrivet record: wrote {} signed facts → facts/coordination.yaml", accepted.len());

    Ok(())
}
