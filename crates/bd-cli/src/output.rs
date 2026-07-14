//! Rendering.
//!
//! Two audiences, one rule each:
//!
//! * **Humans** get whatever reads best. This is a clean-room port; the text
//!   layout owes nothing to the Go original.
//! * **Agents** get `--json`, whose field names are exactly
//!   [`bd_core::Issue`]'s serde names. External tooling parses that shape, so it
//!   is a compatibility surface: never rename a field here, never wrap an issue
//!   in an envelope. If you need to say something *about* an issue, put it
//!   beside the issue, not around it.

use std::io::{IsTerminal, Write};

use anyhow::Result;
use bd_core::{Comment, Dependency, Event, Issue, Priority, Status};
use serde::Serialize;
use serde_json::{Map, Value, json};

const TITLE_WIDTH: usize = 56;

#[derive(Debug, Clone)]
pub struct Out {
    json: bool,
    color: bool,
    quiet: bool,
    pub verbose: u8,
}

impl Out {
    pub fn new(json: bool, no_color: bool, quiet: bool, verbose: u8) -> Self {
        // Color is a property of the *destination*, not of the user's wishes
        // alone: piping into a file must never smuggle escape codes into it.
        let color = !json
            && !no_color
            && std::env::var_os("NO_COLOR").is_none()
            && std::io::stdout().is_terminal();
        Out {
            json,
            color,
            quiet,
            verbose,
        }
    }

    pub fn is_json(&self) -> bool {
        self.json
    }

    /// A line of human output. Silent under `--json` — structured mode emits
    /// exactly one document, and a stray "Created issue: ..." would corrupt it.
    pub fn line(&self, msg: impl AsRef<str>) {
        if self.json || self.quiet {
            return;
        }
        println!("{}", msg.as_ref());
    }

    /// Diagnostics go to stderr so that `bd q ... | xargs` keeps working.
    pub fn warn(&self, msg: impl AsRef<str>) {
        eprintln!("{}", self.paint(&format!("warning: {}", msg.as_ref()), YELLOW));
    }

    pub fn detail(&self, msg: impl AsRef<str>) {
        if self.verbose > 0 && !self.quiet {
            eprintln!("{}", self.paint(msg.as_ref(), DIM));
        }
    }

