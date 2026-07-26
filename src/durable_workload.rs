// Provider-side workload lifecycle: an explicit state machine driven
// by heartbeats observed across M-of-N Nostr relays.
//
// Single-writer invariant: a workload emits `PublishLeaseRevocation`
// only after its own local state has left `Live`, and a standby
// cannot promote until it observes that revocation. So at most one
// provider is `Live` for a workload at any moment.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Replication / availability mode chosen by the consumer at spawn.
///
/// `None` is cheapest: one container, no checkpoint, no failover.
/// `WarmStandby` registers standby providers; on eviction the state
/// machine emits `PublishLeaseRevocation` so the lease can be handed
/// off. `Checkpointed` respawns from the latest Blossom checkpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case")]
pub enum ReplicationMode {
    #[default]
    None,
    Checkpointed,
    WarmStandby {
        standby_providers: Vec<String>,
    },
}

/// What to do when a workload exits unexpectedly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "kebab-case")]
pub enum RestartPolicy {
    Never,
    OnFailure { max_attempts: u8 },
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self::OnFailure { max_attempts: 3 }
    }
}

/// Lifecycle state for a workload tracked by a single provider:
/// `Provisioning -> Live -> Suspect -> {Live, Evicted}`, then
/// `Evicted -> {Respawning, Failed}` per replication + restart
/// policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkloadState {
    Provisioning {
        since: u64,
    },
    Live {
        since: u64,
    },
    /// Observed silent on too many relays; debounce window before
    /// eviction.
    Suspect {
        since: u64,
    },
    /// Heartbeats absent past T2; lease is forfeit on this provider.
    Evicted {
        at: u64,
    },
    /// Restart in progress (only when `RestartPolicy::OnFailure`
    /// and replication is `None`).
    Respawning {
        since: u64,
        attempts_used: u8,
        last_error: Option<String>,
    },
    /// Terminal: lease is dead and not coming back here.
    Failed {
        reason: String,
    },
}

/// A heartbeat the provider received from the relay pool.
/// `seen_at` is our local clock; `event_timestamp` is the
/// heartbeat's claimed creation time, and anything older than
/// `stale_secs` is ignored to defeat replay-on-relay.
#[derive(Debug, Clone)]
pub struct HeartbeatObservation {
    pub provider_npub: String,
    pub relay_url: String,
    pub seen_at: u64,
    pub event_timestamp: u64,
}

/// Operator-tunable timing + quorum knobs.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct QuorumConfig {
    /// Required count of live relays (M).
    pub m: u8,
    /// Total relay set size (N). Informational — quorum logic only
    /// requires observing `m` distinct live relays.
    pub n: u8,
    /// Live → Suspect after this many seconds with quorum lost.
    pub t1_secs: u64,
    /// Suspect → Evicted after this many seconds without recovery.
    pub t2_secs: u64,
    /// Heartbeats older than this on the wire don't count.
    pub stale_secs: u64,
}

impl Default for QuorumConfig {
    fn default() -> Self {
        Self {
            m: 2,
            n: 3,
            t1_secs: 120,
            t2_secs: 300,
            stale_secs: 180,
        }
    }
}

/// One workload as tracked by a provider.
#[derive(Debug, Clone)]
pub struct DurableWorkload {
    pub workload_id: u32,
    pub provider_npub: String,
    pub state: WorkloadState,
    pub replication: ReplicationMode,
    pub restart_policy: RestartPolicy,
    /// Blossom URI of the latest checkpoint, when there is one.
    pub state_uri: Option<String>,
    pub created_at: u64,
    pub expires_at: u64,
}

/// Side-effects the state machine asks the controller to perform.
/// The state machine never does I/O; the caller translates these
/// into Nostr publishes and backend respawns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateMachineEvent {
    EnteredLive {
        workload_id: u32,
    },
    EnteredSuspect {
        workload_id: u32,
    },
    Evicted {
        workload_id: u32,
        reason: &'static str,
    },
    /// The local state has left `Live`. The controller should
    /// publish a `LeaseRevocation` Nostr event addressed to the
    /// listed standby providers so exactly one of them can promote.
    PublishLeaseRevocation {
        workload_id: u32,
        standby_providers: Vec<String>,
    },
    /// Controller should attempt a local respawn (None + OnFailure).
    /// The result is fed back via `notify_respawn_failed`.
    AttemptRespawn {
        workload_id: u32,
        attempt: u8,
    },
    Failed {
        workload_id: u32,
        reason: String,
    },
}

