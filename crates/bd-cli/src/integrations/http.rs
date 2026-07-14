//! The HTTP seam.
//!
//! Trackers talk to the network through this and only this. The real
//! implementation wraps `reqwest`; the test implementation replays fixtures. A
//! tracker cannot tell them apart, which is the point: every integration is
//! testable offline, with no credentials, against a recorded response.

use anyhow::{Result, bail};
use async_trait::async_trait;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    pub fn as_str(&self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }
}

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

impl HttpRequest {
    pub fn get(url: impl Into<String>) -> Self {
        HttpRequest {
            method: Method::Get,
            url: url.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    pub fn post(url: impl Into<String>, body: impl Into<String>) -> Self {
        HttpRequest {
            method: Method::Post,
            url: url.into(),
            headers: Vec::new(),
            body: Some(body.into()),
        }
    }

    pub fn header(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }

    pub fn bearer(self, token: &str) -> Self {
        self.header("Authorization", format!("Bearer {token}"))
    }

    pub fn json(mut self) -> Self {
        self.headers
            .push(("Content-Type".into(), "application/json".into()));
        self
    }
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

impl HttpResponse {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Parse the body as JSON, or fail with the status and body included.
    ///
    /// A 401 whose body says "token expired" is infinitely more useful than
    /// "error decoding response body", which is what you get if you deserialize
    /// without checking the status first.
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        if !self.ok() {
            bail!("HTTP {}: {}", self.status, truncate(&self.body, 500));
        }
        serde_json::from_str(&self.body)
            .map_err(|e| anyhow::anyhow!("unexpected response shape: {e}\nbody: {}", truncate(&self.body, 500)))
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

#[async_trait]
pub trait Http: Send + Sync {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse>;
}

// ---------------------------------------------------------------------------
// Real
// ---------------------------------------------------------------------------

pub struct RealHttp {
    client: reqwest::Client,
}

impl RealHttp {
    pub fn new() -> Result<Self> {
        Ok(RealHttp {
            client: reqwest::Client::builder()
                .user_agent(concat!("bd/", env!("CARGO_PKG_VERSION")))
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
        })
    }
}

#[async_trait]
impl Http for RealHttp {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse> {
        let mut b = match req.method {
            Method::Get => self.client.get(&req.url),
            Method::Post => self.client.post(&req.url),
            Method::Put => self.client.put(&req.url),
            Method::Patch => self.client.patch(&req.url),
            Method::Delete => self.client.delete(&req.url),
        };
        for (k, v) in &req.headers {
            b = b.header(k, v);
        }
        if let Some(body) = &req.body {
            b = b.body(body.clone());
        }

        let resp = b.send().await?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        Ok(HttpResponse { status, body })
    }
}

// ---------------------------------------------------------------------------
// Fake, for tests
// ---------------------------------------------------------------------------

/// Replays canned responses keyed by `"METHOD url"`.
///
/// Records every request it was given, so a test can assert not just on what
/// came back but on what was *asked* — which is where the bugs live. A tracker
/// that pages correctly and a tracker that silently fetches only page one look
/// identical from the outside; they differ in the requests they make.
#[derive(Default)]
pub struct FakeHttp {
    responses: HashMap<String, HttpResponse>,
    pub seen: std::sync::Mutex<Vec<HttpRequest>>,
}

impl FakeHttp {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on(mut self, method: Method, url: &str, status: u16, body: &str) -> Self {
        self.responses.insert(
            format!("{} {url}", method.as_str()),
            HttpResponse {
                status,
                body: body.to_string(),
            },
        );
        self
    }

    /// The requests made, in order.
    pub fn requests(&self) -> Vec<HttpRequest> {
        self.seen.lock().unwrap().clone()
    }
}

#[async_trait]
impl Http for FakeHttp {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse> {
        let key = format!("{} {}", req.method.as_str(), req.url);
        self.seen.lock().unwrap().push(req);

        // An unstubbed request is a test bug, and it must be loud. Returning an
        // empty 200 here would let a tracker that calls the wrong URL pass.
        self.responses
            .get(&key)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("FakeHttp: no stubbed response for `{key}`"))
    }
}
