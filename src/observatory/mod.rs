// Public, reproducible aggregator over the marketplace's Nostr
// footprint. It emits one versioned JSON snapshot, which is the only
// data source for the static frontend — there is no Paygress-run
// private database.
//
// Reproducibility property: given the same `AggregatorInput` and the
// same `now`, `compute_snapshot` produces byte-identical JSON on any
// machine, on any day, in any process.

pub mod aggregator;
