// Kind-9841 CI Job Result, signed by this adapter.
//
// The ngit-ci NIP wants the Job Result signed by "the compute provider that
// ran the job", so that a client can weigh a result by the reputation of the
// key that made the execution claim. The coordinator signs the Workflow Result
// and quotes ours.
//
// The adapter is the entity that can make that claim: it chose the provider,
// rented the box and watched the process exit. The provider itself only knows
// it leased a container to somebody. So we sign, and name the provider and the
// lease we bought, which is what ties the claim to specific hardware someone
// staked on.
//
// Two known deviations from the spec, both to raise with ngit-ci upstream:
//   - one result per *run*, not per job. The coordinator submits one script per
//     workflow run and splits act's JSON into per-job results on its side; we
//     never see job ids, so `job` is omitted rather than invented.
//   - the coordinator has no way to learn our event id, so it cannot `q`-tag
//     us. Clients find our result by `#c` / `#a` filters instead.

use std::collections::BTreeMap;

/// Kind for a CI Job Result. Experimental placeholder in the ngit-ci NIP; it
/// moves if the spec is assigned a permanent number.
pub const KIND_CI_JOB_RESULT: u16 = 9841;

const LOG_TAIL_BYTES: usize = 4096;

/// What the coordinator tells us about the run, read out of the execute
/// message's env map. `workflow_path`, `workflow_sha256` and `trigger` need
/// the upstream `job_env` patch; without them we cannot build a spec-compliant
/// event and publish nothing rather than something malformed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiContext {
    pub repo_coordinate: String,
    pub commit: String,
    pub workflow_path: String,
    pub workflow_sha256: String,
    pub trigger: String,
    pub git_ref: Option<String>,
    pub trigger_event: Option<String>,
}

impl CiContext {
    pub fn from_env(env: &BTreeMap<String, String>) -> Option<Self> {
        let get = |key: &str| env.get(key).map(String::as_str).filter(|v| !v.is_empty());
        Some(Self {
            repo_coordinate: get("NGIT_CI_REPOSITORY")?.to_string(),
            commit: get("GITHUB_SHA")?.to_string(),
            workflow_path: get("NGIT_CI_WORKFLOW_PATH")?.to_string(),
            workflow_sha256: get("NGIT_CI_WORKFLOW_SHA256")?.to_string(),
            trigger: get("NGIT_CI_TRIGGER")?.to_string(),
            git_ref: get("GITHUB_REF").map(String::from),
            trigger_event: get("NGIT_CI_TRIGGER_EVENT").map(String::from),
        })
    }
}

/// Where the job ran, so the claim is auditable back to hardware someone
/// staked on rather than to an anonymous key.
pub struct Lease<'a> {
    pub provider: &'a str,
    pub pod_id: &'a str,
}

pub struct Outcome {
    pub exit_code: i32,
    pub started_at: u64,
}

impl Outcome {
    /// Spec conclusion values. We only ever distinguish these two — a coordinator
    /// timeout drops the connection, so we never reach the publish path at all.
    fn conclusion(&self) -> &'static str {
        if self.exit_code == 0 {
            "success"
        } else {
            "failure"
        }
    }
}

pub fn build_tags(ctx: &CiContext, lease: &Lease, outcome: &Outcome) -> Vec<Vec<String>> {
    let mut tags = vec![
        vec!["a".to_string(), ctx.repo_coordinate.clone()],
        vec!["c".to_string(), ctx.commit.clone()],
        vec![
            "w".to_string(),
            ctx.workflow_path.clone(),
            ctx.workflow_sha256.clone(),
        ],
        vec!["o".to_string(), ctx.trigger.clone()],
        vec!["conclusion".to_string(), outcome.conclusion().to_string()],
        vec!["exit_code".to_string(), outcome.exit_code.to_string()],
        vec!["started_at".to_string(), outcome.started_at.to_string()],
    ];

    // A pull-request run has no ref; a push has no trigger event to point at.
    // The NIP-22 `K`/`P` tags need the trigger event's kind and author, which
    // the coordinator does not pass us, so `E` goes out alone.
    match ctx.trigger.as_str() {
        "pull_request" => {
            if let Some(event) = &ctx.trigger_event {
                tags.push(vec!["E".to_string(), event.clone()]);
            }
        }
        _ => {
            if let Some(git_ref) = &ctx.git_ref {
                tags.push(vec!["r".to_string(), git_ref.clone()]);
            }
        }
    }

    // Our extensions: which rented box this ran on, and the provider whose
    // reputation and stake back the claim. Deliberately not `p` — NIP-22 uses
    // that for the pull-request author, and a client would read the provider
    // key as the person who opened the proposal.
    tags.push(vec!["provider".to_string(), lease.provider.to_string()]);
    tags.push(vec!["lease".to_string(), lease.pod_id.to_string()]);
    tags
}

/// The last few KB of a job's output, in the shape the coordinator uses for
/// its own results so a client rendering both sees one format.
///
/// Bounded as it goes: a chatty build emits hundreds of MB, and holding all of
/// it to publish 4 KB would let any workflow exhaust the adapter's memory.
#[derive(Default)]
pub struct LogTail {
    tail: String,
    total: usize,
}

impl LogTail {
    pub fn push(&mut self, chunk: &str) {
        self.total += chunk.len();
        self.tail.push_str(chunk);
        // Trimming at twice the cap keeps this amortized O(1) rather than
        // shifting the buffer on every chunk.
        if self.tail.len() > LOG_TAIL_BYTES * 2 {
            self.trim_to_cap();
        }
    }