    pub fn json_value(&self, v: &impl Serialize) -> Result<()> {
        let mut w = std::io::stdout().lock();
        serde_json::to_writer_pretty(&mut w, v)?;
        w.write_all(b"\n")?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Issues
    // -----------------------------------------------------------------------

    /// A list of issues: JSON array, or a table.
    pub fn issues(&self, issues: &[Issue]) -> Result<()> {
        if self.json {
            return self.json_value(&issues);
        }
        if issues.is_empty() {
            self.line("No matching issues.");
            return Ok(());
        }
        let show_assignee = issues.iter().any(|i| !i.assignee.is_empty());

        let id_w = width(issues.iter().map(|i| i.id.len()), 2, 20);
        let status_w = width(issues.iter().map(|i| i.status.as_str().len()), 6, 14);
        let type_w = width(issues.iter().map(|i| i.issue_type.as_str().len()), 4, 12);
        let assignee_w = if show_assignee {
            width(issues.iter().map(|i| i.assignee.len()), 8, 18)
        } else {
            0
        };

        let mut header = format!(
            "{:<id_w$}  {:<2}  {:<status_w$}  {:<type_w$}  ",
            "ID", "P", "STATUS", "TYPE"
        );
        if show_assignee {
            header.push_str(&format!("{:<assignee_w$}  ", "ASSIGNEE"));
        }
        header.push_str("TITLE");
        self.line(self.paint(&header, BOLD));

        for i in issues {
            // Pad *first*, then paint the padded cell. Painting first would let
            // the escape bytes count toward the column width and every colored
            // row would drift right.
            let pri = self.paint(
                &format!("P{}", i.priority.0),
                priority_color(i.priority),
            );
            let status = self.paint(
                &format!("{:<status_w$}", truncate(i.status.as_str(), status_w)),
                status_color(&i.status),
            );
            let mut row = format!(
                "{:<id_w$}  {}  {}  {:<type_w$}  ",
                truncate(&i.id, id_w),
                pri,
                status,
                truncate(i.issue_type.as_str(), type_w),
            );
            if show_assignee {
                row.push_str(&format!(
                    "{:<assignee_w$}  ",
                    truncate(&i.assignee, assignee_w)
                ));
            }
            row.push_str(&truncate(&i.title, TITLE_WIDTH));
            println!("{row}");
        }
        if !self.quiet {
            self.line(format!(
                "\n{}",
                self.paint(&format!("{} issue(s)", issues.len()), DIM)
            ));
        }
        Ok(())
    }

    /// One issue, in full, with its edges and comments.
    pub fn issue_detail(
        &self,
        issue: &Issue,
        depends_on: &[Dependency],
        dependents: &[Dependency],
        comments: &[Comment],
    ) -> Result<()> {
        if self.json {
            return self.json_value(&issue_json(issue, depends_on, dependents, comments));
        }

        self.line(format!(
            "{} {}",
            self.paint(&issue.id, BOLD),
            self.paint(&issue.title, BOLD)
        ));
        self.line(format!(
            "  {}  {}  {}{}",
            self.paint(
                &format!("P{}", issue.priority.0),
                priority_color(issue.priority)
            ),
            self.paint(issue.status.as_str(), status_color(&issue.status)),
            issue.issue_type,
            if issue.assignee.is_empty() {
                String::new()
            } else {
                format!("  @{}", issue.assignee)
            }
        ));
        self.line(format!(
            "  created {}  updated {}",
            issue.created_at.format("%Y-%m-%d %H:%M"),
            issue.updated_at.format("%Y-%m-%d %H:%M")
        ));
        if let Some(d) = issue.defer_until {
            self.line(format!("  deferred until {}", d.format("%Y-%m-%d %H:%M")));
        }
        if let Some(d) = issue.due_at {
            self.line(format!("  due {}", d.format("%Y-%m-%d %H:%M")));
        }
        if let Some(l) = issue.lease_expires_at {
            self.line(format!("  lease expires {}", l.format("%Y-%m-%d %H:%M")));
        }
        if !issue.labels.is_empty() {
            self.line(format!("  labels: {}", issue.labels.join(", ")));
        }
        if !issue.close_reason.is_empty() {
            self.line(format!("  closed: {}", issue.close_reason));
        }

        self.section("Description", &issue.description);
        self.section("Design", &issue.design);
        self.section("Acceptance", &issue.acceptance_criteria);
        self.section("Notes", &issue.notes);

        if !depends_on.is_empty() {
            self.line(format!("\n{}", self.paint("Depends on", BOLD)));
            for d in depends_on {
                self.line(format!("  {} [{}]", d.depends_on_id, d.dep_type));
            }
        }
        if !dependents.is_empty() {
            self.line(format!("\n{}", self.paint("Depended on by", BOLD)));
            for d in dependents {
                self.line(format!("  {} [{}]", d.issue_id, d.dep_type));
            }
        }
        if !comments.is_empty() {
            self.line(format!("\n{}", self.paint("Comments", BOLD)));
            for c in comments {
                self.line(format!(
                    "  {} {}: {}",
                    self.paint(&c.created_at.format("%Y-%m-%d %H:%M").to_string(), DIM),
                    c.author,
                    c.text
                ));
            }
        }
        Ok(())
    }

    pub fn comments(&self, comments: &[Comment]) -> Result<()> {
        if self.json {
            return self.json_value(&comments);
        }
        if comments.is_empty() {
            self.line("No comments.");
            return Ok(());
        }
        for c in comments {
            self.line(format!(
                "{} {}: {}",
                self.paint(&c.created_at.format("%Y-%m-%d %H:%M").to_string(), DIM),
                c.author,
                c.text
            ));
        }
        Ok(())
    }

    pub fn events(&self, events: &[Event]) -> Result<()> {
        if self.json {
            return self.json_value(&events);
        }
        if events.is_empty() {
            self.line("No history.");
            return Ok(());
        }
        for e in events {
            let change = match (&e.old_value, &e.new_value) {
                (Some(o), Some(n)) => format!(" {o} -> {n}"),
                (None, Some(n)) => format!(" {n}"),
                _ => String::new(),
            };
            self.line(format!(
                "{} {:?} by {}{}",
                self.paint(&e.created_at.format("%Y-%m-%d %H:%M").to_string(), DIM),
                e.event_type,
                e.actor,
                change
            ));
        }
        Ok(())
    }

    fn section(&self, title: &str, body: &str) {
        if body.trim().is_empty() {
            return;
        }
        self.line(format!("\n{}", self.paint(title, BOLD)));
        for l in body.lines() {
            self.line(format!("  {l}"));
        }
    }

    // -----------------------------------------------------------------------
    // Color
    // -----------------------------------------------------------------------

    pub fn paint(&self, s: &str, code: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
}

const BOLD: &str = "1";
const DIM: &str = "2";
const RED: &str = "31";
const YELLOW: &str = "33";
const GREEN: &str = "32";
const BLUE: &str = "34";

fn priority_color(p: Priority) -> &'static str {
    match p.0 {
        0 => RED,
        1 => YELLOW,
        3 | 4 => DIM,
        _ => "",
    }
}

fn status_color(s: &Status) -> &'static str {
    match s {
        Status::Open => BLUE,
        Status::InProgress => GREEN,
        Status::Closed => DIM,
        Status::Blocked => RED,
        _ => "",
    }
}

