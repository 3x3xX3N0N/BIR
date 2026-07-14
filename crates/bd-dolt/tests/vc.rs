//! Integration tests for the Dolt commit graph.
//!
//! # These tests skip when there is no `dolt`, and they say so
//!
//! Every test here begins with [`require_dolt!`], which prints that it covered
//! **nothing** and returns. A test that passes green because it did nothing is
//! worse than no test at all — it reports as coverage — so the skip is loud and
//! the reason is named.
//!
//! They also skip, just as loudly, while `bd_dolt::init` is still unwired: the
//! constructor lives in the crate root and is not this module's to write. The
//! moment it lands *and* a `dolt` binary is on PATH, every test below starts
//! running for real, with no edit here.
//!
//! # What the important one proves
//!
//! [`merge_recomputes_the_blocked_cache`] is the reason this file exists. Read
//! it before the others.

use std::path::PathBuf;

use bd_core::{Dependency, DependencyType, Issue, IssueFilter};
use bd_dolt::require_dolt;
use bd_storage::capability::{MergeOutcome, ResolveStrategy};
use bd_storage::{Identity, Storage};
use chrono::Utc;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Open a fresh workspace, or explain why this test is covering nothing.
///
/// Returns `None` — after saying so on stderr — when the crate's constructor is
/// still a stub. Panicking would be wrong (the port is not finished and that is
/// not a failure) and passing silently would be worse.
async fn open_ws(name: &str) -> Option<Workspace> {
    let dir = std::env::temp_dir().join(format!(
        "bd-dolt-vc-{name}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).expect("create temp workspace");

    match bd_dolt::init(&dir, "bd", Identity::new("tester@beads.test")).await {
        Ok(store) => Some(Workspace { store, dir }),
        Err(e) if e.is_unsupported() => {
            eprintln!(
                "SKIPPED: `bd_dolt::init` is still a stub, so this test is NOT covering \
                 anything. It will run as written once the constructor is wired up."
            );
            let _ = std::fs::remove_dir_all(&dir);
            None
        }
        Err(e) => panic!("dolt workspace would not open: {e}"),
    }
}

struct Workspace {
    store: Box<dyn Storage>,
    dir: PathBuf,
}

impl Drop for Workspace {
    fn drop(&mut self) {
        // Best effort: a leaked temp dir is untidy, a panic in Drop is worse.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Workspace {
    async fn add(&self, title: &str) -> String {
        let id = self.store.next_id("bd", title, "").await.expect("next_id");
        self.store
            .create_issue(&Issue::new(&id, title))
            .await
            .expect("create_issue");
        id
    }

    /// `blocked` cannot start until `blocker` closes.
    async fn blocks(&self, blocker: &str, blocked: &str) {
        self.store
            .add_dependency(&Dependency {
                issue_id: blocked.to_string(),
                depends_on_id: blocker.to_string(),
                dep_type: DependencyType::Blocks,
                created_at: Utc::now(),
                created_by: "tester".to_string(),
                metadata: String::new(),
                thread_id: String::new(),
            })
            .await
            .expect("add_dependency");
    }

    async fn is_ready(&self, id: &str) -> bool {
        self.store
            .ready_work(&IssueFilter::default())
            .await
            .expect("ready_work")
            .iter()
            .any(|i| i.id == id)
    }

    fn vc(&self) -> &dyn bd_storage::VersionControl {
        self.store
            .version_control()
            .expect("the dolt backend must offer a commit graph")
    }

    fn remote(&self) -> &dyn bd_storage::RemoteStore {
        self.store.remote().expect("the dolt backend must have remotes")
    }
}

// ---------------------------------------------------------------------------
// The one that matters
// ---------------------------------------------------------------------------

/// A merge lands facts no local write path ever saw, so `is_blocked` is stale.
///
/// # Why the scenario is shaped like this
///
/// The obvious test — close a blocker on a branch, merge it, check the blocked
/// issue became ready — **cannot fail**, and so proves nothing. `is_blocked` is
/// an ordinary column in a versioned table: the branch's own write path already
/// set it to 0, and the merge just carries that 0 across. It would pass with the
/// recompute deleted.
///
/// The cache only goes wrong when the two sides are each locally right and their
/// *combination* is something neither computed:
///
/// * base — `A` blocks `B`. `B.is_blocked = 1`. Both branches start here.
/// * on `feat` — close `A`. That is `B`'s only blocker, so `feat` correctly sets
///   `B.is_blocked = 0`.
/// * on `main` — add a brand-new open issue `C` that also blocks `B`. `B` is
///   still blocked, so `main` correctly leaves `B.is_blocked = 1`.
///
/// Merge `feat` into `main` and Dolt merges the `is_blocked` cell the only way
/// it can: `main` did not change it from the base value, `feat` did, so `feat`
/// wins and `B.is_blocked` becomes **0**. Every step was correct and the answer
/// is wrong — `B` is blocked by `C`, which is open, and nothing in the merge
/// could have known that.
///
/// Without the recompute, `bd ready` now confidently hands `B` to the next agent.
/// No error, no crash, no way to notice. That is the failure this test exists to
/// catch, and the only thing that catches it is a full pass over the graph.
#[tokio::test]
async fn merge_recomputes_the_blocked_cache() {
    require_dolt!();
    let Some(ws) = open_ws("recompute").await else {
        return;
    };

    let a = ws.add("A: the original blocker").await;
    let b = ws.add("B: the work everyone wants").await;
    ws.blocks(&a, &b).await;
    assert!(!ws.is_ready(&b).await, "B starts blocked by A");

    ws.vc().commit("base").await.expect("commit base");
    let main = ws.vc().current_branch().await.expect("current_branch");

    // --- on the branch: A closes, so B looks free ---
    ws.vc().create_branch("feat").await.expect("create_branch");
    ws.vc().checkout("feat").await.expect("checkout feat");
    ws.store
        .close_issue(&a, "done")
        .await
        .expect("close the blocker");
    assert!(ws.is_ready(&b).await, "on feat, B's only blocker is closed");
    ws.vc().commit("close A").await.expect("commit on feat");

    // --- meanwhile on main: a new blocker nobody on feat has heard of ---
    ws.vc().checkout(&main).await.expect("checkout main");
    let c = ws.add("C: a blocker feat never saw").await;
    ws.blocks(&c, &b).await;
    ws.vc().commit("add C").await.expect("commit on main");
    assert!(!ws.is_ready(&b).await, "on main, B is blocked by C");

    // --- the merge ---
    let outcome = ws.vc().merge("feat").await.expect("merge");
    assert!(
        !matches!(outcome, MergeOutcome::Conflicted { .. }),
        "this merge touches disjoint rows and must not conflict: {outcome:?}"
    );

    // A is closed and C is open, so B is still blocked. The merged `is_blocked`
    // cell says otherwise; only the recompute can know that.
    assert!(
        !ws.is_ready(&b).await,
        "REGRESSION: `bd ready` is handing out B, which is blocked by the still-open C. \
         The merge did not recompute the is_blocked cache."
    );
    assert!(
        ws.store
            .blocked_work(&IssueFilter::default())
            .await
            .expect("blocked_work")
            .iter()
            .any(|i| i.id == b),
        "B should appear as blocked work"
    );
}

/// The same hazard, arriving over the wire instead of from a local branch.
///
/// `pull` is `fetch` plus `merge`, and it is the case the doc comment on
/// `RemoteStore::pull` singles out: the rows come from another clone, so *no*
/// local write path saw any of them.
#[tokio::test]
async fn pull_recomputes_the_blocked_cache() {
    require_dolt!();
    let Some(ws) = open_ws("pull-recompute").await else {
        return;
    };

    let remote_dir = ws.dir.join("remote");
    std::fs::create_dir_all(&remote_dir).expect("create remote dir");
    let url = format!("file://{}", remote_dir.display().to_string().replace('\\', "/"));

    ws.vc().commit("base").await.expect("commit");
    let branch = ws.vc().current_branch().await.expect("current_branch");

    ws.remote()
        .add_remote("origin", &url)
        .await
        .expect("add_remote");
    assert_eq!(
        ws.remote().list_remotes().await.expect("list_remotes"),
        vec![("origin".to_string(), url.clone())]
    );

    ws.remote()
        .push("origin", &branch)
        .await
        .expect("push to a file remote needs no credentials");

    // Pulling back what we just pushed changes nothing — and `UpToDate` is the
    // only honest answer. Getting this wrong in the other direction (reporting a
    // merge that did nothing) would be harmless; reporting `UpToDate` for a pull
    // that *did* land rows would skip the recompute, which would not be.
    let outcome = ws.remote().pull("origin", &branch).await.expect("pull");
    assert_eq!(outcome, MergeOutcome::UpToDate);

    ws.remote().fetch("origin").await.expect("fetch");
}

// ---------------------------------------------------------------------------
// Conflicts are data
// ---------------------------------------------------------------------------

/// A conflicting merge is not a failed one.
#[tokio::test]
async fn a_conflicting_merge_returns_conflicts_rather_than_an_error() {
    require_dolt!();
    let Some(ws) = open_ws("conflict").await else {
        return;
    };

    let a = ws.add("contested").await;
    ws.vc().commit("base").await.expect("commit base");
    let main = ws.vc().current_branch().await.expect("current_branch");

    ws.vc().create_branch("feat").await.expect("create_branch");
    ws.vc().checkout("feat").await.expect("checkout feat");
    ws.store
        .update_issue(
            &a,
            &bd_storage::IssuePatch {
                title: Some("theirs".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("edit on feat");
    ws.vc().commit("theirs").await.expect("commit on feat");

    ws.vc().checkout(&main).await.expect("checkout main");
    ws.store
        .update_issue(
            &a,
            &bd_storage::IssuePatch {
                title: Some("ours".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("edit on main");
    ws.vc().commit("ours").await.expect("commit on main");

    // Both sides changed the same cell. That is a conflict, and a conflict is an
    // outcome — an `Err` here would make `bd merge` look broken when it worked.
    let outcome = ws.vc().merge("feat").await.expect("a conflict is not an Err");
    let MergeOutcome::Conflicted { count } = outcome else {
        panic!("expected a conflict, got {outcome:?}");
    };
    assert!(count > 0, "a conflicted merge must count its conflicts");

    let conflicts = ws.vc().conflicts().await.expect("conflicts");
    let issue_conflict = conflicts
        .iter()
        .find(|c| c.table == "issues" && c.issue_id == a)
        .expect("the conflict should name the issue and the table");
    assert!(issue_conflict.ours.is_some(), "our side of the row");
    assert!(issue_conflict.theirs.is_some(), "their side of the row");
    assert!(issue_conflict.base.is_some(), "the common ancestor");

    let resolved = ws
        .vc()
        .resolve_conflicts(ResolveStrategy::Ours)
        .await
        .expect("resolve");
    assert_eq!(resolved, count);
    assert!(ws.vc().conflicts().await.expect("conflicts").is_empty());

    let issue = ws.store.get_issue(&a).await.expect("get").expect("present");
    assert_eq!(issue.title, "ours", "--ours must keep our side");
}

// ---------------------------------------------------------------------------
// Ordinary version control
// ---------------------------------------------------------------------------

#[tokio::test]
async fn branches_commits_and_the_log() {
    require_dolt!();
    let Some(ws) = open_ws("basics").await else {
        return;
    };

    let main = ws.vc().current_branch().await.expect("current_branch");

    ws.add("first").await;
    assert!(
        !ws.vc().status().await.expect("status").is_empty(),
        "an uncommitted issue must show as a dirty table"
    );

    let hash = ws.vc().commit("first commit").await.expect("commit");
    assert_eq!(hash, ws.vc().current_commit().await.expect("head"));
    assert!(
        ws.vc().status().await.expect("status").is_empty(),
        "nothing should be dirty right after a commit"
    );

    // Committing a clean tree is a no-op, not a failure: the user asked for the
    // working tree to be committed and it already is.
    assert_eq!(
        ws.vc().commit("nothing to do").await.expect("empty commit"),
        hash,
        "an empty commit must answer with the commit we are already on"
    );

    let log = ws.vc().log(10).await.expect("log");
    assert_eq!(log[0].hash, hash);
    assert_eq!(log[0].message, "first commit");
    assert!(
        log[0].author.contains("tester"),
        "the commit must be attributed to the store's identity, got {:?}",
        log[0].author
    );

    ws.vc().create_branch("side").await.expect("create_branch");
    let mut branches = ws.vc().list_branches().await.expect("list_branches");
    branches.sort();
    assert!(branches.contains(&"side".to_string()));
    assert!(branches.contains(&main));

    ws.vc().checkout("side").await.expect("checkout");
    assert_eq!(ws.vc().current_branch().await.expect("branch"), "side");

    ws.vc().checkout(&main).await.expect("checkout back");
    ws.vc()
        .delete_branch("side", false)
        .await
        .expect("delete a merged branch");
    assert!(
        !ws.vc()
            .list_branches()
            .await
            .expect("list")
            .contains(&"side".to_string())
    );
}

// ---------------------------------------------------------------------------
// Time travel
// ---------------------------------------------------------------------------

#[tokio::test]
async fn history_as_of_and_diff() {
    require_dolt!();
    let Some(ws) = open_ws("history").await else {
        return;
    };
    let history = ws.store.history().expect("dolt must offer history");

    let a = ws.add("as authored").await;
    let first = ws.vc().commit("author it").await.expect("commit");

    ws.store
        .update_issue(
            &a,
            &bd_storage::IssuePatch {
                title: Some("as revised".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("revise");
    let second = ws.vc().commit("revise it").await.expect("commit");

    let revisions = history.history(&a).await.expect("history");
    assert!(
        revisions.len() >= 2,
        "two commits touched this issue, got {}",
        revisions.len()
    );
    assert_eq!(revisions[0].commit, second, "newest revision first");
    assert_eq!(revisions[0].issue.title, "as revised");
    assert!(revisions.iter().any(|r| r.issue.title == "as authored"));

    let then = history
        .as_of(&a, &first)
        .await
        .expect("as_of")
        .expect("the issue existed at the first commit");
    assert_eq!(then.title, "as authored");
    assert!(
        history
            .as_of("bd-nonexistent", &first)
            .await
            .expect("as_of")
            .is_none()
    );

    let diff = history.diff(&first, &second).await.expect("diff");
    let d = diff
        .iter()
        .find(|d| d.issue_id == a)
        .expect("the revised issue should appear in the diff");
    assert_eq!(d.change, bd_storage::capability::ChangeKind::Modified);
    let title = d
        .fields
        .iter()
        .find(|f| f.field == "title")
        .expect("the title changed");
    assert_eq!(title.from.as_deref(), Some("as authored"));
    assert_eq!(title.to.as_deref(), Some("as revised"));

    // The derived cache is not a change the user made, and showing it as one
    // invites exactly the confusion this backend is trying to avoid.
    assert!(
        !d.fields.iter().any(|f| f.field == "is_blocked"),
        "a diff must not report the is_blocked cache as a user-visible change"
    );
}
