use bd_core::{
    Dependency, DependencyType, Issue, IssueFilter, Priority, SortPolicy, Status, WorkType,
};
use bd_storage::{Backend, Error, Identity, IssuePatch, Locator, Storage};
use chrono::{Duration, Utc};
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A throwaway workspace on disk. In-memory SQLite would be faster, but `init`
/// takes a directory and writes a locator, and testing the real entry point is
/// worth the file I/O.
struct Ws(PathBuf);

impl Drop for Ws {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn workspace(actor: &str) -> (Ws, Box<dyn Storage>) {
    let dir = std::env::temp_dir().join(format!("bd-sqlite-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = crate::init(&dir, "bd", Identity::new(actor)).await.unwrap();
    (Ws(dir), store)
}

async fn ws() -> (Ws, Box<dyn Storage>) {
    workspace("alice").await
}

/// A second store over the same file, acting as somebody else.
async fn as_other(dir: &Ws, actor: &str) -> Box<dyn Storage> {
    let beads = dir.0.join(".beads");
    let loc = Locator::load(&beads).unwrap();
    crate::open(&loc, Identity::new(actor)).await.unwrap()
}

async fn mk(s: &dyn Storage, id: &str) -> Issue {
    s.create_issue(&Issue::new(id, format!("issue {id}")))
        .await
        .unwrap()
}

async fn dep(s: &dyn Storage, from: &str, to: &str, t: DependencyType) {
    s.add_dependency(&Dependency::new(from, to, t).unwrap())
        .await
        .unwrap();
}

async fn ready_ids(s: &dyn Storage) -> Vec<String> {
    s.ready_work(&IssueFilter::ready())
        .await
        .unwrap()
        .into_iter()
        .map(|i| i.id)
        .collect()
}

async fn ready_order(s: &dyn Storage, sort: SortPolicy) -> Vec<String> {
    let f = IssueFilter {
        sort,
        ..IssueFilter::ready()
    };
    s.ready_work(&f)
        .await
        .unwrap()
        .into_iter()
        .map(|i| i.id)
        .collect()
}

async fn list_ids(s: &dyn Storage, f: IssueFilter) -> Vec<String> {
    let mut ids: Vec<String> = s
        .list_issues(&f)
        .await
        .unwrap()
        .into_iter()
        .map(|i| i.id)
        .collect();
    ids.sort();
    ids
}

async fn blocked_ids(s: &dyn Storage) -> Vec<String> {
    let mut ids: Vec<String> = s
        .blocked_work(&IssueFilter::blocked())
        .await
        .unwrap()
        .into_iter()
        .map(|i| i.id)
        .collect();
    ids.sort();
    ids
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn init_writes_a_locator_and_records_the_prefix() {
    let (dir, store) = ws().await;

    let loc = Locator::load(&dir.0.join(".beads")).unwrap();
    assert_eq!(loc.backend, Backend::Sqlite);
    assert!(dir.0.join(".beads/beads.db").exists());

    assert_eq!(
        store.get_config(crate::PREFIX_KEY).await.unwrap().as_deref(),
        Some("bd")
    );
    assert_eq!(store.backend(), Backend::Sqlite);

    // Rule 4: SQLite genuinely has no commit graph and says so plainly.
    assert!(store.version_control().is_none());
    assert!(!store.has_commit_graph());
}

#[tokio::test]
async fn create_and_get_round_trip_with_relations() {
    let (_d, s) = ws().await;

    let mut issue = Issue::new("bd-a", "a title");
    issue.description = "body".into();
    issue.priority = Priority::CRITICAL;
    issue.labels = vec!["infra".into(), "urgent".into()];
    issue.metadata = Some(serde_json::json!({"source": "test"}));
    issue.estimated_minutes = Some(30);
    s.create_issue(&issue).await.unwrap();

    mk(&*s, "bd-b").await;
    dep(&*s, "bd-a", "bd-b", DependencyType::Related).await;
    s.add_comment("bd-a", "looks fine").await.unwrap();

    let got = s.get_issue("bd-a").await.unwrap().unwrap();
    assert_eq!(got.title, "a title");
    assert_eq!(got.priority, Priority::CRITICAL);
    assert_eq!(got.labels, vec!["infra", "urgent"]);
    assert_eq!(got.metadata, issue.metadata);
    assert_eq!(got.estimated_minutes, Some(30));
    assert_eq!(got.dependencies.len(), 1);
    assert_eq!(got.comments.len(), 1);
    assert_eq!(got.comments[0].author, "alice");
    assert_eq!(got.created_by, "alice");
    assert!(!got.content_hash.is_empty());

    // list does not hydrate: one query, not N.
    let listed = s.list_issues(&IssueFilter::new()).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().all(|i| i.labels.is_empty()));

    assert!(s.get_issue("bd-nope").await.unwrap().is_none());
    assert!(matches!(
        s.create_issue(&issue).await,
        Err(Error::AlreadyExists(_))
    ));
}

// ---------------------------------------------------------------------------
// The fixpoint. This is the test that matters.
// ---------------------------------------------------------------------------

/// `bd-e` blocks `bd-d`; `bd-c` is a child of `bd-d`; `bd-b` a child of `bd-c`;
/// `bd-a` a child of `bd-b`. Closing `bd-e` must free the entire chain.
///
/// The ids run *backwards* down the chain on purpose. Both the id index and the
/// rowid order then visit the deepest descendant first, which is the adverse
/// order: a single mark/unmark pass reaches `bd-c` before `bd-d` has learned it
/// is free, so it propagates exactly one level and leaves `bd-c`, `bd-b` and
/// `bd-a` wrongly blocked. That is not a contrived arrangement — it is what a
/// workspace looks like whenever a child is filed before its parent.
///
/// `blocked::tests::one_pass_leaves_the_deep_end_of_the_chain_wrong` pins the
/// same thing one layer down, by running a single pass and watching it fail.
#[tokio::test]
async fn transitive_parent_child_blocking_requires_a_fixpoint() {
    let (_d, s) = ws().await;

    for id in ["bd-a", "bd-b", "bd-c", "bd-d", "bd-e"] {
        mk(&*s, id).await;
    }

    dep(&*s, "bd-d", "bd-e", DependencyType::Blocks).await;
    dep(&*s, "bd-c", "bd-d", DependencyType::ParentChild).await;
    dep(&*s, "bd-b", "bd-c", DependencyType::ParentChild).await;
    dep(&*s, "bd-a", "bd-b", DependencyType::ParentChild).await;

    // Blocked-ness has already propagated *down* the whole chain on the way in.
    assert_eq!(blocked_ids(&*s).await, vec!["bd-a", "bd-b", "bd-c", "bd-d"]);
    assert_eq!(ready_ids(&*s).await, vec!["bd-e"]);

    s.close_issue("bd-e", "done").await.unwrap();

    assert_eq!(
        blocked_ids(&*s).await,
        Vec::<String>::new(),
        "a single-pass recompute leaves the deep end of the chain blocked"
    );
    let mut ready = ready_ids(&*s).await;
    ready.sort();
    assert_eq!(ready, vec!["bd-a", "bd-b", "bd-c", "bd-d"]);

    // The graph is at a fixpoint: a full recompute must find nothing to do.
    assert_eq!(s.recompute_blocked().await.unwrap(), 0);
}

/// `is_blocked` is derived state. Recomputing it must not stamp this clone's
/// wall clock onto a version-controlled row — two clones that recompute the same
/// flip a second apart would then hand the merge a conflict on a column neither
/// of them edited, and every stale-guard that reads `updated_at` as "a human
/// touched this" would be wrong.
#[tokio::test]
async fn recomputing_is_blocked_does_not_bump_updated_at() {
    let (_d, s) = ws().await;
    mk(&*s, "bd-a").await;
    mk(&*s, "bd-b").await;
    dep(&*s, "bd-b", "bd-a", DependencyType::Blocks).await;

    let before = s.get_issue("bd-b").await.unwrap().unwrap().updated_at;
    assert_eq!(blocked_ids(&*s).await, vec!["bd-b"]);

    s.close_issue("bd-a", "done").await.unwrap();

    let after = s.get_issue("bd-b").await.unwrap().unwrap();
    assert!(blocked_ids(&*s).await.is_empty(), "bd-b should be free now");
    assert_eq!(
        after.updated_at, before,
        "the is_blocked recompute bumped updated_at on a row nobody edited"
    );
}

#[tokio::test]
async fn closing_a_blocker_frees_its_dependers_and_reopening_re_blocks_them() {
    let (_d, s) = ws().await;
    mk(&*s, "bd-a").await;
    mk(&*s, "bd-b").await;
    dep(&*s, "bd-b", "bd-a", DependencyType::Blocks).await;

    assert_eq!(blocked_ids(&*s).await, vec!["bd-b"]);
    s.close_issue("bd-a", "done").await.unwrap();
    assert_eq!(blocked_ids(&*s).await, Vec::<String>::new());
    s.reopen_issue("bd-a").await.unwrap();
    assert_eq!(blocked_ids(&*s).await, vec!["bd-b"]);
}

/// `B conditional-blocks A` = "run B only if A **fails**". A clean close of A
/// means the failure path is moot, so B stays blocked; a failing close releases
/// it. Getting this backwards would run recovery work after a success.
#[tokio::test]
async fn conditional_blocks_releases_only_on_a_failing_close() {
    let (_d, s) = ws().await;
    mk(&*s, "bd-a").await;
    mk(&*s, "bd-b").await;
    dep(&*s, "bd-b", "bd-a", DependencyType::ConditionalBlocks).await;

    assert_eq!(blocked_ids(&*s).await, vec!["bd-b"]);

    s.close_issue("bd-a", "done").await.unwrap();
    assert_eq!(
        blocked_ids(&*s).await,
        vec!["bd-b"],
        "a successful close must NOT arm the failure path"
    );

    s.reopen_issue("bd-a").await.unwrap();
    s.close_issue("bd-a", "failed: could not reproduce")
        .await
        .unwrap();
    assert_eq!(blocked_ids(&*s).await, Vec::<String>::new());
    assert!(ready_ids(&*s).await.contains(&"bd-b".to_string()));
}

/// A `waits-for` gate is over the *spawner's children*, not the spawner — the
/// waiter has no edge to the beads that actually gate it.
#[tokio::test]
async fn waits_for_gates_on_the_spawners_children() {
    let (_d, s) = ws().await;
    for id in ["bd-s", "bd-c1", "bd-c2", "bd-w"] {
        mk(&*s, id).await;
    }
    dep(&*s, "bd-c1", "bd-s", DependencyType::ParentChild).await;
    dep(&*s, "bd-c2", "bd-s", DependencyType::ParentChild).await;
    dep(&*s, "bd-w", "bd-s", DependencyType::WaitsFor).await;

    assert_eq!(blocked_ids(&*s).await, vec!["bd-w"]);

    s.close_issue("bd-c1", "done").await.unwrap();
    assert_eq!(
        blocked_ids(&*s).await,
        vec!["bd-w"],
        "the default gate needs *every* child done"
    );

    s.close_issue("bd-c2", "done").await.unwrap();
    assert_eq!(blocked_ids(&*s).await, Vec::<String>::new());
}

#[tokio::test]
async fn waits_for_any_children_gate_opens_on_the_first_close() {
    let (_d, s) = ws().await;
    for id in ["bd-s", "bd-c1", "bd-c2", "bd-w"] {
        mk(&*s, id).await;
    }
    dep(&*s, "bd-c1", "bd-s", DependencyType::ParentChild).await;
    dep(&*s, "bd-c2", "bd-s", DependencyType::ParentChild).await;

    let mut d = Dependency::new("bd-w", "bd-s", DependencyType::WaitsFor).unwrap();
    d.metadata = r#"{"gate":"any-children"}"#.to_string();
    s.add_dependency(&d).await.unwrap();

    assert_eq!(blocked_ids(&*s).await, vec!["bd-w"]);
    s.close_issue("bd-c1", "done").await.unwrap();
    assert_eq!(blocked_ids(&*s).await, Vec::<String>::new());
}

#[tokio::test]
async fn deleting_a_blocker_frees_what_it_blocked() {
    let (_d, s) = ws().await;
    mk(&*s, "bd-a").await;
    mk(&*s, "bd-b").await;
    mk(&*s, "bd-c").await;
    dep(&*s, "bd-b", "bd-a", DependencyType::Blocks).await;
    dep(&*s, "bd-c", "bd-b", DependencyType::ParentChild).await;
    assert_eq!(blocked_ids(&*s).await, vec!["bd-b", "bd-c"]);

    s.delete_issue("bd-a").await.unwrap();

    assert_eq!(blocked_ids(&*s).await, Vec::<String>::new());
    assert!(s.get_issue("bd-a").await.unwrap().is_none());
    // The audit trail outlives the row.
    assert!(
        s.list_events("bd-a")
            .await
            .unwrap()
            .iter()
            .any(|e| e.event_type == bd_core::EventType::Deleted)
    );
}

#[tokio::test]
async fn removing_an_edge_frees_the_depender() {
    let (_d, s) = ws().await;
    mk(&*s, "bd-a").await;
    mk(&*s, "bd-b").await;
    dep(&*s, "bd-b", "bd-a", DependencyType::Blocks).await;
    assert_eq!(blocked_ids(&*s).await, vec!["bd-b"]);

    s.remove_dependency("bd-b", "bd-a", &DependencyType::Blocks)
        .await
        .unwrap();
    assert_eq!(blocked_ids(&*s).await, Vec::<String>::new());
    assert!(s.dependencies_of("bd-b").await.unwrap().is_empty());
    assert!(matches!(
        s.remove_dependency("bd-b", "bd-a", &DependencyType::Blocks)
            .await,
        Err(Error::NotFound(_))
    ));
}

/// **Data loss.** Two beads may be joined by several edges at once — the schema's
/// primary key says so — so a delete keyed on the pair alone takes all of them.
///
/// `bd dep remove A B` was therefore a way to destroy an edge you never named,
/// silently, while reporting success. Nothing in the graph says an edge used to
/// be there.
#[tokio::test]
async fn removing_one_edge_type_leaves_the_other_edges_between_the_pair_alone() {
    let (_d, s) = ws().await;
    mk(&*s, "bd-a").await;
    mk(&*s, "bd-b").await;

    dep(&*s, "bd-b", "bd-a", DependencyType::Blocks).await;
    dep(&*s, "bd-b", "bd-a", DependencyType::Related).await;
    assert_eq!(s.dependencies_of("bd-b").await.unwrap().len(), 2);

    s.remove_dependency("bd-b", "bd-a", &DependencyType::Blocks)
        .await
        .unwrap();

    let left = s.dependencies_of("bd-b").await.unwrap();
    assert_eq!(
        left.len(),
        1,
        "removing the `blocks` edge also destroyed the `related` one: {left:?}"
    );
    assert_eq!(left[0].dep_type, DependencyType::Related);
    // And the gate really did lift — this is not passing because nothing was
    // deleted at all.
    assert_eq!(blocked_ids(&*s).await, Vec::<String>::new());

    // An edge that is not there is an error, not a shrug: a typo'd edge type
    // must not report that it removed something.
    assert!(matches!(
        s.remove_dependency("bd-b", "bd-a", &DependencyType::Blocks)
            .await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
async fn a_pinned_bead_neither_blocks_nor_is_blocked() {
    let (_d, s) = ws().await;
    mk(&*s, "bd-a").await;
    mk(&*s, "bd-b").await;
    dep(&*s, "bd-b", "bd-a", DependencyType::Blocks).await;
    assert_eq!(blocked_ids(&*s).await, vec!["bd-b"]);

    s.update_issue(
        "bd-a",
        &IssuePatch {
            pinned: Some(true),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(blocked_ids(&*s).await, Vec::<String>::new());
    assert_eq!(ready_ids(&*s).await, vec!["bd-b"]);
}

// ---------------------------------------------------------------------------
// Claims
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_claim_is_exclusive_until_it_lapses() {
    let (dir, alice) = ws().await;
    let bob = as_other(&dir, "bob").await;
    mk(&*alice, "bd-a").await;

    let claim = alice
        .claim_issue("bd-a", Duration::hours(1))
        .await
        .unwrap();
    assert_eq!(claim.holder, "alice");

    let held = alice.get_issue("bd-a").await.unwrap().unwrap();
    assert_eq!(held.status, Status::InProgress);
    assert_eq!(held.assignee, "alice");
    assert!(held.started_at.is_some());

    match bob.claim_issue("bd-a", Duration::hours(1)).await {
        Err(Error::AlreadyClaimed { id, holder }) => {
            assert_eq!(id, "bd-a");
            assert_eq!(holder, "alice");
        }
        other => panic!("bob stole an unexpired claim: {other:?}"),
    }

    // The holder may always re-claim; that is a renewal, not a contest.
    assert!(alice.claim_issue("bd-a", Duration::hours(1)).await.is_ok());

    alice.release_claim("bd-a").await.unwrap();
    assert!(bob.claim_issue("bd-a", Duration::hours(1)).await.is_ok());
}

#[tokio::test]
async fn open_competition_beads_may_be_claimed_by_everybody_at_once() {
    let (dir, alice) = ws().await;
    let bob = as_other(&dir, "bob").await;

    let mut issue = Issue::new("bd-a", "shared");
    issue.work_type = Some(WorkType::OpenCompetition);
    alice.create_issue(&issue).await.unwrap();

    assert!(alice.claim_issue("bd-a", Duration::hours(1)).await.is_ok());
    assert!(
        bob.claim_issue("bd-a", Duration::hours(1)).await.is_ok(),
        "open-competition work is not fenced"
    );
    assert_eq!(
        alice.get_issue("bd-a").await.unwrap().unwrap().work_type,
        Some(WorkType::OpenCompetition)
    );
}

/// An agent that dies mid-task must not hold its work hostage.
#[tokio::test]
async fn an_expired_lease_returns_the_issue_to_ready() {
    let (dir, alice) = ws().await;
    let bob = as_other(&dir, "bob").await;
    mk(&*alice, "bd-a").await;

    alice
        .claim_issue("bd-a", Duration::seconds(-1))
        .await
        .unwrap();
    assert!(ready_ids(&*alice).await.contains(&"bd-a".to_string()));

    // A lapsed lease is not a claim: bob may simply take it.
    assert!(bob.claim_issue("bd-a", Duration::seconds(-1)).await.is_ok());

    let freed = alice.expire_claims().await.unwrap();
    assert_eq!(freed, vec!["bd-a"]);

    let after = alice.get_issue("bd-a").await.unwrap().unwrap();
    assert_eq!(after.status, Status::Open);
    assert_eq!(after.assignee, "");
    assert!(after.lease_expires_at.is_none());

    assert!(alice.expire_claims().await.unwrap().is_empty());
}

#[tokio::test]
async fn renewing_somebody_elses_claim_is_refused() {
    let (dir, alice) = ws().await;
    let bob = as_other(&dir, "bob").await;
    mk(&*alice, "bd-a").await;
    alice
        .claim_issue("bd-a", Duration::hours(1))
        .await
        .unwrap();

    assert!(alice.renew_claim("bd-a", Duration::hours(2)).await.is_ok());
    assert!(matches!(
        bob.renew_claim("bd-a", Duration::hours(2)).await,
        Err(Error::AlreadyClaimed { .. })
    ));
}

/// Renewing a claim nobody holds is not a *conflict*, and calling it one is how
/// `bd heartbeat` on an unclaimed bead came to report "already claimed by ''" —
/// which reads as a race with a nameless agent and sends you looking for it.
#[tokio::test]
async fn renewing_a_claim_nobody_holds_says_nobody_holds_it() {
    let (_d, s) = ws().await;
    mk(&*s, "bd-a").await;

    match s.renew_claim("bd-a", Duration::hours(1)).await {
        Err(Error::NotClaimed(id)) => assert_eq!(id, "bd-a"),
        Err(Error::AlreadyClaimed { holder, .. }) => {
            panic!("an unclaimed issue was reported as claimed by {holder:?}")
        }
        other => panic!("expected NotClaimed, got {other:?}"),
    }

    // A missing issue is still a missing issue, not an unclaimed one.
    assert!(matches!(
        s.renew_claim("bd-ghost", Duration::hours(1)).await,
        Err(Error::NotFound(_))
    ));
}

/// **`bd ready` means claimable.**
///
/// An issue another agent took five minutes ago is not claimable — `claim_issue`
/// will fence a second agent out of it — so listing it hands two agents the same
/// bead and lets one of them discover the collision after it has started work.
///
/// The other half is what keeps leases honest: once the lease *lapses*, the work
/// must come back on its own. That is the whole reason a claim is a lease and not
/// a lock, and it is what makes a dead agent's work recoverable without anyone
/// noticing it died.
#[tokio::test]
async fn ready_hides_work_that_is_held_and_returns_it_when_the_lease_lapses() {
    let (dir, alice) = ws().await;
    let bob = as_other(&dir, "bob").await;

    mk(&*alice, "bd-free").await;
    mk(&*alice, "bd-held").await;
    mk(&*alice, "bd-lapsed").await;

    alice.claim_issue("bd-held", Duration::hours(1)).await.unwrap();
    // A lease that has already expired by the time anyone reads it: a dead agent,
    // without the wait.
    alice
        .claim_issue("bd-lapsed", Duration::seconds(-1))
        .await
        .unwrap();

    let mut ids = ready_ids(&*bob).await;
    ids.sort();
    assert_eq!(
        ids,
        vec!["bd-free", "bd-lapsed"],
        "a live lease is a claim and must keep the bead out of `bd ready`; \
         a lapsed one is not and must not"
    );

    // The count has to agree with the list. `bd status` and `bd prime` read the
    // count, and a board that says "2 ready" over a list of one is worse than
    // either number alone.
    assert_eq!(bob.stats().await.unwrap().ready, 2);

    // The holder does not get a private view of it either: finding your own
    // in-flight work is `bd prime`'s job, not `bd ready`'s.
    assert!(!ready_ids(&*alice).await.contains(&"bd-held".to_string()));

    // And releasing it puts it straight back on offer.
    alice.release_claim("bd-held").await.unwrap();
    let mut ids = ready_ids(&*bob).await;
    ids.sort();
    assert_eq!(ids, vec!["bd-free", "bd-held", "bd-lapsed"]);
}

/// A claimed bead that a new edge has since gated must still show up in
/// `bd blocked`. The lease term belongs to the *ready* side only: hiding held
/// work from `bd blocked` too would make the gate invisible to everybody,
/// including the agent holding it.
#[tokio::test]
async fn blocked_work_still_shows_a_bead_somebody_is_holding() {
    let (_d, s) = ws().await;
    mk(&*s, "bd-a").await;
    mk(&*s, "bd-b").await;

    s.claim_issue("bd-b", Duration::hours(1)).await.unwrap();
    dep(&*s, "bd-b", "bd-a", DependencyType::Blocks).await;

    assert_eq!(blocked_ids(&*s).await, vec!["bd-b"]);
    assert!(!ready_ids(&*s).await.contains(&"bd-b".to_string()));
}

// ---------------------------------------------------------------------------
// Graph integrity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_cycle_is_refused_before_it_is_written() {
    let (_d, s) = ws().await;
    for id in ["bd-a", "bd-b", "bd-c"] {
        mk(&*s, id).await;
    }

    dep(&*s, "bd-a", "bd-b", DependencyType::Blocks).await;
    dep(&*s, "bd-b", "bd-c", DependencyType::Blocks).await;

    let closing = Dependency::new("bd-c", "bd-a", DependencyType::Blocks).unwrap();
    match s.add_dependency(&closing).await {
        Err(Error::Cycle(path)) => {
            assert_eq!(path.first().unwrap(), "bd-c");
            assert_eq!(path.last().unwrap(), "bd-c");
        }
        other => panic!("cycle accepted: {other:?}"),
    }

    // A cycle is a cycle whatever mix of ordering edges closes it: bd-a already
    // blocks-depends on bd-b, so making bd-b a *child* of bd-a would close the
    // loop just as surely.
    let mixed = Dependency::new("bd-b", "bd-a", DependencyType::ParentChild).unwrap();
    assert!(matches!(s.add_dependency(&mixed).await, Err(Error::Cycle(_))));

    // Pure parent-child loops too.
    mk(&*s, "bd-p").await;
    mk(&*s, "bd-q").await;
    dep(&*s, "bd-q", "bd-p", DependencyType::ParentChild).await;
    let loopy = Dependency::new("bd-p", "bd-q", DependencyType::ParentChild).unwrap();
    assert!(matches!(s.add_dependency(&loopy).await, Err(Error::Cycle(_))));

    // Nothing got written: the store is still a DAG.
    assert!(s.find_cycles().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_self_edge_is_refused() {
    let (_d, s) = ws().await;
    mk(&*s, "bd-a").await;
    mk(&*s, "bd-b").await;

    let mut d = Dependency::new("bd-a", "bd-b", DependencyType::Blocks).unwrap();
    d.depends_on_id = "bd-a".into();
    assert!(matches!(
        s.add_dependency(&d).await,
        Err(Error::Domain(bd_core::Error::SelfDependency(_)))
    ));
}

#[tokio::test]
async fn an_edge_to_a_missing_bead_is_refused() {
    let (_d, s) = ws().await;
    mk(&*s, "bd-a").await;
    let d = Dependency::new("bd-a", "bd-ghost", DependencyType::Blocks).unwrap();
    assert!(matches!(s.add_dependency(&d).await, Err(Error::NotFound(_))));
}

#[tokio::test]
async fn association_edges_never_gate_work() {
    let (_d, s) = ws().await;
    mk(&*s, "bd-a").await;
    mk(&*s, "bd-b").await;
    dep(&*s, "bd-b", "bd-a", DependencyType::Related).await;
    dep(&*s, "bd-b", "bd-a", DependencyType::DiscoveredFrom).await;

    assert_eq!(blocked_ids(&*s).await, Vec::<String>::new());
    assert_eq!(s.dependents_of("bd-a").await.unwrap().len(), 2);
}

// ---------------------------------------------------------------------------
// Ready work
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ready_ordering_follows_the_sort_policy() {
    let (_d, s) = ws().await;
    let now = Utc::now();

    let make = |id: &str, p: i32, age: Duration| {
        let mut i = Issue::new(id, id);
        i.priority = Priority(p);
        i.created_at = now - age;
        i.updated_at = i.created_at;
        i
    };
    for i in [
        make("bd-r0", 0, Duration::hours(1)),
        make("bd-r3", 3, Duration::hours(2)),
        make("bd-o0", 0, Duration::days(10)),
        make("bd-o4", 4, Duration::days(20)),
    ] {
        s.create_issue(&i).await.unwrap();
    }

    // Hybrid: anything from the last 48h ranks by priority and comes first;
    // everything older ranks by age. Keeps a P0 filed this morning visible
    // without letting a year-old P3 starve.
    assert_eq!(
        ready_order(&*s, SortPolicy::Hybrid).await,
        vec!["bd-r0", "bd-r3", "bd-o4", "bd-o0"]
    );
    assert_eq!(
        ready_order(&*s, SortPolicy::Priority).await,
        vec!["bd-o0", "bd-r0", "bd-r3", "bd-o4"]
    );
    assert_eq!(
        ready_order(&*s, SortPolicy::Oldest).await,
        vec!["bd-o4", "bd-o0", "bd-r3", "bd-r0"]
    );
}

#[tokio::test]
async fn ready_hides_deferred_infra_ephemeral_and_pinned_beads() {
    let (_d, s) = ws().await;

    mk(&*s, "bd-ok").await;

    let mut deferred = Issue::new("bd-later", "later");
    deferred.defer_until = Some(Utc::now() + Duration::hours(1));
    s.create_issue(&deferred).await.unwrap();

    let mut past = Issue::new("bd-due", "due");
    past.defer_until = Some(Utc::now() - Duration::hours(1));
    s.create_issue(&past).await.unwrap();

    let mut gate = Issue::new("bd-gate", "gate");
    gate.issue_type = bd_core::IssueType::Gate;
    s.create_issue(&gate).await.unwrap();

    let mut wisp = Issue::new("bd-wisp", "wisp");
    wisp.ephemeral = true;
    s.create_issue(&wisp).await.unwrap();

    let mut pinned = Issue::new("bd-pin", "pin");
    pinned.pinned = true;
    s.create_issue(&pinned).await.unwrap();

    let mut ids = ready_ids(&*s).await;
    ids.sort();
    assert_eq!(ids, vec!["bd-due", "bd-ok"]);
}

#[tokio::test]
async fn a_caller_filter_can_only_narrow_ready_work() {
    let (_d, s) = ws().await;
    mk(&*s, "bd-a").await;
    mk(&*s, "bd-b").await;
    dep(&*s, "bd-b", "bd-a", DependencyType::Blocks).await;

    // Asking for blocked work through `ready_work` gets you nothing extra: the
    // is_blocked = 0 term is not the caller's to switch off.
    let sneaky = IssueFilter {
        is_blocked: Some(true),
        ..IssueFilter::new()
    };
    assert_eq!(
        s.ready_work(&sneaky)
            .await
            .unwrap()
            .into_iter()
            .map(|i| i.id)
            .collect::<Vec<_>>(),
        vec!["bd-a"]
    );

    let by_label = IssueFilter {
        labels_all: vec!["nope".into()],
        ..IssueFilter::ready()
    };
    assert!(s.ready_work(&by_label).await.unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Filters, config, stats, events
// ---------------------------------------------------------------------------

#[tokio::test]
async fn filters_push_down_into_sql() {
    let (_d, s) = ws().await;

    let mut a = Issue::new("bd-a", "needle in a haystack");
    a.priority = Priority::CRITICAL;
    a.labels = vec!["x".into()];
    a.spec_id = "spec-1".into();
    a.metadata = Some(serde_json::json!({"k": 1}));
    s.create_issue(&a).await.unwrap();

    let mut b = Issue::new("bd-b", "something else");
    b.priority = Priority::LOW;
    b.assignee = "bob".into();
    s.create_issue(&b).await.unwrap();

    mk(&*s, "bd-c").await;
    dep(&*s, "bd-c", "bd-a", DependencyType::ParentChild).await;
    mk(&*s, "bd-d").await;
    dep(&*s, "bd-d", "bd-c", DependencyType::ParentChild).await;

    assert_eq!(
        list_ids(
            &*s,
            IssueFilter {
                text: Some("needle".into()),
                ..Default::default()
            }
        )
        .await,
        vec!["bd-a"]
    );
    assert_eq!(
        list_ids(
            &*s,
            IssueFilter {
                labels_any: vec!["x".into()],
                ..Default::default()
            }
        )
        .await,
        vec!["bd-a"]
    );
    assert_eq!(
        list_ids(
            &*s,
            IssueFilter {
                assignee: Some("bob".into()),
                ..Default::default()
            }
        )
        .await,
        vec!["bd-b"]
    );
    assert_eq!(
        list_ids(
            &*s,
            IssueFilter {
                spec_id: Some("spec-1".into()),
                ..Default::default()
            }
        )
        .await,
        vec!["bd-a"]
    );
    assert_eq!(
        list_ids(
            &*s,
            IssueFilter {
                has_metadata_key: Some("k".into()),
                ..Default::default()
            }
        )
        .await,
        vec!["bd-a"]
    );
    // "at least P1" means P0 and P1, because P0 is the *most* important.
    assert_eq!(
        list_ids(
            &*s,
            IssueFilter {
                min_priority: Some(Priority::HIGH),
                ..Default::default()
            }
        )
        .await,
        vec!["bd-a"]
    );
    // --parent is transitive: bd-d is a grandchild, and reporting only bd-c
    // would quietly undercount an epic.
    assert_eq!(
        list_ids(
            &*s,
            IssueFilter {
                parent: Some("bd-a".into()),
                ..Default::default()
            }
        )
        .await,
        vec!["bd-c", "bd-d"]
    );

    assert_eq!(s.count_issues(&IssueFilter::new()).await.unwrap(), 4);
    assert_eq!(
        s.list_issues(&IssueFilter::new().with_limit(2))
            .await
            .unwrap()
            .len(),
        2
    );
}

/// The negative and bounded filters. Each one is a conjunct that `bd-query`
/// relies on being pushed down; a backend that quietly ignored one would turn a
/// pushdown into a wrong answer rather than a slow one.
#[tokio::test]
async fn negative_and_bounded_filters_push_down_too() {
    let (_d, s) = ws().await;

    let mut p0 = Issue::new("bd-p0", "critical");
    p0.priority = Priority::CRITICAL;
    s.create_issue(&p0).await.unwrap();

    let mut p4 = Issue::new("bd-p4", "trivial");
    p4.priority = Priority::TRIVIAL;
    s.create_issue(&p4).await.unwrap();

    let mut gate = Issue::new("bd-gate", "gate");
    gate.issue_type = bd_core::IssueType::Gate;
    s.create_issue(&gate).await.unwrap();

    s.close_issue("bd-p4", "done").await.unwrap();

    // `NOT status=closed`
    assert_eq!(
        list_ids(
            &*s,
            IssueFilter {
                exclude_statuses: vec![Status::Closed],
                ..Default::default()
            }
        )
        .await,
        vec!["bd-gate", "bd-p0"]
    );

    // "at most this important" is a numeric >=, because P0 is the most urgent.
    assert_eq!(
        list_ids(
            &*s,
            IssueFilter {
                max_priority: Some(Priority::LOW),
                ..Default::default()
            }
        )
        .await,
        vec!["bd-p4"]
    );

    assert_eq!(
        list_ids(
            &*s,
            IssueFilter {
                exclude_types: vec![bd_core::IssueType::Gate],
                ..Default::default()
            }
        )
        .await,
        vec!["bd-p0", "bd-p4"]
    );

    // A bound outside P0-P4 is a legal query with no answers, and the database
    // gets to say so without a scan.
    assert!(
        list_ids(
            &*s,
            IssueFilter {
                min_priority: Some(Priority(-1)),
                ..Default::default()
            }
        )
        .await
        .is_empty()
    );
}

#[tokio::test]
async fn every_mutation_leaves_an_event_behind() {
    let (_d, s) = ws().await;
    mk(&*s, "bd-a").await;
    mk(&*s, "bd-b").await;

    s.add_label("bd-a", "urgent").await.unwrap();
    s.remove_label("bd-a", "urgent").await.unwrap();
    s.add_comment("bd-a", "hi").await.unwrap();
    dep(&*s, "bd-a", "bd-b", DependencyType::Blocks).await;
    s.remove_dependency("bd-a", "bd-b", &DependencyType::Blocks)
        .await
        .unwrap();
    s.close_issue("bd-a", "done").await.unwrap();
    s.reopen_issue("bd-a").await.unwrap();

    use bd_core::EventType::*;
    let kinds: Vec<_> = s
        .list_events("bd-a")
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.event_type)
        .collect();
    assert_eq!(
        kinds,
        vec![
            Created,
            LabelAdded,
            LabelRemoved,
            Commented,
            DependencyAdded,
            DependencyRemoved,
            StatusChanged,
            Closed,
            StatusChanged,
            Reopened,
        ]
    );

    let events = s.list_events("bd-a").await.unwrap();
    assert!(events.iter().all(|e| e.actor == "alice"));
}

#[tokio::test]
async fn stats_count_what_an_agent_cares_about() {
    let (_d, s) = ws().await;
    for id in ["bd-a", "bd-b", "bd-c"] {
        mk(&*s, id).await;
    }
    dep(&*s, "bd-b", "bd-a", DependencyType::Blocks).await;
    s.claim_issue("bd-c", Duration::hours(1)).await.unwrap();

    let st = s.stats().await.unwrap();
    assert_eq!(st.total, 3);
    assert_eq!(st.open, 2);
    assert_eq!(st.in_progress, 1);
    assert_eq!(st.closed, 0);
    assert_eq!(st.blocked, 1); // bd-b
    // bd-a, and only bd-a: bd-b is gated by the graph, and bd-c is held under a
    // live lease, which is not claimable work however much it looks like it.
    assert_eq!(st.ready, 1);
    assert_eq!(st.by_priority.get(&2), Some(&3));
    assert_eq!(st.by_type.get("task"), Some(&3));
}

// ---------------------------------------------------------------------------
// Derived state: the content hash
// ---------------------------------------------------------------------------

/// `content_hash` is the answer to "are these two beads the same bead" across
/// clones, and it is what an import dedups on. A hash computed once at create and
/// never refreshed describes content the issue no longer has — so an edited bead
/// stops matching itself, and keeps matching whatever it used to say.
///
/// Every write below moves something the hash covers. `compute_content_hash` in
/// bd-core is the authority on what that is; this test asks the store to agree
/// with it after each one.
#[tokio::test]
async fn content_hash_follows_the_content_through_every_write() {
    let (_d, s) = ws().await;

    #[track_caller]
    fn agrees(i: &Issue, after: &str) {
        assert_eq!(
            i.content_hash,
            i.compute_content_hash(),
            "the stored content_hash describes content the issue no longer has, after {after}"
        );
    }

    let created = s.create_issue(&Issue::new("bd-a", "before")).await.unwrap();
    agrees(&created, "create");
    let first = created.content_hash.clone();

    let renamed = s
        .update_issue(
            "bd-a",
            &IssuePatch {
                title: Some("after".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    agrees(&renamed, "update");
    assert_ne!(renamed.content_hash, first, "the content changed; the hash did not");

    // Labels are hashed too — `compute_content_hash` sorts and folds them in —
    // so they are a write path of their own.
    s.add_label("bd-a", "infra").await.unwrap();
    agrees(&s.get_issue("bd-a").await.unwrap().unwrap(), "add_label");
    s.remove_label("bd-a", "infra").await.unwrap();
    agrees(&s.get_issue("bd-a").await.unwrap().unwrap(), "remove_label");

    // A claim moves the assignee and the status.
    s.claim_issue("bd-a", Duration::hours(1)).await.unwrap();
    agrees(&s.get_issue("bd-a").await.unwrap().unwrap(), "claim");
    s.release_claim("bd-a").await.unwrap();
    agrees(&s.get_issue("bd-a").await.unwrap().unwrap(), "release");

    let closed = s.close_issue("bd-a", "done").await.unwrap();
    agrees(&closed, "close");
    let reopened = s.reopen_issue("bd-a").await.unwrap();
    agrees(&reopened, "reopen");

    // The round trip settles: the same content hashes the same way it did before
    // any of this happened.
    assert_eq!(
        reopened.content_hash,
        Issue {
            title: "after".into(),
            ..Issue::new("bd-a", "after")
        }
        .compute_content_hash()
    );
}

// ---------------------------------------------------------------------------
// Comments: ids have to mean the same thing in two workspaces
// ---------------------------------------------------------------------------

/// **Silent corruption.** `upsert_comment` treats the incoming id as the
/// comment's identity — that is what makes `bd import` idempotent. A
/// workspace-local integer id is not an identity: two workspaces that have each
/// ever written a comment both hold a comment `1`, so importing A's export into B
/// overwrites B's comment with A's text *and* re-parents it onto A's issue.
///
/// Nothing errors. B's comment is simply gone, and the one wearing its id belongs
/// to someone else.
#[tokio::test]
async fn a_comment_from_another_workspace_cannot_overwrite_a_local_one() {
    let (_da, a) = workspace("alice").await;
    let (_db, b) = workspace("bob").await;

    // Two independent workspaces, each with its own first comment.
    mk(&*a, "a-1").await;
    let from_a = a.add_comment("a-1", "alice said this").await.unwrap();

    mk(&*b, "b-1").await;
    let from_b = b.add_comment("b-1", "bob said this").await.unwrap();

    assert_ne!(
        from_a.id, from_b.id,
        "two workspaces minted the same comment id; one of them is about to lose a comment"
    );

    // Now import A into B, exactly as `bd import` does: the issue, then the
    // comment, keyed on its own id.
    b.create_issue(&a.get_issue("a-1").await.unwrap().unwrap())
        .await
        .unwrap();
    b.upsert_comment(&from_a).await.unwrap();

    // B's own comment must be untouched, and still attached to B's own issue.
    let bs = b.list_comments("b-1").await.unwrap();
    assert_eq!(bs.len(), 1, "b-1 lost its comment to the import: {bs:?}");
    assert_eq!(bs[0].id, from_b.id);
    assert_eq!(bs[0].text, "bob said this");
    assert_eq!(bs[0].author, "bob");

    // And A's comment landed where it belongs, with its author intact.
    let as_ = b.list_comments("a-1").await.unwrap();
    assert_eq!(as_.len(), 1);
    assert_eq!(as_[0].text, "alice said this");
    assert_eq!(as_[0].author, "alice");

    // Re-importing is a no-op, which is the property the id-as-identity buys.
    b.upsert_comment(&from_a).await.unwrap();
    assert_eq!(b.list_comments("a-1").await.unwrap().len(), 1);
}

/// A thread is ordered by *time*. Comment ids are UUIDs now, so an ORDER BY id
/// would shuffle the conversation — and a shuffled conversation reads as a
/// different one.
#[tokio::test]
async fn comments_come_back_in_the_order_they_were_written() {
    let (_d, s) = ws().await;
    mk(&*s, "bd-a").await;

    for text in ["first", "second", "third", "fourth", "fifth"] {
        s.add_comment("bd-a", text).await.unwrap();
    }

    let texts: Vec<String> = s
        .list_comments("bd-a")
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.text)
        .collect();
    assert_eq!(texts, ["first", "second", "third", "fourth", "fifth"]);
}

// ---------------------------------------------------------------------------
// The batched seams
// ---------------------------------------------------------------------------

/// Without these, `children`, `epic`, `orphans`, `lint` and `export` are all
/// N+1 — and the only alternative on the old seam was to scan the whole
/// workspace to answer a question about four beads.
#[tokio::test]
async fn the_batched_getters_answer_in_one_query_and_in_the_callers_order() {
    let (_d, s) = ws().await;
    for id in ["bd-a", "bd-b", "bd-c"] {
        mk(&*s, id).await;
    }
    dep(&*s, "bd-a", "bd-b", DependencyType::Blocks).await;
    dep(&*s, "bd-a", "bd-c", DependencyType::Related).await;
    dep(&*s, "bd-b", "bd-c", DependencyType::Blocks).await;
    s.add_comment("bd-a", "one").await.unwrap();
    s.add_comment("bd-a", "two").await.unwrap();
    s.add_comment("bd-b", "three").await.unwrap();

    // get_issues: the caller's order, and a missing id is a gap rather than an
    // error — asking about a bead somebody deleted is ordinary.
    let got = s
        .get_issues(&["bd-c".into(), "bd-a".into(), "bd-gone".into()])
        .await
        .unwrap();
    assert_eq!(
        got.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
        ["bd-c", "bd-a"]
    );
    assert!(s.get_issues(&[]).await.unwrap().is_empty());

    // list_dependencies: the whole edge set, in one query.
    let all = s.list_dependencies(&IssueFilter::new()).await.unwrap();
    assert_eq!(all.len(), 3);

    // …or the edges out of the issues a filter selects.
    let f = IssueFilter {
        text: Some("issue bd-a".into()),
        ..IssueFilter::new()
    };
    let out_of_a = s.list_dependencies(&f).await.unwrap();
    assert_eq!(out_of_a.len(), 2);
    assert!(out_of_a.iter().all(|d| d.issue_id == "bd-a"));

    // dependencies_of_many / comments_of_many: keyed, and ids with nothing are
    // simply absent rather than present-and-empty.
    let deps = s
        .dependencies_of_many(&["bd-a".into(), "bd-c".into()])
        .await
        .unwrap();
    assert_eq!(deps.len(), 1, "bd-c has no out-edges: {deps:?}");
    assert_eq!(deps[0].0, "bd-a");
    assert_eq!(deps[0].1.len(), 2);

    let comments = s
        .comments_of_many(&["bd-a".into(), "bd-b".into(), "bd-c".into()])
        .await
        .unwrap();
    assert_eq!(comments.len(), 2);
    assert_eq!(comments[0].0, "bd-a");
    assert_eq!(
        comments[0].1.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
        ["one", "two"],
        "a batched read must keep a thread in order too"
    );
    assert!(s.comments_of_many(&[]).await.unwrap().is_empty());
}

/// The two sort policies whose whole reason for existing is that their consumers
/// were sorting in memory — which is only ever safe until somebody adds a
/// `--limit`, at which point the database pages one order and the answer is
/// re-sorted into another.
#[tokio::test]
async fn updated_and_closed_sort_in_sql_so_a_limit_pages_the_right_rows() {
    let (_d, s) = ws().await;
    let now = Utc::now();

    // Created newest-first, so that a sort by *creation* would produce the exact
    // opposite order and this test could not pass by accident.
    for (n, id) in ["bd-1", "bd-2", "bd-3"].iter().enumerate() {
        let mut i = Issue::new(*id, *id);
        i.created_at = now - Duration::days(n as i64);
        i.updated_at = i.created_at;
        s.create_issue(&i).await.unwrap();
    }
    // Touch bd-3, so it is the *most* recently updated despite being the oldest.
    s.update_issue(
        "bd-3",
        &IssuePatch {
            title: Some("touched".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let ids = |v: Vec<Issue>| -> Vec<String> { v.into_iter().map(|i| i.id).collect() };

    // Least-recently-touched first: that is the question `bd stale` asks.
    let stale = s
        .list_issues(&IssueFilter {
            sort: SortPolicy::Updated,
            ..IssueFilter::new()
        })
        .await
        .unwrap();
    assert_eq!(ids(stale), ["bd-2", "bd-1", "bd-3"]);

    // And with a LIMIT the database must hand back the *first page of that
    // order*, not the first page of some other order re-sorted afterwards.
    let page = s
        .list_issues(&IssueFilter {
            sort: SortPolicy::Updated,
            limit: Some(1),
            ..IssueFilter::new()
        })
        .await
        .unwrap();
    assert_eq!(ids(page), ["bd-2"]);

    s.close_issue("bd-1", "done").await.unwrap();
    s.close_issue("bd-2", "done").await.unwrap();

    // Most-recently-closed first — the opposite direction, because it answers the
    // opposite question. Open issues have no close time and fall to the end.
    let closed = s
        .list_issues(&IssueFilter {
            sort: SortPolicy::Closed,
            ..IssueFilter::new()
        })
        .await
        .unwrap();
    assert_eq!(ids(closed), ["bd-2", "bd-1", "bd-3"]);
}

/// `bd promote`: a wisp becomes a real bead.
///
/// Both fields have to move. An ephemeral bead is hidden from `bd ready`, and a
/// bead that still declares a `wisp_type` still has that type's TTL — so clearing
/// only one of them yields work that is either invisible or that `bd gc` deletes
/// out from under whoever claimed it.
#[tokio::test]
async fn promoting_a_wisp_clears_both_the_flag_and_the_ttl() {
    let (_d, s) = ws().await;

    let mut wisp = Issue::new("bd-w", "a recovery wisp");
    wisp.ephemeral = true;
    wisp.wisp_type = Some(bd_core::WispType::Recovery);
    s.create_issue(&wisp).await.unwrap();

    assert!(
        !ready_ids(&*s).await.contains(&"bd-w".to_string()),
        "an ephemeral bead is never claimable work"
    );

    let promoted = s.update_issue("bd-w", &IssuePatch::promote()).await.unwrap();
    assert!(!promoted.ephemeral);
    assert_eq!(
        promoted.wisp_type, None,
        "a promoted bead that kept its wisp type still has that type's TTL, and gc will reap it"
    );
    assert_eq!(ready_ids(&*s).await, vec!["bd-w"]);
}

#[tokio::test]
async fn config_round_trips() {
    let (_d, s) = ws().await;
    s.set_config("a", "1").await.unwrap();
    s.set_config("a", "2").await.unwrap();
    assert_eq!(s.get_config("a").await.unwrap().as_deref(), Some("2"));
    assert!(s.get_config("missing").await.unwrap().is_none());
    assert!(s.list_config().await.unwrap().contains(&(
        crate::PREFIX_KEY.to_string(),
        "bd".to_string()
    )));
}

#[tokio::test]
async fn next_id_never_collides_with_a_stored_id() {
    let (_d, s) = ws().await;

    let first = s.next_id("bd", "title", "desc").await.unwrap();
    assert!(first.starts_with("bd-"));
    s.create_issue(&Issue::new(&first, "title")).await.unwrap();

    let second = s.next_id("bd", "title", "desc").await.unwrap();
    assert_ne!(first, second);
    assert!(s.get_issue(&second).await.unwrap().is_none());
}

#[tokio::test]
async fn update_applies_only_the_fields_that_were_set() {
    let (_d, s) = ws().await;
    let mut i = Issue::new("bd-a", "before");
    i.description = "keep me".into();
    s.create_issue(&i).await.unwrap();

    let updated = s
        .update_issue(
            "bd-a",
            &IssuePatch {
                title: Some("after".into()),
                priority: Some(Priority::CRITICAL),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(updated.title, "after");
    assert_eq!(updated.priority, Priority::CRITICAL);
    assert_eq!(updated.description, "keep me");
    assert!(updated.updated_at >= updated.created_at);

    assert!(matches!(
        s.update_issue("bd-ghost", &IssuePatch::default()).await,
        Err(Error::NotFound(_))
    ));
}

// ---------------------------------------------------------------------------
// Schema version
// ---------------------------------------------------------------------------

/// `bd init` stamps `PRAGMA user_version` with the current schema version, so
/// every database this port creates is self-describing from birth. This is the
/// insurance bought before the first schema change exists — upstream ships
/// schema changes with no recorded version, and every upgrade there is a
/// coordinated, manual event across machines.
#[tokio::test]
async fn init_stamps_the_schema_version() {
    let (_d, s) = ws().await;
    assert_eq!(
        s.schema_version().await.unwrap(),
        bd_storage::SCHEMA_VERSION,
        "a fresh workspace must carry the version it was written at"
    );
}

/// `migrate` on a current database is a no-op that says so; on a
/// pre-versioning database (raw stamp 0 — what every 0.1.0 workspace looks
/// like) it stamps and reports the transition. Both exit successfully: an
/// agent may run it unconditionally after upgrading bd.
#[tokio::test]
async fn migrate_stamps_a_preversioning_database_and_is_idempotent() {
    let (dir, s) = ws().await;

    let done = s.migrate().await.unwrap();
    assert_eq!(done.from, bd_storage::SCHEMA_VERSION, "already stamped");
    assert_eq!(done.to, bd_storage::SCHEMA_VERSION);

    // Rewind the stamp to what a database from before version stamping holds.
    s.close().await.unwrap();
    let beads = dir.0.join(".beads");
    let loc = Locator::load(&beads).unwrap();
    let pool = crate::connect(&loc.db_path()).await.unwrap();
    sqlx::raw_sql("PRAGMA user_version = 0")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let s = as_other(&dir, "bob").await;
    assert_eq!(s.schema_version().await.unwrap(), 0, "raw stamp, honestly");

    let done = s.migrate().await.unwrap();
    assert_eq!((done.from, done.to), (0, bd_storage::SCHEMA_VERSION));
    assert_eq!(s.schema_version().await.unwrap(), bd_storage::SCHEMA_VERSION);
}

/// A stamp *newer* than this build is refused — migrating downward is data
/// loss wearing a maintenance command's name, and the error says what the real
/// fix is (upgrading bd).
#[tokio::test]
async fn migrate_refuses_to_downgrade() {
    let (dir, s) = ws().await;
    s.close().await.unwrap();

    let beads = dir.0.join(".beads");
    let loc = Locator::load(&beads).unwrap();
    let pool = crate::connect(&loc.db_path()).await.unwrap();
    sqlx::raw_sql("PRAGMA user_version = 99")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let s = as_other(&dir, "bob").await;
    assert_eq!(s.schema_version().await.unwrap(), 99);
    let err = s.migrate().await.unwrap_err().to_string();
    assert!(err.contains("newer"), "the refusal explains itself: {err}");
    assert!(
        err.contains("Upgrade bd"),
        "the refusal points at the real fix: {err}"
    );
    assert_eq!(
        s.schema_version().await.unwrap(),
        99,
        "a refused migrate must not have touched the stamp"
    );
}
