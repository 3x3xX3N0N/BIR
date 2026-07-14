use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Workspace summary, as shown by `bd status`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    pub total: u64,
    pub open: u64,
    pub in_progress: u64,
    pub closed: u64,
    pub blocked: u64,
    /// Claimable right now. The number an agent actually cares about.
    pub ready: u64,
    pub by_priority: BTreeMap<i32, u64>,
    pub by_type: BTreeMap<String, u64>,
}
