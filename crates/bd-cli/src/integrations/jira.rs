//! Jira sync. Not implemented yet.

use anyhow::Result;
use async_trait::async_trait;

use super::{Http, SyncReport, Tracker, TrackerStatus};
use crate::context::Ctx;

pub struct Jira;

#[async_trait]
impl Tracker for Jira {
    fn name(&self) -> &'static str {
        "jira"
    }

    fn required_config(&self) -> &'static [&'static str] {
        &["jira.url"]
    }

    fn secret_env(&self) -> &'static str {
        "JIRA_TOKEN"
    }

    async fn status(&self, _ctx: &Ctx) -> Result<TrackerStatus> {
        Ok(TrackerStatus {
            name: "jira".into(),
            configured: false,
            missing: self.required_config().iter().map(|s| s.to_string()).collect(),
            detail: Some("not implemented yet".into()),
        })
    }

    async fn pull(&self, _ctx: &Ctx, _http: &dyn Http) -> Result<SyncReport> {
        anyhow::bail!("jira pull is not implemented yet")
    }

    async fn push(&self, _ctx: &Ctx, _http: &dyn Http) -> Result<SyncReport> {
        anyhow::bail!("jira push is not implemented yet")
    }
}
