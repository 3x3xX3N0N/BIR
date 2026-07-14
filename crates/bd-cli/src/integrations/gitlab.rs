//! GitLab sync. Not implemented yet.

use anyhow::Result;
use async_trait::async_trait;

use super::{Http, SyncReport, Tracker, TrackerStatus};
use crate::context::Ctx;

pub struct GitLab;

#[async_trait]
impl Tracker for GitLab {
    fn name(&self) -> &'static str {
        "gitlab"
    }

    fn required_config(&self) -> &'static [&'static str] {
        &["gitlab.url"]
    }

    fn secret_env(&self) -> &'static str {
        "GITLAB_TOKEN"
    }

    async fn status(&self, _ctx: &Ctx) -> Result<TrackerStatus> {
        Ok(TrackerStatus {
            name: "gitlab".into(),
            configured: false,
            missing: self.required_config().iter().map(|s| s.to_string()).collect(),
            detail: Some("not implemented yet".into()),
        })
    }

    async fn pull(&self, _ctx: &Ctx, _http: &dyn Http) -> Result<SyncReport> {
        anyhow::bail!("gitlab pull is not implemented yet")
    }

    async fn push(&self, _ctx: &Ctx, _http: &dyn Http) -> Result<SyncReport> {
        anyhow::bail!("gitlab push is not implemented yet")
    }
}
