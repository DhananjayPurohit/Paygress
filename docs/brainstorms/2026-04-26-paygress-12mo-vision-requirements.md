---
date: 2026-04-26
topic: paygress-12mo-vision
---

# Paygress 12-Month Vision: From MVP to Sovereign Compute Marketplace

## Problem Frame

Paygress today is a working MVP that lets a consumer rent a Linux container on a stranger's hardware by sending a Cashu token over Nostr. The substrate (LXD/Proxmox/K8s, Cashu, Nostr DMs, MCP, WireGuard tunnel) is in place. What it is **not yet** is something a non-technical activist or an AI agent can rely on:

- **No fault-tolerance contract.** Compute disappears at any moment with no automatic recovery. Stateful workloads (relays, mints, BTC nodes) are operationally exposed.
- **No trust layer.** Consumers cannot distinguish honest providers from malicious or honeypot providers; providers cannot prove tenant isolation. This is the unsolved problem in DePIN, not unique to Paygress.
- **No demand pull.** "Rent a container with sats" is too generic. The grant audience (HRF, OpenSats, Spiral) and the latent commercial audience (AI agents in 2026) both need turnkey *workloads*, not bare containers.
- **No agent-native surface beyond the MCP scaffold.** The MCP server exists; the language SDKs, streaming top-up, and L402-paywalled template endpoints that would make this *the* compute layer for autonomous agents are not yet shipped.
- **Proposal/code drift.** The grant pitch leans on L402; the live product runs on Cashu. Reviewers reading the README will spot this. The fix is narrative, not architectural — both belong in the stack, applied where each fits best.

The 12-month effort funded by the OpenSats / HRF / Spiral grant ($300K, two engineers, $60K hardware) should resolve all five gaps simultaneously, with explicit phasing because two engineers cannot harden every pillar at the same depth.

## Requirements

### Audience and positioning

- **R1.** Paygress is positioned for **two primary users with one shared substrate**: (a) freedom-tech operators (activists, journalists, Nostr/Cashu/Bitcoin infra hosts) and (b) AI agents and developers needing cheap, permissionless, ephemeral compute paid in Bitcoin. The product must serve both without forking the codebase. Where these audiences pull in opposite directions, default to the freedom-tech invariant (permissionless, no KYC, jurisdiction-aware) and let the agent path layer on top.
- **R2.** Public messaging adopts a **two-layer payment narrative**: Cashu ecash for compute leases (privacy, offline-friendly, no channel between consumer and provider); L402 as an **opt-in, demand-driven paywall pattern** for workload APIs, with `inference-endpoint` as the year-1 reference implementation (R17). Both are Lightning-native and reuse the team's prior `ngx_l402` work. The grant proposal's L402 framing must be edited to describe a *pattern* (one ref impl) rather than a *requirement on every template*.

### Pillar 0 — Payment correctness (P0 prerequisite, blocks all of Pillars 1–4)

- **R0a.** Wire `cdk` mint redemption into the **Nostr-DM provider path** (`src/provider.rs`). Today the provider extracts a token's face value but never spends it at the mint, so a token can be replayed across N providers undetected. Until this lands, every requirement that depends on payment correctness (R7 receipts, R8 escrow, R15 streaming) is built on sand. This is the first work item of Q1, not an implicit assumption.
- **R0b.** Port single-shot **TopUp** from the K8s/`pod_provisioning.rs` path into the Nostr-DM provider path. `src/provider.rs:378` currently warns "TopUp is not yet implemented." R15 streaming top-up is three layers — single-shot top-up, mint redemption (R0a), and the streaming protocol — and the doc must not pretend the first two are free.

### Pillar 1 — Reliability primitives (deep hardening, P1)

