//! GitLab REST v4 client: lazy token, error taxonomy mapping, page
//! aggregation. One blocking reqwest client; every call carries the
//! request timeout — a hung endpoint fails one request, never the
//! transport (the protocol's deadline semantics live in rootle; this
//! is the backend-side courtesy bound).

use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const DEFAULT_INSTANCE: &str = "https://gitlab.com";
pub const DEFAULT_TOKEN_ENV: &str = "GITLAB_TOKEN";

/// Every blob served through the protocol must fit the preview cap —
/// rootle refuses over 1 MiB at its boundary; refusing here first
/// saves the transfer (plans/0009 F5).
pub const BLOB_CAP: usize = 1024 * 1024;

/// Trees aggregate server pages up to this many entries; past it the
/// listing reports `truncated: true` (plans/0009 F4).
pub const TREE_ENTRY_CAP: usize = 25_000;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{message}")]
    Api {
        status: u16,
        message: String,
        retry_after: Option<u64>,
    },
    #[error("network: {0}")]
    Network(String),
}

pub type ApiResult<T> = Result<T, ApiError>;

pub struct GitLab {
    instance: String,
    token_env: String,
    http: reqwest::blocking::Client,
    token: std::sync::OnceLock<String>,
}

/// Null-tolerant by necessity (reader-tolerance protocol rule): empty
/// repos have `default_branch: null`, and `simple=true` listings can
/// omit optional fields — one odd project in a page must not fail the
/// whole listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: u64,
    pub path_with_namespace: String,
    #[serde(default)]
    pub default_branch: Option<String>,
    #[serde(default)]
    pub web_url: Option<String>,
    #[serde(default)]
    pub http_url_to_repo: Option<String>,
}

impl Project {
    pub fn branch(&self) -> String {
        self.default_branch.clone().unwrap_or_else(|| "main".into())
    }
    pub fn web(&self) -> String {
        self.web_url
            .clone()
            .unwrap_or_else(|| format!("https://gitlab.com/{}", self.path_with_namespace))
    }
    pub fn clone_remote(&self) -> String {
        self.http_url_to_repo
            .clone()
            .unwrap_or_else(|| format!("https://gitlab.com/{}.git", self.path_with_namespace))
    }
}

#[derive(Debug, Deserialize)]
pub struct Group {
    pub id: u64,
    pub full_path: String,
    pub web_url: String,
}

#[derive(Debug, Deserialize)]
pub struct TreeEntry {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String, // "blob" | "tree"
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct Branch {
    pub commit: BranchCommit,
}

#[derive(Debug, Deserialize)]
pub struct BranchCommit {
    pub id: String,
}

/// Blob search hit — GitLab returns real line numbers (startline),
/// so hits arrive located (plans/0009 F7).
#[derive(Debug, Deserialize)]
pub struct BlobHit {
    pub project_id: u64,
    pub path: String,
    #[serde(rename = "ref", default)]
    pub git_ref: Option<String>,
    pub startline: Option<u32>,
    pub data: Option<String>,
}

impl BlobHit {
    pub fn branch(&self) -> String {
        self.git_ref.clone().unwrap_or_else(|| "main".into())
    }
}

impl GitLab {
    pub fn new(instance: &str, token_env: &str) -> Self {
        GitLab {
            instance: instance.trim_end_matches('/').to_string(),
            token_env: token_env.to_string(),
            http: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
            token: std::sync::OnceLock::new(),
        }
    }

    /// Lazily read the token on first use — never at startup: rootle
    /// may respawn this process many times per session (the restart
    /// obligations in the protocol), and a missing token is a per-call
    /// auth error, not a crash.
    fn token(&self) -> ApiResult<&str> {
        if let Some(t) = self.token.get() {
            return Ok(t);
        }
        let t = std::env::var(&self.token_env).unwrap_or_default();
        if t.is_empty() {
            return Err(ApiError::Api {
                status: 401,
                message: format!(
                    "no {} in environment — set it to a GitLab token with read_api + read_repository",
                    self.token_env
                ),
                retry_after: None,
            });
        }
        Ok(self.token.get_or_init(|| t))
    }

    fn url(&self, path: &str, query: &[(&str, &str)]) -> String {
        let mut url = format!("{}/api/v4{}", self.instance, path);
        if !query.is_empty() {
            let qs: Vec<String> = query
                .iter()
                .map(|(k, v)| format!("{k}={}", urlencode_path(v)))
                .collect();
            url.push('?');
            url.push_str(&qs.join("&"));
        }
        url
    }

    pub fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> ApiResult<T> {
        let token = self.token()?;
        let req = self.http.get(self.url(path, query)).bearer_auth(token);
        let resp = req.send().map_err(|e| map_send_err(&e))?;
        check_status(resp)?
            .json()
            .map_err(|e| ApiError::Network(e.to_string()))
    }

