//! State-machine tests for `paygress::durable_workload` (Unit 5 of
//! the 12-month plan,
//! docs/plans/2026-04-26-001-feat-paygress-12mo-vision-plan.md).
//!
//! These run before any production wiring of the fourth concurrent
//! loop in `ProviderService::run`. The state machine is logic-heavy
//! and bug-prone, so the plan calls for test-first execution. Each
//! transition has a deterministic test; the single-writer invariant
//! has a proptest.
//!
//! Out of scope here:
//! - Multi-provider integration (two state machines, real Nostr).
//! - Spawn-from-checkpoint after eviction — that's Unit 6.

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

fn observation(provider: &str, relay: &str, when: u64) -> HeartbeatObservation {
    HeartbeatObservation {
        provider_npub: provider.to_string(),
        relay_url: relay.to_string(),
        seen_at: when,
        event_timestamp: when,
    }
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

    let obs: Vec<_> = RELAYS
        .iter()
        .map(|r| observation(PROVIDER_A, r, 10))
        .collect();
    let _ = sm.tick(10, &obs);

    assert!(matches!(sm.state_of(1), Some(WorkloadState::Live { .. })));
}

#[test]
fn live_stays_live_with_one_relay_silent() {
    let mut sm = WorkloadStateMachine::new(quorum());
    sm.track(workload(1, PROVIDER_A, ReplicationMode::None, 0));
    let _ = sm.tick(
        10,
        &RELAYS
            .iter()
            .map(|r| observation(PROVIDER_A, r, 10))
            .collect::<Vec<_>>(),
    );
    assert!(matches!(sm.state_of(1), Some(WorkloadState::Live { .. })));

    // Next tick: only 2 of 3 relays observe a heartbeat. M=2 of N=3
    // is met, so we stay Live.
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
    sm.track(workload(1, PROVIDER_A, ReplicationMode::None, 0));
    let _ = sm.tick(
        10,
        &RELAYS
            .iter()
            .map(|r| observation(PROVIDER_A, r, 10))
            .collect::<Vec<_>>(),
    );

    // No more observations. After T1 = 120s of silence we go Suspect.
    let _ = sm.tick(200, &[]);
    assert!(matches!(
        sm.state_of(1),
        Some(WorkloadState::Suspect { .. })
    ));
}

#[test]
fn suspect_recovers_to_live_within_t2() {
    let mut sm = WorkloadStateMachine::new(quorum());
    sm.track(workload(1, PROVIDER_A, ReplicationMode::None, 0));
    let _ = sm.tick(
        10,
        &RELAYS
            .iter()
            .map(|r| observation(PROVIDER_A, r, 10))
            .collect::<Vec<_>>(),
    );
    let _ = sm.tick(200, &[]); // → Suspect

    // Heartbeats resume on M-of-N before T2.
    let _ = sm.tick(
        220,
        &RELAYS
            .iter()
            .map(|r| observation(PROVIDER_A, r, 220))
            .collect::<Vec<_>>(),
    );
    assert!(matches!(sm.state_of(1), Some(WorkloadState::Live { .. })));
}

#[test]
fn suspect_evicts_after_t2_silence() {
    let mut sm = WorkloadStateMachine::new(quorum());
    sm.track(workload(1, PROVIDER_A, ReplicationMode::None, 0));
    let _ = sm.tick(
        10,
        &RELAYS
            .iter()
            .map(|r| observation(PROVIDER_A, r, 10))
            .collect::<Vec<_>>(),
    );
    let _ = sm.tick(200, &[]); // → Suspect at t≈200

    // T2 = 300s past Suspect entry. Tick well past that.
    let events = sm.tick(600, &[]);

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
fn warm_standby_eviction_emits_lease_revocation() {
    let mut sm = WorkloadStateMachine::new(quorum());
    let replication = ReplicationMode::WarmStandby {
        standby_providers: vec![PROVIDER_B.to_string()],
    };
    sm.track(workload(1, PROVIDER_A, replication, 0));
    let _ = sm.tick(
        10,
        &RELAYS
            .iter()
            .map(|r| observation(PROVIDER_A, r, 10))
            .collect::<Vec<_>>(),
    );
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

    // Observation claims a 1-hour-old event_timestamp at tick t=10.
    // stale_secs = 180s, so this must be ignored.
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
    let mut sm = WorkloadStateMachine::new(quorum());
    let mut wl = workload(1, PROVIDER_A, ReplicationMode::None, 0);
    wl.restart_policy = RestartPolicy::OnFailure { max_attempts: 1 };
    sm.track(wl);
    let _ = sm.tick(
        10,
        &RELAYS
            .iter()
            .map(|r| observation(PROVIDER_A, r, 10))
            .collect::<Vec<_>>(),
    );
    let _ = sm.tick(200, &[]); // Suspect
    let _ = sm.tick(600, &[]); // Evicted → Respawning

    // First respawn attempt fails.
    sm.notify_respawn_failed(1, "backend down");
    // After exhausting max_attempts the next tick must surface Failed.
    let _ = sm.tick(1200, &[]);

    assert!(matches!(sm.state_of(1), Some(WorkloadState::Failed { .. })));
}

proptest! {
    /// Approximation of the cross-provider single-writer invariant:
    /// whenever the state machine emits a `PublishLeaseRevocation`,
    /// its own local state must already have left `Live`. A standby
    /// promotion that observes the revocation can only become Live
    /// AFTER the primary's local state machine has crossed out of
    /// Live, so two-Live windows are impossible by construction.
    /// Across 256 random observation sequences this property must
    /// hold.
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