- **R3.** Ship a **Durable Workload** abstraction available via CLI and SDK. A workload is described by `{manifest, state_uri, replication, restart_policy}` where `replication ∈ {none, warm-standby}`. `none` is the default (cheapest, agent-friendly). `warm-standby` is **single-writer always**: at most one replica holds the write lease at any instant; failover is fast restart from the latest encrypted Blossom checkpoint on a new provider after the lease is revoked via Nostr. Concurrent N-way replication is explicitly rejected — without consensus, two replicas of a Cashu mint double-sign and destroy the unforgeability invariant; two replicas of a Lightning node double-spend HTLCs; two replicas of a Nostr relay disagree on NIP-09 deletions and NIP-40 expirations. Templates declare which replication modes they tolerate; `cashu-mint`, `bitcoin-node`, and any LN-bearing template are restricted to `replication=none` regardless of consumer request.
- **R4.** Implement the **heartbeat → lease-revocation → standby-promotion → respawn** loop as a first-class state machine (`healthy → suspect → evicted → respawning → live`) driven by Nostr Kind 38384 heartbeats signed by the provider's key. Eviction requires M-of-N relay observations (operator-tunable) so a single relay outage cannot fake death; the eviction timer fail-safes to evicted (not indefinite suspended) so suppression cannot block respawn. State is reconstructible by any third-party observer reading public Nostr events — no private Paygress backend.
- **R5.** Provide a **state-persistence pattern** for stateful templates. Default mechanism is content-addressed Blossom (BUD-04) with operator-chosen Blossom servers; templates that need it ship with checkpoint/restore hooks pre-wired. Checkpoint blobs are **encrypted client-side before upload** (key derived from the consumer's lease-specific identity) so that Blossom server operators and downloaders-by-hash cannot read tenant state. Blob hashes are communicated only over encrypted DMs, not in public Nostr events.
- **R6.** Provide a **premium-tier adapter pattern** behind the same backend trait used by LXD/Proxmox/K8s, with **LNVPS as the first reference implementation conditional on confirmed engagement.** Week-1 deliverable: a written posture from the LNVPS team on whether they will (a) run a Paygress provider agent themselves, (b) accept a Paygress wrapper of their public API, or (c) decline. If LNVPS confirms by end of Q1, the adapter ships. If LNVPS declines or stalls, R6 falls back to an in-marketplace **stake-weighted premium tier** (providers who lock larger R8 stakes appear in the `reliability=premium` cohort). Either way the consumer-facing contract is the same: `--reliability premium` returns from a curated set, never from anonymous unstaked offers.

### Pillar 2 — Trust and reputation (P1 = R7 + R10; R8/R9 are explicit cut-ladder candidates)

- **R7.** Ship **signed completion receipts** as a Nostr event kind. After a successful or failed lease, the consumer publishes a receipt (lease ID, duration delivered vs. paid, success/failure) that is **co-signed by the provider** and **bound to a verifiable payment proof** (the redeemed Cashu token signature from R0a, or a stake-escrow lock proof from R8 if escrow was used). Receipts without provider co-signature and payment proof do not contribute to the score. The score function explicitly down-weights pubkeys with short history or with receipts only against a single provider, to resist obvious Sybil patterns. A bootstrap protocol seeds the first ~100 receipts via a public commitment by the Paygress team (anchor providers running known workloads with public completion logs) so the corpus has signal before consumer-driven receipts arrive. Aggregation is reproducible from public Nostr events alone — no central registry.
- **R8 (cut-ladder candidate).** Ship **optional provider stake escrow**: providers may lock N sats in a 2-of-3 escrow with the consumer and a **consumer-chosen (or mutually agreed) arbiter relay** — the provider must not unilaterally pick the arbiter, since a provider-chosen arbiter trivially defeats the escrow. Failed-delivery proofs slash the stake; the proof format is a verifiable absence of heartbeats in the public Nostr event log over a defined window. **R8 is the first item demoted to year-2 if Q2 milestones slip** (see "Capacity and cut-ladder" below). Year-1 fallback: ship the escrow protocol *spec* as part of the NIP (R18) without a productized implementation, so other clients can build it later.
- **R9 (year-2 research deliverable).** **Demoted from production tier to research deliverable.** Year-1 deliverable for R9 is: (a) a published threat model that explicitly disclaims side-channel resistance, (b) reference attestation values for a chosen platform, (c) a working demo on rented attested instances. Productizing TEE attestation as a consumer-facing tier is a year-2 commitment, contingent on Rust attestation tooling maturity and a substrate decision (Confidential Containers under K8s is the leading candidate; LXD/Proxmox cannot pass guest measurements through and are explicitly out of scope for attestation). Until productized, providers may self-declare an **`isolation_level` tag** on their offer (`shared-kernel` / `dedicated-host` / `attested-research-tier`) which the observatory surfaces as informational, with no cryptographic claim. Templates whose security pitch depends on attestation (cashu-mint key extraction, etc.) must document that year-1 isolation is provider-trust-only.
- **R10.** Ship a **public read-only observatory** that visualizes live offers, heartbeats, completion-rate per provider, and a jurisdiction map. The observatory is a static site **plus a build-time aggregator** (a public, reproducible script — e.g., a scheduled GitHub Action — that crawls Nostr relays, computes rolling-30-day metrics, and publishes a JSON snapshot the static frontend reads). The aggregator is *operationally a backend* but is openly reproducible: anyone can run the same script and verify the published numbers; no Paygress-private database holds reputation. **Jurisdiction disclosure is opt-in per provider and coarse-grained (continent or region, never city);** providers in hostile environments can publish offers without geographic metadata. Heartbeat timing is rounded to reduce passive geolocation.

### Pillar 3 — Killer templates (curated set ships hardened; rest stay beta)

- **R11.** Ship **three flagship templates to production quality** in year 1, picked to span both audiences and demo well:
  - `nostr-relay` (replicated, Blossom-backed event store) — freedom-tech anchor.
  - `inference-endpoint` (vLLM or llama.cpp behind L402-paywalled HTTP) — agent-economy anchor; reconnects `ngx_l402` to the product narrative.
  - `headless-browser` (Playwright/Chromium pool, agent-friendly) — agent-economy anchor with broad demand.
- **R12.** Ship **four additional templates as beta-quality reference implementations** that demonstrate the platform but are explicitly labeled "beta" (no SLA): `tor-bridge`, `cashu-mint`, `bitcoin-node` (pruned + LN), `ngit-build-runner`. Beta templates are real, runnable, and tested in CI but do not promise hardening within the grant year.
- **R13.** Templates are **signed by the Paygress project keys**; consumers can opt to require a project signature so a malicious provider cannot ship a tampered image under a known template name.

### Pillar 4 — Agent-native surface (curated subset hardened)

- **R14.** Expose all spawn/topup/status operations as **MCP tools** so any MCP-compatible agent (Claude, ChatGPT, etc.) can rent compute without bespoke auth or wallet integration.
- **R15.** Ship **streaming Cashu top-up**: a single SDK call keeps a workload alive as long as the consumer streams sats; stopping payment lets the workload die at the next billing tick. **The user-visible contract is "stream sats to extend the lease"; the wire protocol is chunked top-up by default.** Year-1 ships **chunked top-up every N seconds (operator-tunable, default 60s)** via the existing TopUp message path, after R0a/R0b land. A "true" sub-second streaming Cashu protocol (new NUT extension) is a stretch goal contingent on `cdk` maintainer cooperation and is not on the critical path. Before committing to streaming-Cashu, a one-page comparison vs. NWC subscriptions, BOLT-12 keysend, and rolling-LSAT must be written and shared with the team — if a non-Cashu mechanism delivers the same agent UX with materially less spec invention, R15 should adopt it.
- **R16.** Ship language coverage in this prioritized form: (a) **Rust SDK** as canonical implementation, (b) **thin Python wrapper** that subprocesses the Rust CLI and exposes spawn/topup/status/durable-workload as Python functions, (c) **auto-generated TypeScript types** from the JSON schemas published with R18 (no hand-rolled TS runtime). Full hand-rolled Python and TypeScript implementations of the durable-workload + MCP + streaming surfaces are a year-2 item; year-1 capacity does not exist. No web app, no dashboard product — the observatory is the only UI.
- **R17.** L402 paywalls on workload APIs are an **opt-in, demand-driven pattern**, not a default. Year-1 ships **one reference implementation** — `inference-endpoint`, where per-token billing matches the per-request paywall model and reconnects `ngx_l402` to the product narrative. Other templates expose their APIs however the consumer wants (plain HTTP, API key, Cashu, or L402). The composable-resale story is preserved as a pattern other templates can adopt later without forcing every Q1–Q3 template to ship per-container `ngx_l402` plumbing.

### Cross-cutting deliverables

- **R18.** Publish the **Paygress offer schema and lease protocol as a Nostr Implementation Possibility (NIP)** by month 9, so other clients and providers can interoperate. This is the difference between Paygress-the-product and Paygress-the-protocol; the grant funds the latter posture. **All offer, heartbeat, and lease event payloads carry an explicit `version` field from day one** so the schema can evolve through NIP review without breaking in-flight providers and consumers. To make Success Criterion 5 (≥3 external integrations) realistic, recruit a co-author / first external integrator (Nostr client, agent framework, or DePIN project) by Q2 — without a pre-committed lighthouse implementation in a non-Paygress codebase, NIP adoption inside one calendar year is unlikely.

- **R19.** Publish a **security-audit report** covering tenant isolation, payment-flow correctness (R0a/R0b/R7), Nostr-DM replay/race conditions, escrow-protocol soundness if R8 ships, and Blossom-checkpoint encryption. Audit kicks off in early Q3 so remediation has a real Q4 window. Public disclosure of findings.
- **R20.** Ship a **provider one-click bootstrap on three jurisdictions** (already partly built: `paygress-cli bootstrap`) and onboard ≥10 independent providers across ≥5 jurisdictions during the year, prioritizing the Global South per the grant's geographic-diversity goal. Resourced by R22.
- **R21.** Ship an **opinionated `paygress deploy <template>` command** that hides reliability, persistence, and replication choices behind sane defaults per template. A freedom-tech operator with command-line comfort runs `paygress deploy nostr-relay --pay <cashu-token>` and gets a warm-standby relay with encrypted Blossom checkpoints — without needing to know R3/R5/R7 exist. Advanced flags remain for power users; the activist UX promise from Success Criterion 1 lives in this command.
- **R22.** **Provider community / DevRel allocation.** Carve a part-time community/DevRel role (paid from the grant or by reallocating engineering hours) tasked with onboarding the providers named in R20. Two engineers cannot also do recruitment, jurisdictional support, and translation. Without an explicit allocation, R20's targets get cut and the grant's Global South promise becomes decorative. Document the allocation alongside the $60K hardware split.
- **R23.** **Abuse-response policy** for `inference-endpoint` and `headless-browser` templates (and any future template that exposes outbound network actions). Specify: where complaints land, what authority Paygress vs. providers vs. arbiters have, how a provider opts out of template categories, what happens on a takedown demand, and how this interacts with the freedom-tech "no KYC" invariant. Resolve before R11 ships.

## Success Criteria

- A freedom-tech operator with command-line comfort can deploy a warm-standby Nostr relay in <10 minutes via a single `paygress deploy nostr-relay` command (R21), paying with a Cashu token, and have it survive the loss of any single provider with no manual recovery. (Note: `cashu-mint` deploys via the same command but is beta and runs `replication=none` — single-provider failover is not promised for the mint in year 1.)
- An AI agent can spin a `headless-browser` or `inference-endpoint` via MCP in one tool call, stream sats to keep it alive, and — for `inference-endpoint` specifically — resell its work via an L402 paywall. No KYC step on either side.
- The public observatory (R10) shows ≥10 active providers across ≥5 jurisdictions, ≥99% payment-success rate (gated on R0a), and ≥95% lease-fulfillment rate over a rolling 30-day window by month 12.
- The grant proposal narrative, the README, and the deployed product describe the same payment architecture without contradiction: **Cashu for compute leases (incl. chunked streaming top-up); L402 as an opt-in paywall pattern reference-implemented in `inference-endpoint`.**
- At least three external projects integrate the Paygress NIP or SDK during year 1, *with at least one external co-author or lighthouse implementation committed before NIP draft publication.*

## Scope Boundaries

- **Out of scope:** SaaS web app or hosted dashboard product. The observatory is a static frontend + a public reproducible aggregator (R10).
- **Out of scope:** Any token issuance, governance DAO, or revenue-sharing economics on top of sats. Pure-protocol play.
- **Out of scope:** Custom Cashu mint implementation; reuse `cdk` / nutshell.
- **Out of scope:** Mobile clients.
- **Out of scope:** Live-migration with cooperative providers (CRIU/Proxmox-migrate). Reliability is delivered by warm-standby restart from encrypted Blossom checkpoint, not by cross-provider live memory migration.
- **Out of scope:** **Concurrent N-way replication with consensus.** Replication is single-writer warm-standby always (see R3). Any user request for `mirrored:N` with N>1 active writers is rejected.
- **Out of scope:** "Fully Nostr-based control plane" in the strong sense of consensus on Nostr. Each provider's local agent remains a coordinator; the marketplace is centralization-free in the sense that no Paygress-controlled private state exists.
- **Out of scope (year 1):** Productized TEE-attested tier (R9 demoted to research deliverable). Hand-rolled Python and TypeScript SDKs (R16 collapsed to thin wrappers + codegen). DLC-based automated slashing of provider stake. Templates beyond the three flagship + four beta listed in R11/R12.
- **Out of scope (year 1):** Sub-second streaming Cashu protocol. Year-1 ships chunked top-up (R15); a true streaming-Cashu NUT extension is a stretch goal contingent on `cdk` cooperation.
- **Out of scope (year 1):** L402 paywalls on `headless-browser`, `cashu-mint`, or other API-exposing templates beyond the `inference-endpoint` reference implementation.

## Key Decisions

- **Payment correctness is the foundation, not an assumption.** R0a (mint redemption on the Nostr-DM path) and R0b (single-shot top-up on the Nostr-DM path) are P0 prerequisites blocking R7/R8/R15. Without them, every trust mechanism above is built on replayable bearer tokens.
- **Dual audience, single substrate.** Optimize for both freedom-tech and agents; default to freedom-tech invariants when they conflict. Rationale: the grant funds freedom tech; the durable demand is agents; the architecture serves both if reliability and trust are real. Conflict cases (abuse response, observability, jurisdictional pressure) are enumerated in R23 and the conflict table planning produces.
- **Warm-standby, not concurrent replication.** Single-writer always; failover is fast restart from encrypted Blossom checkpoint. Rationale: concurrent N-way replication without consensus silently breaks Cashu mint unforgeability, double-spends LN HTLCs, and corrupts NIP-09/NIP-40 semantics on Nostr relays. Doing the safe thing is more important than the wider knob.
- **L402 is a pattern, not a default.** One reference implementation in year 1 (`inference-endpoint`); other API-exposing templates choose their own auth. Rationale: per-container `ngx_l402` plumbing in the Proxmox/LXD path has no shared nginx layer and is per-template work; the agent-economy resale story is preserved without forcing every flagship to ship it.
- **Premium tier is conditional on confirmed engagement.** R6 ships LNVPS adapter only if LNVPS confirms cooperation by end of Q1; otherwise R6 falls back to in-marketplace stake-weighted premium. Rationale: a premium tier whose existence depends on an unconfirmed third-party roadmap is not a deliverable.
- **TEE is a year-2 productization, year-1 research deliverable.** R9 produces a threat model + reference values + working demo on rented attested instances; year-1 consumer-facing isolation is a self-declared `isolation_level` tag (R9). Rationale: Rust attestation tooling is immature; LXD/Proxmox cannot pass guest measurements through; "cannot read tenant memory" is a research claim, not a checkbox.
- **Curated template set.** Three flagship + four beta, replication-mode-restricted per template. Rationale: two engineers cannot ship eight production-quality templates; templates that need consensus to replicate safely don't get to claim replication.
- **Protocol over product.** Publish a NIP with at least one external lighthouse co-implementer, ship Rust SDK + thin Python/TS wrappers, no SaaS dashboard. Rationale: matches OpenSats / HRF / Spiral funding posture; collapsing R16 to wrappers preserves capacity for Pillar 1/2.
- **Reputation = co-signed receipts + payment-proof + Sybil weighting + bootstrap seed.** Optional stake (R8) layered on top, on the cut-ladder if Q2 slips. Rationale: receipts without provider co-signature and payment binding are gameable.

## Capacity and Cut-Ladder

P1 deliverables sum to roughly 1.1× the engineering capacity of two engineers in 12 months even after the cuts above. This section makes the cut-ladder explicit so silent depth-cuts at month 9 cannot happen — a published order of demotion is part of the contract with the grant funders.

**Order of cuts if Q2 milestones slip:**

1. **R8 (provider stake escrow) → year-2 spec only, no productized implementation.** R7 receipts cover the trust pitch for the grant; escrow is layered protection most useful once the marketplace has economic gravity.
2. **R9 (TEE) → already demoted from production tier; if Q2 also slips, the year-1 demo on rented attested hardware is descoped to a written threat model only.**
3. **R12 beta templates → cut from 4 to 2.** Keep `cashu-mint` and `tor-bridge` (operationally simplest of the four; activist-aligned). Drop `bitcoin-node` and `ngit-build-runner` to year 2.
4. **R15 streaming → ships only as discrete chunked top-up; no `cdk` extension work.** The "stream sats to extend lease" UX still works, just at minute granularity.
5. **One R11 flagship template → cut from 3 to 2.** Most defensible drop is `headless-browser` (broadest agent demand but largest per-template engineering scope; `nostr-relay` and `inference-endpoint` remain to anchor both audiences).

The cuts at 1–2 are the *expected* path under realistic execution risk; cuts 3–5 are the contingency path if external dependencies (LNVPS engagement, `cdk` cooperation, NIP co-author recruitment) fall through.

## Dependencies / Assumptions

- Cashu (`cdk` 0.9) remains the primary lease-payment path; no migration to L402 macaroons for leases. **Mint redemption (R0a) requires `cdk` wallet integration on the provider side — currently not present in `src/cashu.rs` or `src/provider.rs`; this is greenfield work, not a configuration change.**
- Nostr-sdk 0.33 supports the encrypted-DM and event-kind needs already used in `src/nostr.rs`.
- `ngx_l402` continues to be maintained and is the natural L402 paywall for the `inference-endpoint` reference implementation (R17).
- **LNVPS engagement is unconfirmed as of doc-write date (2026-04-26).** R6 is gated on a written posture from LNVPS by end of Q1; in-marketplace stake-weighted premium tier is the documented fallback if engagement does not land.
- **TEE production tier is out of year-1 scope** (R9 demoted). The research-tier deliverable assumes rented attested instances are available from at least one cloud provider in 2026.
- Blossom client-side encryption (R5) requires choosing an encryption scheme (likely XChaCha20-Poly1305 with key derived from the consumer's Nostr key); no Blossom client crate currently exists in `Cargo.toml` — greenfield work.
- **Existing custom event Kinds 38383 / 38384 are in production**; R18's `version` field is added during the migration to the NIP-track schema, with a one-revision compatibility window.

## Outstanding Questions

### Resolve Before Planning
- [Affects R3, R11, R17][User decision] Which control plane (K8s/`pod_provisioning.rs` + ngx_l402, or Proxmox-LXD/`provider.rs` + Nostr-DM) is the canonical "Durable Workload" surface? Flagship templates and L402-paywalled `inference-endpoint` need a single answer before Q1 implementation begins; running two control planes for the lifecycle of the grant is the silent capacity killer.
- [Affects R6][External] LNVPS engagement: written posture from LNVPS team needed in week 1. If posture is "decline" or no response by end of Q1, R6 falls back to in-marketplace stake-weighted premium tier per the cut-ladder. Decision must be visible.
- [Affects R7, R8][User decision] Sybil weighting parameters in the receipt score function (history minimum, single-counterparty penalty, anchor-provider seed list). The protocol design needs concrete numbers before Pillar 2 implementation begins.

### Deferred to Planning
- [Affects R3, R5][Technical] Encrypted-checkpoint schema for Blossom: encryption primitive, key derivation from consumer's Nostr key, blob lifecycle (TTL, garbage collection).
- [Affects R8][Needs research] Specific arbiter-relay model: consumer-chosen single arbiter vs. threshold of relays vs. future DLC oracle. The stake-escrow protocol must not preclude later DLC migration.
- [Affects R9][Needs research] If TEE research deliverable proceeds to year-2 productization, which substrate? Confidential Containers under K8s is the leading candidate; LXD/Proxmox are explicitly out.
- [Affects R11][Technical] For `inference-endpoint`: model server choice (vLLM/llama.cpp/SGLang) given heterogeneous provider hardware. Affects which providers can host the template.
- [Affects R15][Technical] Streaming-vs-discrete top-up comparison doc (NWC, BOLT-12 keysend, rolling-LSAT, chunked-Cashu, streaming-nut) with criteria = agent UX, privacy, engineering cost.
- [Affects R18][Needs research] NIP overlap with NIP-38383 marketplace and NIP-89; minimum-viable PR to NIPs repo. **Plus: identify and recruit a co-author / lighthouse implementer before draft publication** — without this, Success Criterion 5 is unlikely.
- [Affects R20, R22][User decision] Provider DevRel allocation: paid role from grant, reallocated engineering hours, or external partner. Ten providers across five jurisdictions does not happen as a side-effect of code shipping.
- [Affects R20][Technical] $60K hardware budget split: bare-metal nodes (lighthouse providers + test fleet), networking/storage gear, rented attested instances for R9 demo. Public spec required before procurement.
- [Affects R23][User decision] Abuse-response policy concrete text: complaint intake channel, provider opt-out granularity, takedown response, interaction with no-KYC invariant. Resolve before R11 ships.

## Next Steps

Three blocking items in **Resolve Before Planning** above (control plane choice, LNVPS engagement, Sybil weighting). Once those land, `-> /ce:plan` for structured implementation planning. Suggested phasing baseline (subject to refinement during planning):

- **Q1.** R0a + R0b (payment correctness — gates everything else); R3/R4 durable workload + warm-standby state machine; R5 encrypted Blossom checkpoints; R6 decision (LNVPS or in-marketplace fallback); `nostr-relay` flagship; `paygress deploy` opinionated CLI (R21).
- **Q2.** R7 receipts (with provider co-signature + payment proof + Sybil weighting + anchor-provider seed); R10 observatory (static frontend + reproducible aggregator); `inference-endpoint` flagship with the L402 reference paywall (R17); NIP co-author recruitment.
- **Q3.** R14 MCP tools over the canonical control plane; R15 chunked top-up; R16 Rust SDK + thin Python/TS wrappers; `headless-browser` flagship (or skip per cut-ladder if behind); R18 NIP draft published with co-author; R19 audit kicks off in early Q3 so remediation has a real Q4 window (not late-Q4).
- **Q4.** R19 audit remediation; R12 beta templates (down to 2 if cut-ladder triggered); R22 provider DevRel push to hit R20 targets; R23 abuse-response policy published; R8 spec-only delivery if cut-ladder triggered.
