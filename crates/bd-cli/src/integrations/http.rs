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

#[derive(Debug, Clone, Default)]
pub struct HttpResponse {
    pub status: u16,
    /// Response headers, in the order the server sent them.
    ///
    /// Not decoration: GitHub's `Link: rel="next"` is the *only* authoritative
    /// statement of where the next page is, and Jira and GitLab put rate-limit
    /// and page-count information nowhere else. A seam that carries a status and
    /// a body forces every paginated tracker to infer the end of a listing from
    /// the shape of the data, which works until it doesn't.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl HttpResponse {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// A header by name, **case-insensitively** — HTTP header names are, and
    /// HTTP/2 lowercases every one of them in transit. Matching `"Link"` exactly
    /// finds nothing on an HTTP/2 connection and everything on an HTTP/1 one,
    /// which is a bug that reproduces only against certain servers.
    ///
    /// The first match wins. A repeated header (`Set-Cookie`) is not something
    /// this seam has any business folding.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
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
        // Headers before the body: `text()` consumes the response.
        let headers = resp
            .headers()
            .iter()
            // A header whose value is not UTF-8 is legal and unusable. Dropping
            // it beats failing the request over something no tracker reads.
            .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_string(), v.to_string())))
            .collect();
        let body = resp.text().await.unwrap_or_default();
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
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
///
/// Two kinds of stub, because two kinds of API:
///
/// * [`on`](FakeHttp::on) — one response, replayed for every request to that
///   key. Right when the URL identifies the answer, which is most REST calls.
/// * [`on_seq`](FakeHttp::on_seq) — a **queue** of responses for that key,
///   popped in order. Required for anything that paginates with the cursor in
///   the request *body*: Notion's `start_cursor` and Linear's GraphQL `after`
///   both make page one and page two the same method and the same URL, so one
///   response per key cannot express a two-page fixture at all.
#[derive(Default)]
pub struct FakeHttp {
    responses: HashMap<String, HttpResponse>,
    /// Keys stubbed with `on_seq`. Checked first, so a key is either sequenced
    /// or repeated, never half of each.
    queues: std::sync::Mutex<HashMap<String, std::collections::VecDeque<HttpResponse>>>,
    pub seen: std::sync::Mutex<Vec<HttpRequest>>,
}

impl FakeHttp {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on(mut self, method: Method, url: &str, status: u16, body: &str) -> Self {
        self.responses.insert(
            key(method, url),
            HttpResponse {
                status,
                headers: Vec::new(),
                body: body.to_string(),
            },
        );
        self
    }

    /// The same, but with response headers — for a tracker that reads `Link`,
    /// `Retry-After`, or anything else the server says outside the body.
    pub fn on_with_headers(
        mut self,
        method: Method,
        url: &str,
        status: u16,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Self {
        self.responses.insert(
            key(method, url),
            HttpResponse {
                status,
                headers: headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                body: body.to_string(),
            },
        );
        self
    }

    /// A queue of responses for one key, popped in order.
    ///
    /// Draining it is an error, loudly: a tracker that asks for one page more
    /// than the fixture has is either looping or mis-reading the cursor, and
    /// handing it a repeat of the last page would hide both.
    pub fn on_seq(self, method: Method, url: &str, responses: Vec<(u16, String)>) -> Self {
        self.queues.lock().unwrap().insert(
            key(method, url),
            responses
                .into_iter()
                .map(|(status, body)| HttpResponse {
                    status,
                    headers: Vec::new(),
                    body,
                })
                .collect(),
        );
        self
    }

    /// The requests made, in order.
    pub fn requests(&self) -> Vec<HttpRequest> {
        self.seen.lock().unwrap().clone()
    }

    /// The request bodies, in order — what a cursor assertion actually needs.
    pub fn bodies(&self) -> Vec<String> {
        self.requests()
            .into_iter()
            .map(|r| r.body.unwrap_or_default())
            .collect()
    }
}

fn key(method: Method, url: &str) -> String {
    format!("{} {url}", method.as_str())
}

#[async_trait]
impl Http for FakeHttp {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse> {
        let k = key(req.method, &req.url);
        self.seen.lock().unwrap().push(req);

        // A sequenced key wins: it is the more specific stub, and a test that set
        // one is asserting on the order.
        let mut queues = self.queues.lock().unwrap();
        if let Some(q) = queues.get_mut(&k) {
            return q.pop_front().ok_or_else(|| {
                anyhow::anyhow!(
                    "FakeHttp: the queue for `{k}` is drained — the tracker made more \
                     requests than the fixture has responses"
                )
            });
        }
        drop(queues);

        // An unstubbed request is a test bug, and it must be loud. Returning an
        // empty 200 here would let a tracker that calls the wrong URL pass.
        self.responses
            .get(&k)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("FakeHttp: no stubbed response for `{k}`"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HTTP header names are case-insensitive, and HTTP/2 lowercases every one of
    /// them on the wire. A tracker looking for `"Link"` by exact match finds it
    /// over HTTP/1 and misses it over HTTP/2 — so it pages correctly against the
    /// server you tested with and silently reads one page against the real one.
    #[test]
    fn a_header_is_found_whatever_its_case() {
        let resp = HttpResponse {
            status: 200,
            headers: vec![
                ("link".into(), "<https://api/next>; rel=\"next\"".into()),
                ("X-RateLimit-Remaining".into(), "42".into()),
            ],
            body: String::new(),
        };

        assert_eq!(resp.header("Link"), resp.header("link"));
        assert!(resp.header("Link").unwrap().contains("rel=\"next\""));
        assert_eq!(resp.header("x-ratelimit-remaining"), Some("42"));
        assert_eq!(resp.header("X-RATELIMIT-REMAINING"), Some("42"));
        assert_eq!(resp.header("Retry-After"), None);
    }

    #[tokio::test]
    async fn the_fake_stubs_headers_and_pops_a_queue_in_order() {
        let http = FakeHttp::new()
            .on_with_headers(Method::Get, "https://api/x", 200, &[("Link", "next")], "{}")
            // Two responses on one key: the case a single canned response per
            // `"METHOD url"` cannot express, and the reason `on_seq` exists.
            .on_seq(
                Method::Post,
                "https://api/q",
                vec![(200, "one".into()), (200, "two".into())],
            );

        assert_eq!(
            http.send(HttpRequest::get("https://api/x"))
                .await
                .unwrap()
                .header("link"),
            Some("next")
        );

        let post = || HttpRequest::post("https://api/q", "");
        assert_eq!(http.send(post()).await.unwrap().body, "one");
        assert_eq!(http.send(post()).await.unwrap().body, "two");

        // Drained, and loud about it. Replaying the last response instead would
        // let a tracker whose cursor never advances loop forever and pass.
        let err = http.send(post()).await.expect_err("the queue is empty");
        assert!(err.to_string().contains("drained"), "{err}");
    }
}
