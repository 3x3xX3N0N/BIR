use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// Pure-Rust, zero external dependencies, single-writer. No commit graph.
    #[default]
    Sqlite,
    /// Versioned MySQL-compatible store. Branch, merge, push, pull.
    Dolt,
    Postgres,
    Mysql,
}

impl Backend {
    pub fn as_str(&self) -> &'static str {
        match self {
            Backend::Sqlite => "sqlite",
            Backend::Dolt => "dolt",
            Backend::Postgres => "postgres",
            Backend::Mysql => "mysql",
        }
    }

    /// Whether this engine can branch, merge, and talk to remotes.
    pub fn has_commit_graph(&self) -> bool {
        matches!(self, Backend::Dolt)
    }
}

impl std::str::FromStr for Backend {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "sqlite" => Ok(Backend::Sqlite),
            "dolt" => Ok(Backend::Dolt),
            "postgres" | "postgresql" | "pg" => Ok(Backend::Postgres),
            "mysql" => Ok(Backend::Mysql),
            other => Err(Error::Db(format!("unknown backend: {other}"))),
        }
    }
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Points at a workspace and says what kind it is.
///
/// This is written once, at `bd init`, and is thereafter the *authority* on
/// which engine owns the data. Opening consults it and nothing else — not
/// `$BEADS_BACKEND`, not a flag. A workspace created as Dolt stays Dolt, and an
/// environment variable cannot silently reinterpret it as something else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Locator {
    pub backend: Backend,
    /// Stable id for the workspace, independent of its path on disk.
    pub workspace_id: String,
    /// The `.beads` directory.
    #[serde(skip)]
    pub dir: PathBuf,
}

pub const BEADS_DIR: &str = ".beads";
pub const LOCATOR_FILE: &str = "workspace.json";

impl Locator {
    pub fn new(backend: Backend, workspace_id: impl Into<String>, dir: impl Into<PathBuf>) -> Self {
        Locator {
            backend,
            workspace_id: workspace_id.into(),
            dir: dir.into(),
        }
    }

    /// Walk up from `start` looking for a `.beads` directory, the way git finds
    /// its root. Returns `None` if there is no workspace anywhere above.
    pub fn discover(start: &Path) -> Option<PathBuf> {
        let mut cur = Some(start);
        while let Some(dir) = cur {
            let candidate = dir.join(BEADS_DIR);
            if candidate.is_dir() {
                return Some(candidate);
            }
            cur = dir.parent();
        }
        None
    }

    pub fn load(beads_dir: &Path) -> Result<Self> {
        let path = beads_dir.join(LOCATOR_FILE);
        let raw = std::fs::read_to_string(&path).map_err(|_| Error::NoWorkspace)?;
        let mut loc: Locator =
            serde_json::from_str(&raw).map_err(|e| Error::Db(format!("corrupt {LOCATOR_FILE}: {e}")))?;
        loc.dir = beads_dir.to_path_buf();
        Ok(loc)
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| Error::Db(format!("could not create {}: {e}", self.dir.display())))?;
        let path = self.dir.join(LOCATOR_FILE);
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| Error::Db(format!("could not serialize locator: {e}")))?;
        // Write-then-rename: a crash mid-write must not leave a workspace that
        // cannot be opened.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| Error::Db(format!("could not write locator: {e}")))?;
        std::fs::rename(&tmp, &path)
            .map_err(|e| Error::Db(format!("could not commit locator: {e}")))?;
        Ok(())
    }

    /// Where the SQLite database file lives, for backends that use one.
    pub fn db_path(&self) -> PathBuf {
        self.dir.join("beads.db")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_round_trips() {
        for b in [Backend::Sqlite, Backend::Dolt, Backend::Postgres, Backend::Mysql] {
            assert_eq!(b.as_str().parse::<Backend>().unwrap(), b);
        }
        assert!("nonsense".parse::<Backend>().is_err());
    }

    #[test]
    fn only_dolt_has_a_commit_graph() {
        assert!(Backend::Dolt.has_commit_graph());
        assert!(!Backend::Sqlite.has_commit_graph());
        assert!(!Backend::Postgres.has_commit_graph());
    }

    #[test]
    fn locator_survives_a_round_trip_to_disk() {
        let tmp = std::env::temp_dir().join(format!("bd-loc-{}", std::process::id()));
        let beads = tmp.join(BEADS_DIR);
        let loc = Locator::new(Backend::Dolt, "ws-123", &beads);
        loc.save().unwrap();

        let loaded = Locator::load(&beads).unwrap();
        // Rule 3: the backend comes back from disk, not from ambient state.
        assert_eq!(loaded.backend, Backend::Dolt);
        assert_eq!(loaded.workspace_id, "ws-123");
        assert_eq!(loaded.dir, beads);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn discover_walks_up_like_git() {
        let tmp = std::env::temp_dir().join(format!("bd-disc-{}", std::process::id()));
        let nested = tmp.join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(tmp.join(BEADS_DIR)).unwrap();

        let found = Locator::discover(&nested).expect("should find .beads above");
        assert_eq!(found, tmp.join(BEADS_DIR));

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn discover_returns_none_outside_a_workspace() {
        let tmp = std::env::temp_dir().join(format!("bd-none-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        // No .beads anywhere under tmp; the only risk is a stray one above it
        // in the real temp tree, which we accept.
        if Locator::discover(&tmp).is_some_and(|p| p.starts_with(&tmp)) {
            panic!("found a workspace that should not exist");
        }
        std::fs::remove_dir_all(&tmp).ok();
    }
}
