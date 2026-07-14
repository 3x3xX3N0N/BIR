//! Ado sync. Not implemented yet.

use anyhow::Result;
use async_trait::async_trait;

use super::{Http, SyncReport, Tracker, TrackerStatus};
use crate::context::Ctx;

pub struct Ado;

#[async_trait]
impl Tracker for Ado {
    fn name(&self) -> &'static str {
        "ado"
    }

    fn required_config(&self) -> &'static [&'static str] {
        &["ado.url"]
    }

    fn secret_env(&self) -> &'static str {
        "AZURE_DEVOPS_PAT"
    }

    async fn status(&self, _ctx: &Ctx) -> Result<TrackerStatus> {
        Ok(TrackerStatus {
            name: "ado".into(),
            configured: false,
            missing: self.required_config().iter().map(|s| s.to_string()).collect(),
            detail: Some("not implemented yet".into()),
        })
    }

    async fn pull(&self, _ctx: &Ctx, _http: &dyn Http) -> Result<SyncReport> {
        anyhow::bail!("ado pull is not implemented yet")
    }

    async fn push(&self, _ctx: &Ctx, _http: &dyn Http) -> Result<SyncReport> {
        anyhow::bail!("ado push is not implemented yet")
    }
}