/// Tracked workloads keyed by `workload_id`, advanced by `tick`.
pub struct WorkloadStateMachine {
    config: QuorumConfig,
    workloads: HashMap<u32, DurableWorkload>,
}

impl WorkloadStateMachine {
    pub fn new(config: QuorumConfig) -> Self {
        Self {
            config,
            workloads: HashMap::new(),
        }
    }

    pub fn track(&mut self, workload: DurableWorkload) {
        self.workloads.insert(workload.workload_id, workload);
    }

    pub fn untrack(&mut self, workload_id: u32) {
        self.workloads.remove(&workload_id);
    }

    pub fn state_of(&self, workload_id: u32) -> Option<&WorkloadState> {
        self.workloads.get(&workload_id).map(|w| &w.state)
    }

    pub fn workload(&self, workload_id: u32) -> Option<&DurableWorkload> {
        self.workloads.get(&workload_id)
    }

    /// Apply observations and advance every tracked workload.
    /// Returns the side-effects the controller must perform.
    pub fn tick(
        &mut self,
        now: u64,
        observations: &[HeartbeatObservation],
    ) -> Vec<StateMachineEvent> {
        let mut events = Vec::new();
        let cfg = self.config;

        for workload in self.workloads.values_mut() {
            // Distinct relays where this provider was observed within
            // stale_secs. `event_timestamp` is trusted because the
            // relay round-trip authenticated the signed event;
            // staleness is what defeats replay.
            let mut live_relays = std::collections::HashSet::new();
            for obs in observations {
                if obs.provider_npub != workload.provider_npub {
                    continue;
                }
                if obs.event_timestamp + cfg.stale_secs < now {
                    continue;
                }
                live_relays.insert(obs.relay_url.clone());
            }
            let quorum_alive = live_relays.len() as u8 >= cfg.m;

            advance(workload, now, quorum_alive, &cfg, &mut events);
        }

        events
    }

    /// Controller reports that a respawn attempt for a workload
    /// failed. The state machine either retries (if attempts
    /// remain) or marks the workload `Failed`.
    pub fn notify_respawn_failed(&mut self, workload_id: u32, reason: &str) {
        let Some(workload) = self.workloads.get_mut(&workload_id) else {
            return;
        };
        let WorkloadState::Respawning {
            since: _,
            attempts_used,
            last_error: _,
        } = &workload.state
        else {
            return;
        };
        let attempts_used = *attempts_used;

        let max = match workload.restart_policy {
            RestartPolicy::OnFailure { max_attempts } => max_attempts,
            RestartPolicy::Never => 0,
        };

        if attempts_used >= max {
            workload.state = WorkloadState::Failed {
                reason: format!(
                    "respawn exhausted after {} attempt(s): {}",
                    attempts_used, reason
                ),
            };
        } else {
            // Hold in Respawning so the controller can re-attempt on
            // its own cadence; record the error for diagnostics.
            workload.state = WorkloadState::Respawning {
                since: workload_state_since(&workload.state).unwrap_or(0),
                attempts_used,
                last_error: Some(reason.to_string()),
            };
        }
    }

    /// Controller reports a successful respawn; heartbeats from the
    /// new container keep the workload in `Live`.
    pub fn notify_respawn_succeeded(&mut self, workload_id: u32, now: u64) {
        if let Some(workload) = self.workloads.get_mut(&workload_id) {
            workload.state = WorkloadState::Live { since: now };
        }
    }
}

fn workload_state_since(state: &WorkloadState) -> Option<u64> {
    match state {
        WorkloadState::Provisioning { since }
        | WorkloadState::Live { since }
        | WorkloadState::Suspect { since }
        | WorkloadState::Respawning { since, .. } => Some(*since),
        WorkloadState::Evicted { at } => Some(*at),
        WorkloadState::Failed { .. } => None,
    }
}