    fn trim_to_cap(&mut self) {
        let mut start = self.tail.len() - LOG_TAIL_BYTES;
        while !self.tail.is_char_boundary(start) {
            start += 1;
        }
        self.tail.drain(..start);
    }

    /// `omitted` counts everything the job wrote, not just what survived
    /// trimming, so the number means the same as the coordinator's.
    pub fn into_content(mut self) -> String {
        if self.tail.len() > LOG_TAIL_BYTES {
            self.trim_to_cap();
        }
        let omitted = self.total - self.tail.len();
        if omitted == 0 {
            return self.tail;
        }
        format!("[log-tail omitted={}]\n{}", omitted, self.tail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn full_env() -> BTreeMap<String, String> {
        env_of(&[
            ("NGIT_CI_REPOSITORY", "30617:abc:ngx-l402"),
            ("GITHUB_SHA", "9d44dbf5"),
            ("GITHUB_REF", "refs/heads/main"),
            ("NGIT_CI_WORKFLOW_PATH", ".ngit/act/workflows/build.yml"),
            ("NGIT_CI_WORKFLOW_SHA256", "eb8bdb49"),
            ("NGIT_CI_TRIGGER", "push"),
            ("NGIT_CI_TRIGGER_EVENT", "340111da"),
        ])
    }

    fn tag<'a>(tags: &'a [Vec<String>], name: &str) -> Option<&'a Vec<String>> {
        tags.iter().find(|t| t[0] == name)
    }

    #[test]
    fn context_needs_the_upstream_env_patch() {
        // Today's coordinator sends only these four; without the workflow and
        // trigger we must not publish a half-built event.
        let old = env_of(&[
            ("NGIT_CI_REPOSITORY", "30617:abc:demo"),
            ("GITHUB_SHA", "deadbeef"),
            ("GITHUB_REF", "refs/heads/main"),
            ("NGIT_CI_TRIGGER_EVENT", "aaaa"),
        ]);
        assert!(CiContext::from_env(&old).is_none());
        assert!(CiContext::from_env(&full_env()).is_some());
    }

    #[test]
    fn empty_values_count_as_absent() {
        let mut env = full_env();
        env.insert("NGIT_CI_WORKFLOW_SHA256".to_string(), String::new());
        assert!(CiContext::from_env(&env).is_none());
    }

    #[test]
    fn push_runs_carry_the_ref_and_no_pr_root() {
        let ctx = CiContext::from_env(&full_env()).expect("context");
        let tags = build_tags(
            &ctx,
            &Lease {
                provider: "c0ca9e7d",
                pod_id: "container-2007",
            },
            &Outcome {
                exit_code: 0,
                started_at: 1785160959,
            },
        );
        assert_eq!(tag(&tags, "r").unwrap()[1], "refs/heads/main");
        assert!(tag(&tags, "E").is_none());
        assert_eq!(tag(&tags, "conclusion").unwrap()[1], "success");
        assert_eq!(
            tag(&tags, "w").unwrap()[1..],
            [".ngit/act/workflows/build.yml", "eb8bdb49"]
        );
        assert_eq!(tag(&tags, "provider").unwrap()[1], "c0ca9e7d");
        assert_eq!(tag(&tags, "lease").unwrap()[1], "container-2007");
    }

    #[test]
    fn pull_request_runs_carry_the_root_and_no_ref() {
        let mut env = full_env();
        env.insert("NGIT_CI_TRIGGER".to_string(), "pull_request".to_string());
        let ctx = CiContext::from_env(&env).expect("context");
        let tags = build_tags(
            &ctx,
            &Lease {
                provider: "c0ca9e7d",
                pod_id: "container-2007",
            },
            &Outcome {
                exit_code: 1,
                started_at: 1,
            },
        );
        assert_eq!(tag(&tags, "E").unwrap()[1], "340111da");
        assert!(
            tag(&tags, "p").is_none(),
            "NIP-22 reserves `p` for the proposal author"
        );
        assert!(tag(&tags, "r").is_none(), "a PR run has no ref");
        assert_eq!(tag(&tags, "conclusion").unwrap()[1], "failure");
        assert_eq!(tag(&tags, "exit_code").unwrap()[1], "1");
    }

    #[test]
    fn short_logs_are_untouched() {
        let mut tail = LogTail::default();
        tail.push("hello");
        assert_eq!(tail.into_content(), "hello");
    }

    #[test]
    fn long_logs_report_everything_the_job_wrote() {
        let mut tail = LogTail::default();
        tail.push(&"x".repeat(LOG_TAIL_BYTES + 100));
        let content = tail.into_content();
        assert!(content.starts_with("[log-tail omitted=100]\n"));
        assert_eq!(
            content.len(),
            "[log-tail omitted=100]\n".len() + LOG_TAIL_BYTES
        );
    }

    #[test]
    fn memory_stays_bounded_across_many_chunks() {
        // A verbose build streams far more than we will ever publish; holding
        // all of it would let any workflow exhaust the adapter.
        let mut tail = LogTail::default();
        for _ in 0..1000 {
            tail.push(&"y".repeat(LOG_TAIL_BYTES));
        }
        assert!(
            tail.tail.len() <= LOG_TAIL_BYTES * 2,
            "buffer grew to {}",
            tail.tail.len()
        );
        assert!(tail.into_content().starts_with("[log-tail omitted="));
    }

    #[test]
    fn trimming_never_splits_a_codepoint() {
        // Job output is arbitrary UTF-8 from someone else's build.
        let mut tail = LogTail::default();
        tail.push(&"é".repeat(LOG_TAIL_BYTES));
        assert!(tail.into_content().contains('é'));
    }
}