fn width(lens: impl Iterator<Item = usize>, min: usize, max: usize) -> usize {
    lens.max().unwrap_or(min).clamp(min, max)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{keep}…")
}

/// The `--json` shape for a single issue: the issue's own serde fields, plus
/// hydrated relations *beside* them (never wrapping them).
pub fn issue_json(
    issue: &Issue,
    depends_on: &[Dependency],
    dependents: &[Dependency],
    comments: &[Comment],
) -> Value {
    let mut v = serde_json::to_value(issue).unwrap_or(Value::Null);
    if let Some(obj) = v.as_object_mut() {
        if !depends_on.is_empty() {
            obj.insert(
                "dependencies".into(),
                serde_json::to_value(depends_on).unwrap_or(Value::Null),
            );
        }
        if !dependents.is_empty() {
            obj.insert(
                "dependents".into(),
                serde_json::to_value(dependents).unwrap_or(Value::Null),
            );
        }
        if !comments.is_empty() {
            obj.insert(
                "comments".into(),
                serde_json::to_value(comments).unwrap_or(Value::Null),
            );
        }
    }
    v
}

/// A JSONL export record: the issue's fields with a `_type` discriminator, so a
/// reader can tell issues from whatever else a future export emits.
pub fn export_record(issue: &Issue) -> Result<Value> {
    let mut obj = match serde_json::to_value(issue)? {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    let mut out = Map::new();
    out.insert("_type".into(), json!("issue"));
    out.append(&mut obj);
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_record_carries_the_discriminator_and_the_issue_fields() {
        let issue = Issue::new("bd-1", "hello");
        let v = export_record(&issue).unwrap();
        assert_eq!(v["_type"], json!("issue"));
        assert_eq!(v["id"], json!("bd-1"));
        // P2 is the default, but priority is never omitted: P0 must survive.
        assert_eq!(v["priority"], json!(2));
    }

    #[test]
    fn json_field_names_match_bd_core() {
        let issue = Issue::new("bd-1", "t");
        let v = issue_json(&issue, &[], &[], &[]);
        // A rename here breaks every agent parsing our output.
        for key in ["id", "title", "status", "priority", "issue_type", "created_at"] {
            assert!(v.get(key).is_some(), "missing {key} in --json issue");
        }
    }

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("abcdef", 4), "abc…");
    }
}
