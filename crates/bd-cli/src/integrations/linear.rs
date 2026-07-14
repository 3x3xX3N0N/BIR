//! Linear sync. Not implemented yet.

use anyhow::Result;
use async_trait::async_trait;

use super::{Http, SyncReport, Tracker, TrackerStatus};
use crate::context::Ctx;

pub struct Linear;

#[async_trait]
impl Tracker for Linear {
    fn name(&self) -> &'static str {
        "linear"
    }

    fn required_config(&self) -> &'static [&'static str] {
        &["linear.url"]
    }

    fn secret_env(&self) -> &'static str {
        "LINEAR_API_KEY"
    }

    async fn status(&self, _ctx: &Ctx) -> Result<TrackerStatus> {
        Ok(TrackerStatus {
            name: "linear".into(),
            configured: false,
            missing: self.required_config().iter().map(|s| s.to_string()).collect(),
            detail: Some("not implemented yet".into()),
        })
    }

    async fn pull(&self, _ctx: &Ctx, _http: &dyn Http) -> Result<SyncReport> {
        anyhow::bail!("linear pull is not implemented yet")
    }

    async fn push(&self, _ctx: &Ctx, _http: &dyn Http) -> Result<SyncReport> {
        anyhow::bail!("linear push is not implemented yet")
    }
}