/// Whether losing heartbeat quorum should push this workload toward
/// eviction.
///
/// Quorum is a **provider-level** signal — "the provider could not
/// reach enough relays", not "this container is unhealthy". Only
/// warm-standby can act on it (the standby needs the primary to
/// evict itself first). For everything else there is no standby to
/// promote, so evicting would tear down healthy, paid-for containers
/// during a network blip.
fn failover_eligible(workload: &DurableWorkload) -> bool {
    matches!(workload.replication, ReplicationMode::WarmStandby { .. })
}

fn advance(
    workload: &mut DurableWorkload,
    now: u64,
    quorum_alive: bool,
    cfg: &QuorumConfig,
    events: &mut Vec<StateMachineEvent>,
) {
    match workload.state.clone() {
        WorkloadState::Provisioning { .. } => {
            if quorum_alive {
                workload.state = WorkloadState::Live { since: now };
                events.push(StateMachineEvent::EnteredLive {
                    workload_id: workload.workload_id,
                });
            }
        }
        WorkloadState::Live { since } => {
            if quorum_alive {
                workload.state = WorkloadState::Live { since };
            } else if failover_eligible(workload) && now.saturating_sub(since) >= cfg.t1_secs {
                workload.state = WorkloadState::Suspect { since: now };
                events.push(StateMachineEvent::EnteredSuspect {
                    workload_id: workload.workload_id,
                });
            }
        }
        WorkloadState::Suspect { since } => {
            if quorum_alive {
                workload.state = WorkloadState::Live { since: now };
                events.push(StateMachineEvent::EnteredLive {
                    workload_id: workload.workload_id,
                });
            } else if now.saturating_sub(since) >= cfg.t2_secs {
                evict(workload, now, events);
            }
        }
        WorkloadState::Evicted { .. }
        | WorkloadState::Respawning { .. }
        | WorkloadState::Failed { .. } => {
            // The original container is gone, so regained quorum
            // means nothing here; only notify_respawn_{succeeded,
            // failed} move a workload out of these states.
        }
    }
}

fn evict(workload: &mut DurableWorkload, now: u64, events: &mut Vec<StateMachineEvent>) {
    workload.state = WorkloadState::Evicted { at: now };
    events.push(StateMachineEvent::Evicted {
        workload_id: workload.workload_id,
        reason: "heartbeat-quorum-lost-past-t2",
    });

    match (&workload.replication, workload.restart_policy) {
        (ReplicationMode::WarmStandby { standby_providers }, _) => {
            // Single-writer invariant: revocation is emitted only
            // after the local state has left Live (set to Evicted
            // just above), and we stay in Evicted from here.
            events.push(StateMachineEvent::PublishLeaseRevocation {
                workload_id: workload.workload_id,
                standby_providers: standby_providers.clone(),
            });
        }
        (
            ReplicationMode::None | ReplicationMode::Checkpointed,
            RestartPolicy::OnFailure { max_attempts },
        ) => {
            if max_attempts == 0 {
                workload.state = WorkloadState::Failed {
                    reason: "OnFailure with max_attempts=0".to_string(),
                };
                events.push(StateMachineEvent::Failed {
                    workload_id: workload.workload_id,
                    reason: "OnFailure with max_attempts=0".to_string(),
                });
            } else {
                let attempt = 1u8;
                workload.state = WorkloadState::Respawning {
                    since: now,
                    attempts_used: attempt,
                    last_error: None,
                };
                events.push(StateMachineEvent::AttemptRespawn {
                    workload_id: workload.workload_id,
                    attempt,
                });
            }
        }
        (ReplicationMode::None | ReplicationMode::Checkpointed, RestartPolicy::Never) => {
            workload.state = WorkloadState::Failed {
                reason: "RestartPolicy::Never on eviction".to_string(),
            };
            events.push(StateMachineEvent::Failed {
                workload_id: workload.workload_id,
                reason: "RestartPolicy::Never on eviction".to_string(),
            });
        }
    }
}
