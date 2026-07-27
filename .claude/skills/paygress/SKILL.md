---
name: paygress
description: Conventions, architecture and direction for the paygress repo. Load when writing or reviewing code here, planning work, or making positioning/roadmap decisions.
---

# Working on paygress

Pay-per-second compute. Providers run a daemon on a Linux box and advertise over
Nostr; consumers pay in Cashu ecash and get a container or VM. No accounts, no
signups, no chain, no token.

Most of this codebase was written by AI with minimal review. Assume any code you
did not just write may carry that history, and check rather than trust.

## Code style

**Comments: delete unless very, very useful.** The bar is "a competent Rust
reader would otherwise reintroduce a bug or misread intent". Everything else
goes — restatements of the next line, structure narration ("Step 3: now we…"),
essays on alternatives not taken, references to internal plan documents ("Unit 5
of the 12-month plan"), TODO archaeology.

Keep: wire-format and protocol constraints, `#[serde]` compatibility notes,
safety invariants, and anything recording a bug that was actually hit. Examples
worth preserving verbatim:

- `lxc stop` exits non-zero on an already-stopped instance
- `get_container_status` defaults to `Running` so an unreachable backend never
  triggers a destructive action
- the HTTP path must not contact the mint — the instrument is already redeemed
- standby slots holding a consumer volume key are never persisted
- `luksErase` is what makes the ciphertext unrecoverable
- docker's image argument must be last or it becomes CMD

**Clap `///` docs are `--help` output, not comments.** Keep them to one short
user-facing line. Never delete them to reduce "comment density" — `tests/`
asserts on that text. Same for `mcp.rs`, whose `///` field docs ship as the MCP
tool schema that agents read.

**Files over ~700 lines want splitting** into a module directory. The compiler
verifies the split when the public surface is small; take that as licence.

**No AI slop.** Needless `.clone()`, `.to_string()` inside `format!` args,
`unwrap()`/`expect()` on fallible runtime paths, stringly-typed values where an
enum fits, deep nesting an early return would flatten, functions doing several
things, duplicated logic across commands or backends.

**Byte-index slicing on wire data is a remote panic.** `&s[..n]` panics
mid-codepoint, and with `panic = "abort"` that kills the provider. Anything
touching a Nostr event, an HTTP header, a provider name or a mint URL slices by
chars. This class has bitten repeatedly — treat any new `&s[..n]` as a bug.

**Commits:** no Claude co-author trailer. Explain why, not what.

## Architecture rules

**Payment verification does not belong in paygress.** Pricing (amount → lease
duration) is application policy and stays. Verification and settlement belong to
the paywall. On the HTTP path, `X-Payment-Amount-Msat` carries what ngx_l402
settled, so the application never sees an instrument — and therefore works with
any method ngx_l402 supports. Cashu is one option, not the only one.

That header is forgeable, so it is honoured only on a loopback bind. Keep that
boundary structural, never merely documented.

**`extract_token_value` must never contact the mint.** ngx_l402 has already
redeemed; a second call double-spends. `validate_and_redeem` checks the mint
whitelist *before* any network call, so a token pointed at an attacker's mint
causes no outbound request. Do not "optimise" either.

**Consumer key material never reaches provider disk.** `volume_encryption_key`
is `#[serde(skip)]`. Persisting it, or reloading it as `None` and silently
promoting to an unencrypted volume, are both worse than dropping the record.

**Provider state must survive restart.** `active_workloads` and standby slots
are the only record that a lease exists — the backend knows a container runs, not
who paid or when it expires. Mirror to disk on every change, temp-file+rename,
and reconcile against the backend on startup. Losing this strands containers and
burns their vmids permanently.

**Heartbeat quorum is a provider-level signal**, not per-workload liveness. Only
warm-standby acts on losing it; anything else stays Live. Evicting non-replicated
tenants on a relay hiccup tears down boxes people paid for.

## Verification

Every change: `cargo build --all-targets` (expect zero warnings),
`cargo test --all-features`, `cargo clippy --all-targets --all-features`,
`cargo fmt --all`. The only acceptable clippy warning is the clap
`large_enum_variant` on `ProviderAction`, which cannot be boxed.

`cargo check --locked --all-targets` matters separately: `default` features are
what `cargo install paygress-cli` resolves, and `--all-features` will not catch a
break there.

**A green build does not prove a new module is compiled.** An undeclared file is
silently ignored — this has already cost 875 lines of orphaned work. Check the
file appears in `target/debug/paygress-cli.d`.

For anything touching spawn, payment or the backends, run a live check against a
real provider rather than trusting tests:

```
paygress-cli wallet mint --mint <mint> --amount 50
paygress-cli spawn --provider <name> --tier ci --token "$..."
```

## Direction

Positioning is unsettled; do not rewrite it unilaterally. What the evidence
supports:

**Bitcoin-native, not web3.** The DePIN compute category is contracting on real
usage and every player is token-subsidised. OpenSats and HRF do not fund tokens;
web3's adoption mechanism *is* a token. Those are a fork, and paygress is
applying to the former. Avoid "DePIN" and "decentralized compute marketplace"
framing — it invites comparison to a failing category.

**The payment rail is the defensible part.** Metering compute by the second is
commodity — Modal, E2B, Fly all do it. Collecting sub-cent bearer payments from
strangers with no processor and no chargeback window is not.

**Success is many small single-provider deployments, not one large market.** A
spot market needs liquidity on both sides or it is useless to everyone. A
hackerspace running one box for thirty members needs none. This dissolves the
bootstrapping problem rather than solving it, and it is why `bootstrap` is the
most important command in the repo.

**No-KYC is not a differentiator** — Vast.ai, Akash's CLI, Flux and Nosana all
allow anonymous rental, and Vast is cheaper. Do not lead with it.

Open questions the maintainer has not settled: whether the spawn transaction
moves from Nostr DMs to HTTP 402, and whether the product narrows to Nostr
infrastructure hosting (relay, Blossom, NIP-05). Both are live; do not assume
either.

## Honest reporting

Verify claims before repeating them, especially from research or subagents.
Several confident findings this project has received turned out false on a direct
check. Say plainly what was verified, what was assumed, and what was left
undone — and when a premise is wrong, say so rather than working around it.
