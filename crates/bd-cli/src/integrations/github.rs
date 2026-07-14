//! GitHub sync. Not implemented yet.

use anyhow::Result;
use async_trait::async_trait;

use super::{Http, SyncReport, Tracker, TrackerStatus};
use crate::context::Ctx;

pub struct GitHub;

#[async_trait]
impl Tracker for GitHub {
    fn name(&self) -> &'static str {
        "github"
    }

    fn required_config(&self) -> &'static [&'static str] {
        &["github.url"]
    }

    fn secret_env(&self) -> &'static str {
        "GITHUB_TOKEN"
    }

    async fn status(&self, _ctx: &Ctx) -> Result<TrackerStatus> {
        Ok(TrackerStatus {
            name: "github".into(),
            configured: false,
            missing: self.required_config().iter().map(|s| s.to_string()).collect(),
            detail: Some("not implemented yet".into()),
        })
    }

    async fn pull(&self, _ctx: &Ctx, _http: &dyn Http) -> Result<SyncReport> {
        anyhow::bail!("github pull is not implemented yet")
    }

    async fn push(&self, _ctx: &Ctx, _http: &dyn Http) -> Result<SyncReport> {
        anyhow::bail!("github push is not implemented yet")
    }
}
