// Event kinds and protocol constants.

/// NIP-33 parameterized-replaceable, `d = paygress:offer:v1:<npub>`.
pub const KIND_PROVIDER_OFFER: u16 = 38383;
/// NIP-33 addressable heartbeat. The `d` tag is bucketed per minute
/// (`paygress:heartbeat:v1:<npub>:<bucket>`) so heartbeats accumulate as
/// queryable history instead of each one replacing the last.
pub const KIND_PROVIDER_HEARTBEAT: u16 = 38384;
/// NIP-16 ephemeral heartbeat. Relays do not store these, so they serve live
/// presence only; both kinds are published on every beat.
pub const KIND_PROVIDER_HEARTBEAT_EPHEMERAL: u16 = 20384;
/// Lease revocation, published by a primary whose `WarmStandby` workload has
/// left `Live`. Addressable (`d = paygress:revocation:v1:<primary>:<workload>`)
/// so a standby that comes online later still sees it; each standby is added as
/// a `#p` tag for filterable subscriptions.
pub const KIND_LEASE_REVOCATION: u16 = 38385;
/// Published by a standby right after it promotes itself to primary. Peers
/// check for one of these before claiming their own slot, so only the winner of
/// the promotion race spawns a container. Addressable on
/// `d = paygress:promoted:v1:<workload_id>`.
pub const KIND_STANDBY_PROMOTION_ANNOUNCEMENT: u16 = 38386;

/// Schema version for offer + heartbeat payloads. Old payloads without this
/// field deserialize to `1` via `#[serde(default)]`.
pub const SCHEMA_VERSION: u8 = 1;

/// Window for "is this provider alive right now?" lookups.
pub const LIVE_HEARTBEAT_WINDOW_SECS: u64 = 300;

/// Heartbeat `d`-tag bucket size, matching the 60s heartbeat cadence so every
/// beat lands in its own `(npub, kind, d-tag)` slot.
pub const HEARTBEAT_BUCKET_SECS: u64 = 60;
