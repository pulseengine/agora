# agora — handoff

Cold-start doc for whoever picks this up. agora is a **real-time agent-coordination
substrate** for PulseEngine: named agents subscribe to channels and coordinate/agree,
with every message a typed, signed, traceable fact. It **augments** (does not
replace) the GitHub-issue coordination loop.

**Status:** design complete + runnable spike. rivet artifacts green
(`rivet validate` PASS, 100% coverage). Not yet a production service.

---

## 1. The decided architecture

| Layer | What | Tech | Decision |
|---|---|---|---|
| Named agents | capability-isolated wasm components | wasm component + A2A-style identity | AD-006 |
| Coordination logic | type / sign / shape / decide | wasm components, WAC-composed | AD-004, AD-006 |
| Protocol | channel · message · speech-acts | WIT world `agora:agent` | — |
| Durable spine *(native TCB)* | named channels, ordering, replay | **NATS/JetStream** (self-hosted) | AD-003 |
| Record | every message → signed fact | **rivet** + **sigil** (Rekor-style) | AD-001, AD-005 |
| Human window | watch + inject + **out-of-band kill** | **thrum** | AD-007 |

Requirements `REQ-AGORA-001..010`, decisions `AD-AGORA-001..007` live in
`artifacts/` (rivet). Run `rivet validate` / `rivet coverage` to see the graph.

## 2. How we got here — three deep-research runs (2026-06)

1. **Coordination architecture** — MCP is the *local* agent↔tool seam, **not** the
   agent↔agent bus (confirmed); A2A Agent Cards = named discovery; naive
   fact-store-as-bus is an anti-pattern ("Postgres-as-queue considered harmful",
   blackboard-as-broadcast refuted); NATS JetStream = the durable spine; Rekor =
   signed append-only model.
2. **WCM substrate** — `wasi:messaging` *explicitly disclaims* persistence/ack/
   delivery; wRPC is RPC not pub/sub; **wasmCloud** is the closest precedent (NATS
   lattice + capability providers + wRPC) but its providers are native, not wasm.
   **Verdict:** build the durable spine on NATS; spend "WCM to the extreme" on the
   *logic + capabilities + verification*, not the transport.
3. **Cross-talk** (the hard part of going real-time) — see §3.

## 3. The cross-talk control set (the load-bearing safety analysis)

Going real-time strips the async issue model's accidental protections; each must be
re-added. **Structural** (by construction) vs **runtime** (must design in):

- **Structural:** WASI capability channel-scoping (an agent can't hear/emit on
  ungranted channels — reachability-only, NOT total security); sigil-signed
  per-persona identity; WIT types + speech acts; JetStream per-stream ordering.
- **Runtime:** unconditional self-echo filter; hop-count/TTL + rate budget;
  leases/optimistic-commit (duplicate work); decision deadline + owner-decides.

**The Hermes postmortem (`NousResearch/hermes-agent#32791`) — three hard rules:**
1. the self-echo filter must be **unconditional**, never per-channel overridable;
2. the human STOP must be **out-of-band** (privileged kill at the gateway/thrum),
   not an in-channel message the agent can ignore;
3. filter on **signed persona identity**, not sender-TYPE.

## 4. The spike (`agent/` + `host/`) — what runs, what's stubbed

`cd agent && cargo component build --release && cd ../host && cargo run --release`

Proven on a real wasm component: capability-scoping (8 `secret-ops` blocked),
unconditional self-echo (12 dropped), hop-count TTL (cascade bounded, converged),
idempotency, 6 signed facts → `facts/coordination.yaml`. See `README.md` for the
run output and the lighter-vs-wasmCloud friction notes.

**Stubbed seams:** NATS (in-mem bus Vec) · sigil (`sig` is an FNV stub; real
`wsc sign` blocked on `pulseengine/sigil#164`) · rivet (facts as YAML) · thrum
out-of-band kill (not built).

## 5. Open questions (research could not close — agora must decide)

1. Real-time-vs-async **token-cost delta** — benchmark before committing real-time defaults.
2. Exact **self-echo filter key** (signing id vs persona id vs handle).
3. **Cross-stream** causal ordering (single super-stream vs Lamport/vector vs rivet-graph) — general causal-consistency verification is undecidable, so keep causally-related coordination on one stream.
4. Default **decision deadline + amplification budget**.

## 6. Next steps → see the repo issues

Swap in real NATS/JetStream; wire real sigil signing (after sigil#164); build the
thrum out-of-band kill; per-channel capability granularity (WASI grants at socket
level — the per-channel policy layer is ours to build); the lighter-vs-wasmCloud
final call (spike says: stay lighter); apply the five-track release standard
(witness/scry/sigil/rivet) so agora is the flagship dogfood.

## 7. Where things live

- `artifacts/` — rivet requirements + decisions (the spec, validated).
- `agent/`, `host/` — the runnable spike. `facts/coordination.yaml` — sample record.
- `README.md` — spike detail + lighter-vs-wasmCloud. `HANDOFF.md` — this file.
- Full research transcripts (this session): runs `wf_f464d2a7-2ee` (coordination),
  `wf_1a6f6a4c-f6f` (WCM substrate), `wf_64dfec52-63c` (cross-talk).
