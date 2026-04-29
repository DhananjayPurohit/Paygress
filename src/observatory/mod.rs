// Observatory aggregator (Unit 12 of the 12-month plan).
//
// Public, reproducible scheduled aggregator that crawls the Paygress
// marketplace's Nostr footprint and emits a single versioned JSON
// snapshot. The snapshot is the only data source for the static
// frontend; there is no Paygress-controlled private database.
//
// This module exposes the **pure** snapshot computation (offers +
// heartbeats + receipts + pre-computed stake statuses → snapshot).
// The Nostr crawler, the Esplora-backed stake verifier, the binary
// entry point, and the GitHub Actions workflow that publishes the
// snapshot to gh-pages are wired in follow-ups so they can be
// reviewed independently. Keeping the core pure also means anyone
// can audit the score math by feeding it canned inputs.
//
// Reproducibility property
// ------------------------
// Given the same `AggregatorInput` and the same `now`, two
// invocations of `compute_snapshot` (on different machines, on
// different days, in different processes) produce byte-identical
// JSON. The proptest in `tests/observatory.rs` will verify this
// (output ordered deterministically by provider npub).

pub mod aggregator;
