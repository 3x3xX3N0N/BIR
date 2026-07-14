//! Row <-> domain mapping.
//!
//! Deliberately hand-written rather than `#[derive(FromRow)]`: half the columns
//! are string-encoded enums whose spelling is a wire format shared with the Go
//! implementation, and a derive would hide that behind a silent `Into`.

use bd_core::{
    Comment, Dependency, DependencyType, Event, EventType, Issue, IssueType, Priority, Status,
};
use bd_storage::{Error, Result};
use serde::{Serialize, de::DeserializeOwned};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

/// Every issue column, in a fixed order. Shared by every SELECT so that a new
/// column cannot be added to one query and forgotten in another.
pub const ISSUE_COLUMNS: &str = "\
    id, title, description, design, acceptance_criteria, notes, \
    status, priority, issue_type, \
    assignee, owner, created_by, estimated_minutes, \
    created_at, updated_at, started_at, closed_at, close_reason, closed_by_session, \
    lease_expires_at, heartbeat_at, due_at, defer_until, \
    external_ref, source_system, spec_id, metadata, \
    ephemeral, no_history, pinned, is_template, \
    wisp_type, mol_type, work_type, content_hash";

/// The unit-variant enums (`WispType`, `MolType`, `WorkType`, `EventType`) have
/// no `as_str`, only a serde renaming. Going through serde keeps the stored
/// spelling and the JSON spelling from drifting apart.
pub fn enum_to_str<T: Serialize>(v: &T) -> Option<String> {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => Some(s),
        _ => None,
    }
}

pub fn enum_from_str<T: DeserializeOwned>(s: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
}

pub fn issue_from_row(r: &SqliteRow) -> Result<Issue> {
    let metadata: Option<String> = r.try_get("metadata").map_err(dec)?;
    let metadata = match metadata.as_deref() {
        None | Some("") => None,
        Some(s) => Some(serde_json::from_str(s).map_err(|e| {
            Error::Db(format!("issue {}: corrupt metadata JSON: {e}", row_id(r)))
        })?),
    };

    let opt_enum = |col: &str| -> Result<Option<String>> { r.try_get(col).map_err(dec) };
    let wisp_type = opt_enum("wisp_type")?
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(enum_from_str);
    let mol_type = opt_enum("mol_type")?
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(enum_from_str);
    let work_type = opt_enum("work_type")?
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(enum_from_str);

    Ok(Issue {
        id: r.try_get("id").map_err(dec)?,
        title: r.try_get("title").map_err(dec)?,
        description: r.try_get("description").map_err(dec)?,
        design: r.try_get("design").map_err(dec)?,
        acceptance_criteria: r.try_get("acceptance_criteria").map_err(dec)?,
        notes: r.try_get("notes").map_err(dec)?,

        status: Status::from(r.try_get::<String, _>("status").map_err(dec)?),
        priority: Priority(r.try_get("priority").map_err(dec)?),
        issue_type: IssueType::from(r.try_get::<String, _>("issue_type").map_err(dec)?),

        assignee: r.try_get("assignee").map_err(dec)?,
        owner: r.try_get("owner").map_err(dec)?,
        created_by: r.try_get("created_by").map_err(dec)?,
        estimated_minutes: r.try_get("estimated_minutes").map_err(dec)?,

        created_at: r.try_get("created_at").map_err(dec)?,
        updated_at: r.try_get("updated_at").map_err(dec)?,
        started_at: r.try_get("started_at").map_err(dec)?,
        closed_at: r.try_get("closed_at").map_err(dec)?,
        close_reason: r.try_get("close_reason").map_err(dec)?,
        closed_by_session: r.try_get("closed_by_session").map_err(dec)?,

        lease_expires_at: r.try_get("lease_expires_at").map_err(dec)?,
        heartbeat_at: r.try_get("heartbeat_at").map_err(dec)?,

        due_at: r.try_get("due_at").map_err(dec)?,
        defer_until: r.try_get("defer_until").map_err(dec)?,

        external_ref: r.try_get("external_ref").map_err(dec)?,
        source_system: r.try_get("source_system").map_err(dec)?,
        spec_id: r.try_get("spec_id").map_err(dec)?,
        metadata,

        ephemeral: r.try_get("ephemeral").map_err(dec)?,
        no_history: r.try_get("no_history").map_err(dec)?,
        pinned: r.try_get("pinned").map_err(dec)?,
        is_template: r.try_get("is_template").map_err(dec)?,

        wisp_type,
        mol_type,
        work_type,

        // Hydrated separately, and only by `get_issue`.
        labels: Vec::new(),
        dependencies: Vec::new(),
        comments: Vec::new(),

        content_hash: r.try_get("content_hash").map_err(dec)?,
    })
}

pub fn dependency_from_row(r: &SqliteRow) -> Result<Dependency> {
    Ok(Dependency {
        issue_id: r.try_get("issue_id").map_err(dec)?,
        depends_on_id: r.try_get("depends_on_id").map_err(dec)?,
        dep_type: DependencyType::from(r.try_get::<String, _>("type").map_err(dec)?),
        created_at: r.try_get("created_at").map_err(dec)?,
        created_by: r.try_get("created_by").map_err(dec)?,
        metadata: r
            .try_get::<Option<String>, _>("metadata")
            .map_err(dec)?
            .unwrap_or_default(),
        thread_id: r
            .try_get::<Option<String>, _>("thread_id")
            .map_err(dec)?
            .unwrap_or_default(),
    })
}

pub fn comment_from_row(r: &SqliteRow) -> Result<Comment> {
    Ok(Comment {
        id: r.try_get("id").map_err(dec)?,
        issue_id: r.try_get("issue_id").map_err(dec)?,
        author: r.try_get("author").map_err(dec)?,
        text: r.try_get("text").map_err(dec)?,
        created_at: r.try_get("created_at").map_err(dec)?,
    })
}

pub fn event_from_row(r: &SqliteRow) -> Result<Event> {
    let raw: String = r.try_get("event_type").map_err(dec)?;
    let event_type: EventType = enum_from_str(&raw)
        .ok_or_else(|| Error::Db(format!("unknown event type in database: {raw}")))?;
    Ok(Event {
        id: r.try_get("id").map_err(dec)?,
        issue_id: r.try_get("issue_id").map_err(dec)?,
        event_type,
        actor: r.try_get("actor").map_err(dec)?,
        old_value: r.try_get("old_value").map_err(dec)?,
        new_value: r.try_get("new_value").map_err(dec)?,
        created_at: r.try_get("created_at").map_err(dec)?,
    })
}

/// Empty strings become NULL on the way in, so that `json_valid` guards and
/// `IS NULL` checks in SQL see one representation of "absent", not two.
pub fn none_if_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_string())
}

pub fn metadata_to_text(v: &Option<serde_json::Value>) -> Option<String> {
    v.as_ref().map(|m| m.to_string())
}

fn row_id(r: &SqliteRow) -> String {
    r.try_get::<String, _>("id").unwrap_or_default()
}

fn dec(e: sqlx::Error) -> Error {
    Error::Db(e.to_string())
}
