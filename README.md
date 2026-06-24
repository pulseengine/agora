# agora

Real-time agent-coordination substrate for PulseEngine — named agents subscribe to
channels and coordinate/agree, with every message a typed, signed, traceable fact.
**Augments** (does not replace) the GitHub-issue coordination loop.

> Status: **spike**. This proves the architecture + the cross-talk controls run on
> the lighter (NATS-core + wasmtime + WAC) path, with NATS and sigil stubbed at
> their seams. It is the input to the lighter-vs-wasmCloud decision.

## Architecture (five layers)

| Layer | What | Built with |
|---|---|---|
| Named agents | each agent = a capability-isolated wasm component | wasm component, A2A-style identity |
| Coordination logic | type / sign / shape / decide | wasm components, WAC-composed |
| Protocol contract | channel · message · speech-acts | a WIT world (`agora:agent`) |
| Durable spine *(native TCB)* | named channels, durable fan-out, replay | **NATS/JetStream** (self-hosted) |
| Record | every message → signed fact | **rivet** + **sigil** (Rekor-style) |
| Human window | watch live + inject + **out-of-band kill** | **thrum** |

The durable transport is deliberately **native** (NATS), not wasm: `wasi:messaging`
disclaims persistence/ack/delivery, so reimplementing it in wasm is gold-plating.
"WCM to the extreme" is spent where it pays off — the **logic**, **capability
isolation**, and **verification** (witness/scry/sigil) — not the transport.

## What the spike demonstrates (runnable)

`agent/` is a real wasm **component** (pure coordination logic, no ambient
authority). `host/` is the transport + capability layer (where NATS and sigil swap
in) that runs it on wasmtime and enforces the cross-talk controls the research
validated against the **Hermes infinite-ack-loop postmortem**
(`NousResearch/hermes-agent#32791`):

- **capability channel-scoping** (structural) — agents hold no handle to
  `secret-ops`, so it is *never delivered* to them. 8 deliveries blocked.
- **unconditional self-echo filter** (Hermes rule #1) — `sender == me` dropped on
  every channel, never per-channel overridable. 12 echoes dropped.
- **hop-count / TTL** — the deliberately chatty agent *would* loop forever (the
  Hermes failure); the hop budget bounds it (3→2→1→0) and it converges.
- **idempotency** — each (agent, message-id) processed once.
- **signed identity + speech acts + rivet record** — every message carries a
  (stubbed) sigil signature, a FIPA-style `act`, and is mirrored to
  `facts/coordination.yaml` as a typed rivet fact.

```sh
# Canonical: hermetic component build via the PulseEngine ruleset (Bazel) —
# native wasm32-wasip2 through wasi-sdk 29 (= WASI 0.2.6).
bazel build //agent:agent

# Quick path (same native-p2 component, no preview1 adapter):
cd agent && cargo component build --release --target wasm32-wasip2

# Run the host (loads the p2 component, enforces the cross-talk controls):
cd host && cargo run --release   # `cargo test` asserts the controls (8/12/converge/6)
```

## Stubbed seams (the swap-in points)

- **NATS/JetStream** — the host's in-memory `bus` Vec stands in for the durable
  log. Real JetStream gives the global sequence (ordering), durable consumers
  (= the watermark/pending_gates replay), and `Nats-Msg-Id` dedup.
- **sigil** — `sig` is an FNV stub; real `wsc sign --keyless` swaps in (blocked on
  `pulseengine/sigil#164`, the wasip2 parser).
- **rivet** — facts are written as YAML; real `rivet` (0.17 present) ingests them.
- **out-of-band human kill** — Hermes rule #2: thrum must hold a privileged kill at
  the gateway, not an in-channel "stop". Not in this spike.

## Lighter-vs-wasmCloud — what the spike surfaced

The lighter path **works and is fully functional**. Friction encountered (the real
decision input):

- `cargo component` still defaults its *core* target to the legacy `wasm32-wasip1`
  (preview1 + adapter), and honors neither `.cargo/config.toml` `build.target` nor a
  metadata key — so the build pins `--target wasm32-wasip2` explicitly. That yields a
  native component-model component (imports `wasi:io`/`wasi:cli@0.2.x`, no preview1
  adapter); the host's `wasmtime_wasi::p2` linker satisfies it.
- `from` is a reserved WIT keyword (→ `sender`).
- `std` pulls WASI imports, so the host needs a `wasmtime-wasi` linker + the
  version-specific `WasiView`/`WasiCtxView` boilerplate (had to read the crate
  source to get the 41.x API right). **This host-embedding plumbing is exactly what
  wasmCloud would absorb** — at the cost of running wasmCloud as a system and its
  transport providers being native anyway.

Read: for a small team building one substrate, the lighter path is viable; the
friction is one-time host plumbing. wasmCloud is the graduation path if the lattice
features (wadm, multi-host, provider ecosystem) start paying for themselves.

## WASI: on p2 now, p3 is the direction

**WASI 0.3.0 (Preview 3) was ratified 2026-06-11** — it rebases WASI onto the
Component Model's *native async* primitives (`async func`, `stream<T>`, `future<T>`).
This spike deliberately builds on stable **wasm32-wasip2** today, not preview1 and not
p3, because:

- The agent is **pure coordination logic** — its only WASI surface is what `std`
  pulls in; it gains nothing concrete from p3's async streams.
- The Rust **`wasm32-wasip3` target is still tier-3** ("does not yet build" without a
  `libc` `[patch]`; needs nightly + `-Z build-std` + `wasi-sdk ≥22`), and its `std`
  **still emits p2 imports** during the transition — so a p3 build today would add
  major toolchain friction (contradicting the lighter-path thesis above) for p2
  imports anyway.
- p3 host support lands in **wasmtime 43+**; this host is on 41.

**Where p3 actually pays off for agora — and the adoption path when we take it:**

1. **The transport seam (the real win).** p3's `stream<T>`/`future<T>` map cleanly
   onto JetStream consumers — backpressure, ordering, and async delivery become
   first-class in the WIT contract instead of host plumbing. This is the layer that's
   stubbed today (the in-mem `bus`), so it's the right place to adopt p3.
2. **Host:** bump `wasmtime`/`wasmtime-wasi` 41 → 43+ and switch
   `wasmtime_wasi::p2::add_to_linker_sync` → `p3::add_to_linker_async` (async linker,
   async `call_coordinate`).
3. **Agent:** move to `wasm32-wasip3` once it reaches tier-2 and `std` migrates off p2
   imports — then the gap closes with no `build-std`/wasi-sdk friction.

See: [wasi.dev/roadmap](https://wasi.dev/roadmap),
[rustc — wasm32-wasip3](https://doc.rust-lang.org/beta/rustc/platform-support/wasm32-wasip3.html),
[Async Components on wasmCloud with WASI P3](https://wasmcloud.com/blog/wasi-p3-on-wasmcloud/).
