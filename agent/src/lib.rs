// Bindings come from two build paths (see Cargo.toml `cargo-bindgen` feature):
//   - cargo-component: in-tree generated `src/bindings.rs` (feature ON, default)
//   - Bazel/rules_wasm_component: the generated `agent_bindings` crate (feature OFF)
#[cfg(feature = "cargo-bindgen")]
#[allow(warnings)]
mod bindings;
#[cfg(not(feature = "cargo-bindgen"))]
use agent_bindings as bindings;

use bindings::exports::agora::agent::protocol::{Act, Guest, Message};

struct Component;

impl Guest for Component {
    /// The host has already capability-scoped and self-echo-filtered the inbox.
    /// The agent's own discipline: never amplify a dead (hops==0) message, and
    /// reply with a bounded, signed, decremented echo. This is a deliberately
    /// "chatty" agent so the spike can prove the hop-count guard actually kills
    /// the cascade that an undisciplined agent would otherwise run forever.
    fn coordinate(me: String, inbox: Vec<Message>) -> Vec<Message> {
        let mut out = Vec::new();
        for m in inbox {
            if m.hops == 0 {
                // TTL exhausted: refuse to amplify. This is the line that turns
                // the Hermes infinite-ack-loop into a bounded exchange.
                continue;
            }
            let payload = format!("ack[{me}] <- {}", m.payload);
            let sig = format!("sigil-stub:{me}:{:016x}", fnv1a(&payload));
            out.push(Message {
                id: 0, // host stamps the real monotonic id (JetStream sequence)
                sender: me.clone(),
                channel: m.channel.clone(),
                act: Act::Inform,
                payload,
                sig,
                hops: m.hops - 1, // decrement the hop budget
            });
        }
        out
    }
}

/// Placeholder for the sigil signature (real `wsc sign --keyless` swaps in here).
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

bindings::export!(Component with_types_in bindings);