    pub fn get_bytes(&self, path: &str) -> ApiResult<Vec<u8>> {
        let token = self.token()?;
        let req = self.http.get(self.url(path, &[])).bearer_auth(token);
        let resp = req.send().map_err(|e| map_send_err(&e))?;
        check_status(resp)?
            .bytes()
            .map(|b| b.to_vec())
            .map_err(|e| ApiError::Network(e.to_string()))
    }

    /// Aggregate numbered pages until a short page or `entry_cap`
    /// (GitLab caps per_page at 100). Returns (entries, truncated).
    pub fn get_pages<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        base_query: &[(&str, &str)],
        entry_cap: usize,
    ) -> ApiResult<(Vec<T>, bool)> {
        let mut out = Vec::new();
        let mut page = 1u32;
        loop {
            let page_s = page.to_string();
            let mut q: Vec<(&str, &str)> = base_query.to_vec();
            q.push(("per_page", "100"));
            q.push(("page", page_s.as_str()));
            let batch: Vec<T> = self.get_json(path, &q)?;
            let n = batch.len();
            out.extend(batch);
            if n < 100 {
                return Ok((out, false));
            }
            if out.len() >= entry_cap {
                out.truncate(entry_cap);
                return Ok((out, true));
            }
            page += 1;
        }
    }

    pub fn project(&self, path: &str) -> ApiResult<Project> {
        let enc = urlencode_path(path);
        self.get_json(&format!("/projects/{enc}"), &[])
    }

    pub fn group(&self, path: &str) -> ApiResult<Group> {
        let enc = urlencode_path(path);
        self.get_json(&format!("/groups/{enc}"), &[])
    }

    pub fn group_projects(&self, group_id: u64) -> ApiResult<(Vec<Project>, bool)> {
        self.get_pages(
            &format!("/groups/{group_id}/projects"),
            &[("simple", "true"), ("include_subgroups", "true")],
            500,
        )
    }

    pub fn branch_head(&self, project_id: u64, branch: &str) -> ApiResult<String> {
        let enc = urlencode_path(branch);
        let b: Branch = self.get_json(
            &format!("/projects/{project_id}/repository/branches/{enc}"),
            &[],
        )?;
        Ok(b.commit.id)
    }

    pub fn tree_page(&self, project_id: u64, branch: &str, page: u32) -> ApiResult<Vec<TreeEntry>> {
        let page_s = page.to_string();
        self.get_json(
            &format!("/projects/{project_id}/repository/tree"),
            &[
                ("ref", branch),
                ("recursive", "true"),
                ("per_page", "100"),
                ("page", page_s.as_str()),
            ],
        )
    }

    pub fn blob_raw(&self, project_id: u64, sha: &str) -> ApiResult<Vec<u8>> {
        self.get_bytes(&format!(
            "/projects/{project_id}/repository/blobs/{sha}/raw"
        ))
    }

    pub fn search_projects(&self, query: &str) -> ApiResult<Vec<Project>> {
        let items: Vec<Project> = self.get_json(
            "/projects",
            &[
                ("search", query),
                ("simple", "true"),
                ("per_page", "20"),
                ("order_by", "id"),
            ],
        )?;
        Ok(items)
    }

    pub fn search_groups(&self, query: &str) -> ApiResult<Vec<Group>> {
        self.get_json("/groups", &[("search", query), ("per_page", "5")])
    }

    pub fn search_blobs(&self, scope_path: &str, terms: &str) -> ApiResult<Vec<BlobHit>> {
        self.get_json(
            scope_path,
            &[("scope", "blobs"), ("search", terms), ("per_page", "20")],
        )
    }

    pub fn instance(&self) -> &str {
        &self.instance
    }
}

fn check_status(resp: reqwest::blocking::Response) -> ApiResult<reqwest::blocking::Response> {
    let status = resp.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(resp);
    }
    let retry_after = resp
        .headers()
        .get("Retry-After")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    let message = resp
        .json::<serde_json::Value>()
        .ok()
        .and_then(|v| {
            v.get("message")
                .or_else(|| v.get("error"))
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| format!("gitlab http {status}"));
    Err(ApiError::Api {
        status,
        message,
        retry_after,
    })
}

fn map_send_err(e: &reqwest::Error) -> ApiError {
    if e.is_timeout() {
        ApiError::Api {
            status: 0,
            message: "gitlab request timed out".into(),
            retry_after: None,
        }
    } else {
        ApiError::Network(e.to_string())
    }
}

/// Percent-encode a path *with* its slashes preserved but every other
/// reserved byte escaped — GitLab addresses sub-resources by
/// `group%2Fproject` (whole-path encoding) but branches by
/// name-with-slashes. Two helpers, two jobs.
pub fn urlencode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
