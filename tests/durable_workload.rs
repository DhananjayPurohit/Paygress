//! State-machine tests for `paygress::durable_workload`: one deterministic
//! test per transition, plus a proptest for the single-writer invariant.
//! Multi-provider integration and spawn-from-checkpoint are out of scope.

use proptest::prelude::*;

use paygress::durable_workload::{
    DurableWorkload, HeartbeatObservation, QuorumConfig, ReplicationMode, RestartPolicy,
    StateMachineEvent, WorkloadState, WorkloadStateMachine,
};

const PROVIDER_A: &str = "npub1providera";
const PROVIDER_B: &str = "npub1providerb";
const RELAYS: [&str; 3] = ["wss://r1", "wss://r2", "wss://r3"];

fn quorum() -> QuorumConfig {
    QuorumConfig {
        m: 2,
        n: 3,
        t1_secs: 120,
        t2_secs: 300,
        stale_secs: 180,
    }
}

fn workload(id: u32, provider: &str, replication: ReplicationMode, now: u64) -> DurableWorkload {
    DurableWorkload {
        workload_id: id,
        provider_npub: provider.to_string(),
        state: WorkloadState::Provisioning { since: now },
        replication,
        restart_policy: RestartPolicy::OnFailure { max_attempts: 3 },
        state_uri: None,
        created_at: now,
        expires_at: now + 3600,
    }
}

/// Heartbeat quorum is a provider-level signal, so only warm-standby
/// workloads act on losing it — they have a standby to promote. Tests
/// exercising the Suspect / eviction path must use this mode.
fn warm_standby() -> ReplicationMode {
    ReplicationMode::WarmStandby {
        standby_providers: vec![PROVIDER_B.to_string()],
    }
}

fn observation(provider: &str, relay: &str, when: u64) -> HeartbeatObservation {
    HeartbeatObservation {
        provider_npub: provider.to_string(),
        relay_url: relay.to_string(),
        seen_at: when,
        event_timestamp: when,
    }
}

/// A full N-of-N heartbeat sweep for `PROVIDER_A` at `when`.
fn all_relays(when: u64) -> Vec<HeartbeatObservation> {
    RELAYS
        .iter()
        .map(|r| observation(PROVIDER_A, r, when))
        .collect()
}

#[test]
fn initial_state_is_provisioning() {
    let mut sm = WorkloadStateMachine::new(quorum());
    sm.track(workload(1, PROVIDER_A, ReplicationMode::None, 0));
    assert!(matches!(
        sm.state_of(1),
        Some(WorkloadState::Provisioning { .. })
    ));
}

#[test]
fn provisioning_advances_to_live_after_quorum() {
    let mut sm = WorkloadStateMachine::new(quorum());
    sm.track(workload(1, PROVIDER_A, ReplicationMode::None, 0));

    let _ = sm.tick(10, &all_relays(10));

    assert!(matches!(sm.state_of(1), Some(WorkloadState::Live { .. })));
}

#[test]
fn live_stays_live_with_one_relay_silent() {
    let mut sm = WorkloadStateMachine::new(quorum());
    sm.track(workload(1, PROVIDER_A, ReplicationMode::None, 0));
    let _ = sm.tick(10, &all_relays(10));
    assert!(matches!(sm.state_of(1), Some(WorkloadState::Live { .. })));

    // M=2 of N=3 is still met.
    let _ = sm.tick(
        70,
        &[
            observation(PROVIDER_A, RELAYS[0], 70),
            observation(PROVIDER_A, RELAYS[1], 70),
        ],
    );
    assert!(matches!(sm.state_of(1), Some(WorkloadState::Live { .. })));
}

#[test]
fn live_transitions_to_suspect_after_t1_silence() {
    let mut sm = WorkloadStateMachine::new(quorum());
    sm.track(workload(1, PROVIDER_A, warm_standby(), 0));
    let _ = sm.tick(10, &all_relays(10));

    // T1 = 120s of silence.
    let _ = sm.tick(200, &[]);
    assert!(matches!(
        sm.state_of(1),
        Some(WorkloadState::Suspect { .. })
    ));
}

#[test]
fn suspect_recovers_to_live_within_t2() {
    let mut sm = WorkloadStateMachine::new(quorum());
    sm.track(workload(1, PROVIDER_A, warm_standby(), 0));
    let _ = sm.tick(10, &all_relays(10));
    let _ = sm.tick(200, &[]); // → Suspect

    // Heartbeats resume on M-of-N before T2.
    let _ = sm.tick(220, &all_relays(220));
    assert!(matches!(sm.state_of(1), Some(WorkloadState::Live { .. })));
}

#[test]
fn suspect_evicts_after_t2_silence() {
    let mut sm = WorkloadStateMachine::new(quorum());
    sm.track(workload(1, PROVIDER_A, warm_standby(), 0));
    let _ = sm.tick(10, &all_relays(10));
    let _ = sm.tick(200, &[]); // → Suspect at t≈200
    let events = sm.tick(600, &[]); // well past T2 = 300s

    assert!(matches!(
        sm.state_of(1),
        Some(
            WorkloadState::Evicted { .. }
                | WorkloadState::Respawning { .. }
                | WorkloadState::Failed { .. }
        )
    ));
    assert!(events
        .iter()
        .any(|e| matches!(e, StateMachineEvent::Evicted { workload_id: 1, .. })));
}

