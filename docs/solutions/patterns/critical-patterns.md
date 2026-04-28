# Critical patterns

Footguns and invariants that have already cost us — or will, if
ignored. Each entry follows the schema in
`docs/solutions/README.md`.

---

## Cashu

### Decoding a token's face value is NOT redemption

**Symptom**: A consumer sends a Cashu token; the provider accepts
the payment, provisions a container, and the same token is replayed
to N other providers — all of which also accept it. No double-spend
detection fires. The mint never sees the proofs.

**Root cause**: `src/provider.rs:455` (and historically
`src/cashu.rs::extract_token_value`) only *parses* the serialized
token to read the face value out of its proofs. Parsing is purely
local; it does not contact the mint, does not swap proofs (NUT-03),
and does not consume them. A valid-looking token therefore replays
indefinitely.

**Fix / rule**: Before treating a token as paid, perform a
swap-on-receive against the mint using `cdk::wallet::Wallet::receive`
(NUT-03). The swap atomically consumes the input proofs and returns
fresh proofs owned by the provider's wallet. Reject the request on
`Error::TokenAlreadySpent`, `Error::TokenPending`, mint 5xx, or any
mint outside the configured whitelist. Only after a successful swap
may `create_container` (or any backend call) run.

**Where it bites**:
- `src/provider.rs` Nostr-DM handler (currently the canonical path).
- `src/cashu.rs` token utilities.
- Any future provider interface that accepts a Cashu token.

**Reference**: Unit 1 of
`docs/plans/2026-04-26-001-feat-paygress-12mo-vision-plan.md`. Test
coverage placeholder lives at `tests/cashu_redemption.rs`. Routstr is
the production precedent for the `Wallet::receive` integration.

---

## Nostr

### Kind 38384 without a `d` tag is silently overwritten

**Symptom**: Heartbeat events publish successfully (relays accept
them, no error returned), but the eviction loop sees only the most
recent heartbeat per provider key — past heartbeats appear to vanish
from M-of-N quorum calculations, and the state machine bounces
between `Live` and `Suspect` for no apparent reason.

**Root cause**: Kind 38384 is a **parameterized replaceable** event
(NIP-33-style). Relays index it by `(pubkey, kind, d-tag)` and
overwrite older events sharing that triple. If the heartbeat
publisher omits the `d` tag (or always uses the same one), every new
heartbeat replaces the previous one server-side. M-of-N quorum logic
that expects to *count* recent heartbeats across a window will count
at most one.

**Fix / rule**: Heartbeats must be dual-published:
1. **Addressable record** on Kind 38384 with a bucketed `d` tag
   (e.g. `d = "<workload-id>:<bucket-timestamp>"`) so distinct
   buckets coexist on the relay and recent history is queryable.
2. **Ephemeral signal** on Kind 20384 for low-latency observers that
   do not need replay history.

The eviction loop reads from both: addressable for windowed quorum,
ephemeral for fast-path liveness.

**Where it bites**:
- `src/nostr.rs` (publisher).
- `src/provider.rs` heartbeat / eviction loop (Unit 5 will wire
  `src/durable_workload.rs`).
- Any reader that filters by Kind 38384.

**Reference**: Unit 5 of
`docs/plans/2026-04-26-001-feat-paygress-12mo-vision-plan.md` (warm-
standby state machine and M-of-N quorum). NIP-01 / NIP-33 for the
parameterized-replaceable semantics. Test coverage placeholder lives
at `tests/durable_workload.rs`.

---

## CI / build

### Clippy runs in advisory mode — do not assume warnings block PRs

**Symptom**: A contributor adds dead code or unused imports, opens a
PR, sees green CI, and merges. Warnings accumulate and a later PR
that flips clippy to `-D warnings` suddenly "breaks the build" with
~32 unrelated findings.

**Root cause**: `.github/workflows/ci.yml` runs
`cargo clippy --all-targets --all-features` *without* `-D warnings`
on purpose. The 12-month plan staggers tightening:
1. Unit 7 feature-gates the K8s pipeline behind `kubernetes`,
   removing ~half of today's dead-code warnings from default builds.
2. A follow-up PR cleans remaining warnings on the Nostr-DM
   canonical control plane.
3. Only then does the clippy job flip to `-D warnings` and shed the
   "(advisory)" label.

**Fix / rule**: Treat clippy warnings as build-blocking *socially*
even though the job does not fail on them. New code must not
introduce warnings, even while the job is in advisory mode. Reviewers
should request changes for clippy findings the same way they would
for fmt failures.

**Where it bites**:
- `.github/workflows/ci.yml` (the job's comment block restates
  this).
- New contributions that copy patterns from existing legacy modules
  may inherit warnings; check `cargo clippy` locally before pushing.

**Reference**: Unit 3 (this baseline) and Unit 7 (feature-gating) of
`docs/plans/2026-04-26-001-feat-paygress-12mo-vision-plan.md`.
