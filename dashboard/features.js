// Static catalogue of shipped features, hand-maintained alongside
// the PR list. Keeps the dashboard reflective of what's actually
// merged without scraping GitHub.
//
// To add a feature: append to FEATURES with unit, title, summary,
// tests, pr.
window.PAYGRESS_FEATURES = [
  {
    unit: "Unit 3",
    title: "Test infrastructure + CI baseline",
    summary: "GitHub Actions (test/fmt strict, clippy advisory). Skeleton tests; dev-deps for wiremock, proptest, tokio-test.",
    tests: "0 placeholder + 2",
    pr: 21,
  },
  {
    unit: "Unit 1",
    title: "Cashu mint redemption (Nostr-DM)",
    summary: "validate_and_redeem + MintRedeemer trait + CdkRedeemer (per-mint Wallet pool). Real NUT-03 swap; whitelist enforced before mint contact; structured RedeemError.",
    tests: "8 characterization tests",
    pr: 21,
  },
  {
    unit: "Unit 4",
    title: "Heartbeat schema fix + version + isolation_level",
    summary: "Dual-publish on stored kind 38384 (bucketed d-tag) + ephemeral 20384. Adds schema version field. Fixes calculate_uptime always returning ~0.",
    tests: "7 round-trip tests",
    pr: 21,
  },
  {
    unit: "Unit 2",
    title: "TopUp on Nostr-DM provider path",
    summary: "Real handler replaces the not_implemented stub. Mutex discipline keeps the workloads lock off the redemption network call. Wire-format regression test.",
    tests: "4 tests",
    pr: 21,
  },
  {
    unit: "Unit 7",
    title: "K8s pipeline gated behind --features kubernetes",
    summary: "default = []. Legacy K8s + ngx_l402 + HTTP path opt-in only. cargo build (no flags) now produces a Nostr-DM-only binary.",
    tests: "—",
    pr: 21,
  },
  {
    unit: "Unit 9",
    title: "paygress deploy <template> CLI",
    summary: "Opinionated wrapper over spawn with per-template defaults (nostr-relay → warm-standby, headless-browser → none). Cashu token validated by clap before any network call.",
    tests: "3 CLI tests",
    pr: 21,
  },
  {
    unit: "Unit 5 (core)",
    title: "Durable workload state machine",
    summary: "Provisioning → Live → Suspect → {Live, Evicted} → {Respawning, Failed}. M-of-N quorum (M=2, N=3, T1=120s, T2=300s). Single-writer guarantee: revocation emitted only after local state leaves Live.",
    tests: "10 transitions + 1 proptest",
    pr: 22,
  },
  {
    unit: "Unit 6",
    title: "Blossom client + XChaCha20-Poly1305 encryption",
    summary: "BUD-01/02/04 subset. Encrypt-before-hash so the SHA-256 the server addresses by is over ciphertext. Random per-encryption nonces; non-determinism property.",
    tests: "11 (4 wiremock + 7 inline crypto)",
    pr: 23,
  },
  {
    unit: "Unit 16",
    title: "Streaming top-up CLI mode",
    summary: "Year-1 chunked top-up: one TopUp DM per tick, pulling fresh tokens off a file. Generic over send fn so unit tests use a mock recorder.",
    tests: "4 tests",
    pr: 24,
  },
  {
    unit: "Unit 10",
    title: "Signed receipts + Sybil-resistant scoring",
    summary: "Co-signed CompletionReceipt bound to a verifiable Cashu spend proof. Two-layer Sybil resistance: 30-day consumer history floor + 20% same-counterparty cap. Proptest pins the per-consumer ceiling.",
    tests: "8 (incl. proptest)",
    pr: 25,
  },
  {
    unit: "Unit 11",
    title: "Stake-weighted staked tier (fidelity bond)",
    summary: "JoinMarket-style locked-Bitcoin proof. canonical_signing_message binds the proof to provider_npub (cross-npub replay defeated). SSRF-defensive Esplora URL validation.",
    tests: "12 tests",
    pr: 26,
  },
  {
    unit: "Unit 12",
    title: "Observatory aggregator (snapshot core)",
    summary: "Pure compute_snapshot: offers + heartbeats + receipts → versioned reproducible JSON. Same inputs → byte-identical bytes regardless of where it runs. Powers this dashboard.",
    tests: "8 inline",
    pr: 27,
  },
  {
    unit: "deps",
    title: "cdk 0.9 → 0.14 + nostr-sdk 0.33 → 0.43",
    summary: "Long-format keyset-ID support unlocks talking to modern Cashu mints. 13 distinct API migrations across the codebase; behavior preserved.",
    tests: "78 still passing",
    pr: 29,
  },
  {
    unit: "Unit 17",
    title: "Typed PaygressClient SDK surface",
    summary: "Canonical Rust SDK wrapping spawn / topup / status / list_offers. Schema-drift tolerant Outcome enums (Success / Error / Other). Parsers exported for reuse in other transports (MCP).",
    tests: "9 inline",
    pr: 30,
  },
];