#[test]
fn non_replicated_workload_survives_quorum_loss() {
    // Regression: this path used to evict, and eviction dropped the workload
    // without deleting the container — leaking it and burning its vmid.
    let mut sm = WorkloadStateMachine::new(quorum());
    sm.track(workload(1, PROVIDER_A, ReplicationMode::None, 0));
    let _ = sm.tick(10, &all_relays(10));

    // Total relay silence, well past both T1 and T2.
    let _ = sm.tick(200, &[]);
    let events = sm.tick(5000, &[]);

    assert!(
        matches!(sm.state_of(1), Some(WorkloadState::Live { .. })),
        "non-replicated workload must stay Live through quorum loss; got {:?}",
        sm.state_of(1)
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, StateMachineEvent::Evicted { .. })),
        "no eviction event expected; got events={:?}",
        events
    );
}

#[test]
fn checkpointed_workload_also_survives_quorum_loss() {
    // Checkpointed has no standby either.
    let mut sm = WorkloadStateMachine::new(quorum());
    sm.track(workload(1, PROVIDER_A, ReplicationMode::Checkpointed, 0));
    let _ = sm.tick(10, &all_relays(10));
    let _ = sm.tick(200, &[]);
    let _ = sm.tick(5000, &[]);

    assert!(matches!(sm.state_of(1), Some(WorkloadState::Live { .. })));
}

#[test]
fn warm_standby_eviction_emits_lease_revocation() {
    let mut sm = WorkloadStateMachine::new(quorum());
    let replication = ReplicationMode::WarmStandby {
        standby_providers: vec![PROVIDER_B.to_string()],
    };
    sm.track(workload(1, PROVIDER_A, replication, 0));
    let _ = sm.tick(10, &all_relays(10));
    let _ = sm.tick(200, &[]);
    let events = sm.tick(600, &[]);

    let revocation_emitted = events.iter().any(|e| {
        matches!(
            e,
            StateMachineEvent::PublishLeaseRevocation { workload_id: 1, .. }
        )
    });
    assert!(
        revocation_emitted,
        "warm-standby eviction must emit PublishLeaseRevocation; got events={:?}",
        events
    );
}

#[test]
fn stale_observation_does_not_count_for_quorum() {
    let mut sm = WorkloadStateMachine::new(quorum());
    sm.track(workload(1, PROVIDER_A, ReplicationMode::None, 0));

    // event_timestamp is an hour before the tick; stale_secs = 180.
    let stale = HeartbeatObservation {
        provider_npub: PROVIDER_A.to_string(),
        relay_url: RELAYS[0].to_string(),
        seen_at: 10,
        event_timestamp: 10u64.saturating_sub(3600),
    };
    let _ = sm.tick(10, &[stale]);

    assert!(
        matches!(sm.state_of(1), Some(WorkloadState::Provisioning { .. })),
        "stale heartbeat must not advance Provisioning → Live"
    );
}

#[test]
fn untrack_removes_workload() {
    let mut sm = WorkloadStateMachine::new(quorum());
    sm.track(workload(1, PROVIDER_A, ReplicationMode::None, 0));
    sm.untrack(1);
    assert!(sm.state_of(1).is_none());
}

#[test]
fn respawn_failure_after_max_attempts_goes_to_failed() {
    // Entered directly: nothing reaches Respawning today, because
    // warm-standby evicts to a standby and non-replicated workloads no
    // longer evict on quorum. The trigger this waits for is a
    // container-health signal the provider does not yet observe.
    let mut sm = WorkloadStateMachine::new(quorum());
    let mut wl = workload(1, PROVIDER_A, ReplicationMode::None, 0);
    wl.restart_policy = RestartPolicy::OnFailure { max_attempts: 1 };
    wl.state = WorkloadState::Respawning {
        since: 600,
        attempts_used: 1,
        last_error: None,
    };
    sm.track(wl);

    sm.notify_respawn_failed(1, "backend down");

    assert!(matches!(sm.state_of(1), Some(WorkloadState::Failed { .. })));
}

proptest! {
    /// Cross-provider single-writer invariant: whenever the machine emits a
    /// `PublishLeaseRevocation`, its own state must already have left `Live`.
    /// A standby only becomes Live after observing that revocation, so
    /// two-Live windows are impossible by construction.
    #[test]
    fn warm_standby_revocation_only_after_local_eviction(
        seed in any::<u64>(),
    ) {
        use rand::{Rng, SeedableRng};
        let mut rng = rand::rngs::StdRng::seed_from_u64(seed);

        let mut sm = WorkloadStateMachine::new(quorum());
        sm.track(workload(
            1,
            PROVIDER_A,
            ReplicationMode::WarmStandby {
                standby_providers: vec![PROVIDER_B.to_string()],
            },
            0,
        ));

        let mut t = 0u64;
        for _ in 0..100 {
            t += rng.gen_range(10..120);
            let obs: Vec<_> = RELAYS
                .iter()
                .filter(|_| rng.gen_bool(0.7))
                .map(|r| observation(PROVIDER_A, r, t))
                .collect();
            let events = sm.tick(t, &obs);

            for ev in &events {
                if matches!(
                    ev,
                    StateMachineEvent::PublishLeaseRevocation { workload_id: 1, .. }
                ) {
                    let st = sm.state_of(1);
                    prop_assert!(
                        !matches!(st, Some(WorkloadState::Live { .. })),
                        "revocation emitted while local state is still Live: {:?}",
                        st
                    );
                }
            }
        }
    }
}
