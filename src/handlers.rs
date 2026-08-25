//! Protocol method handlers: one function per method, mapping between
//! the wire shapes (doc/provider-protocol.md) and the GitLab API.
//! Handlers are pure request→result — stdin plumbing lives in main.

use crate::api::{self, ApiError, ApiResult, GitLab, TREE_ENTRY_CAP};
use crate::cache::Cache;
use serde_json::{Value, json};

pub struct Handler {
    pub gl: GitLab,
    pub cache: Cache,
}

/// Wire error taxonomy (protocol v1.1): semantics ride in data.kind.
pub struct WireError {
    pub kind: &'static str,
    pub message: String,
    pub retry_after_s: Option<u64>,
}

impl WireError {
    fn from_api(e: &ApiError) -> WireError {
        match e {
            ApiError::Api {
                status,
                message,
                retry_after,
            } => {
                let kind = match status {
                    401 | 403 => "auth",
                    404 => "not_found",
                    429 => "rate_limited",
                    0 => "timeout",
                    _ => "provider",
                };
                WireError {
                    kind,
                    message: message.clone(),
                    // 429 without a header still tells the UI it's throttling.
                    retry_after_s: if *status == 429 {
                        (*retry_after).or(Some(30))
                    } else {
                        *retry_after
                    },
                }
            }
            ApiError::Network(m) => WireError {
                kind: "network",
                message: m.clone(),
                retry_after_s: None,
            },
        }
    }

    pub fn to_json(&self) -> Value {
        let mut data = json!({ "kind": self.kind });
        if let Some(s) = self.retry_after_s {
            data["retry_after_s"] = json!(s);
        }
        json!({ "code": 1, "message": self.message, "data": data })
    }
}

type WireResult = Result<Value, WireError>;

impl From<ApiError> for WireError {
    fn from(e: ApiError) -> WireError {
        WireError::from_api(&e)
    }
}

fn w<T>(r: ApiResult<T>, f: impl FnOnce(T) -> Value) -> WireResult {
    r.map(f).map_err(|e| WireError::from_api(&e))
}

impl Handler {
    pub fn new(instance: &str, token_env: &str, cache_base: Option<std::path::PathBuf>) -> Self {
        Handler {
            gl: GitLab::new(instance, token_env),
            cache: Cache::new(cache_base),
        }
    }

    pub fn dispatch(&self, method: &str, params: &Value) -> WireResult {
        match method {
            "initialize" => Ok(json!({
                "protocol": 1,
                "name": "gitlab",
                // Optimistic: startup does NO network (restart
                // obligations). Unavailable search surfaces as honest
                // per-call errors, not startup failure.
                "capabilities": { "orgs": true, "code_search": true }
            })),
            "search/repos" => self.search_repos(params["query"].as_str().unwrap_or("")),
            "org/repos" => self.org_repos(params["org"].as_str().unwrap_or("")),
            "repo/tree" => self.repo_tree(params["repo"].as_str().unwrap_or("")),
            "repo/blob" => self.repo_blob(
                params["repo"].as_str().unwrap_or(""),
                params["sha"].as_str().unwrap_or(""),
            ),
            "repo/clone_url" => self.clone_url(params["repo"].as_str().unwrap_or("")),
            "repo/web_url" => self.web_url(
                params["repo"].as_str().unwrap_or(""),
                params["path"].as_str().unwrap_or(""),
                params["branch"].as_str().unwrap_or(""),
                params["line"].as_u64(),
                params["is_file"].as_bool().unwrap_or(false),
            ),
            "org/url" => self.org_url(params["org"].as_str().unwrap_or("")),
            "search/code" => self.search_code(params["q"].as_str().unwrap_or("")),
            other => Err(WireError {
                kind: "provider",
                message: format!("unknown method {other:?}"),
                retry_after_s: None,
            }),
        }
    }

    /// Project metadata through the cache; a 404 invalidates a stale
    /// entry once (repo moved/renamed) and retries fresh.
    fn project(&self, path: &str) -> ApiResult<crate::api::Project> {
        if let Some(p) = self.cache.project(path) {
            return Ok(p);
        }
        match self.gl.project(path) {
            Ok(p) => {
                self.cache.put_project(&p.cache_fields());
                Ok(p)
            }
            Err(ApiError::Api { status: 404, .. }) => {
                self.cache.drop_project(path);
                self.gl.project(path)
            }
            Err(e) => Err(e),
        }
    }

    fn group_id(&self, org: &str) -> ApiResult<u64> {
        Ok(self.gl.group(org)?.id)
    }

    fn search_repos(&self, query: &str) -> WireResult {
        w(self.gl.search_projects(query), |projects| {
            let mut items: Vec<Value> = projects
                .into_iter()
                .map(|p| json!({ "full_name": p.path_with_namespace }))
                .collect();
            if let Ok(groups) = self.gl.search_groups(query) {
                for g in groups {
                    items.push(json!({ "org": g.full_path }));
                }
            }
            json!({ "items": items.into_iter().take(20).collect::<Vec<_>>() })
        })
    }

    fn org_repos(&self, org: &str) -> WireResult {
        let id = self.group_id(org)?;
        w(self.gl.group_projects(id), |(projects, truncated)| {
            // The repos level carries the path AFTER the org component
            // (rootle re-joins org + name); nested subgroups keep the
            // rest of their path — multi-slash ids are legal.
            let prefix = format!("{org}/");
            json!({
                "repos": projects
                    .into_iter()
                    .map(|p| {
                        p.path_with_namespace
                            .strip_prefix(&prefix)
                            .map(str::to_string)
                            .unwrap_or(p.path_with_namespace)
                    })
                    .collect::<Vec<String>>(),
                "truncated": truncated,
            })
        })
    }

    fn repo_tree(&self, repo: &str) -> WireResult {
        let project = self.project(repo).map_err(|e| WireError::from_api(&e))?;
        let branch = project.default_branch.clone();
        // Branch head first (mutable, one cheap call): a cached tree
        // keyed by the head sha is immutable thereafter.
        let head = match self.gl.branch_head(project.id, &branch) {
            Ok(h) => h,
            Err(e) => return Err(WireError::from_api(&e)),
        };
        if let Some(cached) = self.cache.tree(&head)
            && let Ok(v) = serde_json::from_slice::<Value>(&cached)
        {
            return Ok(v);
        }
        let mut entries = Vec::new();
        let mut truncated = false;
        let mut page = 1u32;
        loop {
            let batch = match self.gl.tree_page(project.id, &branch, page) {
                Ok(b) => b,
                Err(e) => return Err(WireError::from_api(&e)),
            };
            let n = batch.len();
            for e in batch {
                entries.push(json!({
                    "path": e.path,
                    "type": e.kind,          // "blob" | "tree" — same words
                    "sha": e.id,
                }));
            }
            if n < 100 {
                break;
            }
            if entries.len() >= TREE_ENTRY_CAP {
                entries.truncate(TREE_ENTRY_CAP);
                truncated = true;
                break;
            }
            page += 1;
        }
        let body = json!({
            "entries": entries,
            "truncated": truncated,
            "branch": branch,
        });
        self.cache.put_tree(&head, body.to_string().as_bytes());
        Ok(body)
    }

    fn repo_blob(&self, repo: &str, sha: &str) -> WireResult {
        if let Some(bytes) = self.cache.blob(sha) {
            return Ok(json!({ "bytes_b64": b64(&bytes) }));
        }
        let project = self.project(repo).map_err(|e| WireError::from_api(&e))?;
        let bytes = self
            .gl
            .blob_raw(project.id, sha)
            .map_err(|e| WireError::from_api(&e))?;
        if bytes.len() > api::BLOB_CAP {
            return Err(WireError {
                kind: "provider",
                message: format!(
                    "blob {sha} is {} KiB — over the 1 MiB preview cap",
                    bytes.len() / 1024
                ),
                retry_after_s: None,
            });
        }
        self.cache.put_blob(sha, &bytes);
        Ok(json!({ "bytes_b64": b64(&bytes) }))
    }

    fn clone_url(&self, repo: &str) -> WireResult {
        w(
            self.project(repo),
            |p| json!({ "clone_url": p.http_url_to_repo }),
        )
    }

    fn web_url(
        &self,
        repo: &str,
        path: &str,
        branch: &str,
        line: Option<u64>,
        is_file: bool,
    ) -> WireResult {
        w(self.project(repo), |p| {
            let mut url = p.web_url.clone();
            if !path.is_empty() {
                let kind = if is_file { "blob" } else { "tree" };
                // Slashes inside the branch or path are separators in
                // GitLab's URL grammar — encode per segment, not whole.
                let enc = |s: &str| {
                    s.split('/')
                        .map(api::urlencode_path)
                        .collect::<Vec<_>>()
                        .join("/")
                };
                url.push_str(&format!("/-/{kind}/{}", enc(branch)));
                url.push('/');
                url.push_str(&enc(path));
            }
            if is_file && line.is_some() {
                url.push_str(&format!("#L{}", line.unwrap_or(0)));
            }
            json!({ "url": url })
        })
    }

    fn org_url(&self, org: &str) -> WireResult {
        w(self.gl.group(org), |g| json!({ "url": g.web_url }))
    }

    /// q grammar (the protocol's PROTOCOL SURFACE): repo:/org:/path:/
    /// extension: qualifiers + free terms. repo: and org: scope the
    /// GitLab search endpoint server-side; path:/extension: filter
    /// client-side (no server equivalent). Hits carry GitLab's real
    /// startline — located, no client-side locating needed.
    fn search_code(&self, q: &str) -> WireResult {
        let mut repo_scope = None;
        let mut org_scope = None;
        let mut path_filter = None;
        let mut ext_filter = None;
        let mut terms = Vec::new();
        for tok in q.split_whitespace() {
            let (key, value) = match tok.split_once(':') {
                Some(kv) => kv,
                None => {
                    terms.push(tok);
                    continue;
                }
            };
            match key {
                "repo" => repo_scope = Some(value.to_string()),
                "org" => org_scope = Some(value.to_string()),
                "path" => path_filter = Some(value.to_lowercase()),
                "extension" => ext_filter = Some(value.trim_start_matches('.').to_lowercase()),
                _ => terms.push(tok),
            }
        }
        let search_terms = terms.join(" ");
        let scope_path = if let Some(repo) = &repo_scope {
            let project = self.project(repo).map_err(|e| WireError::from_api(&e))?;
            format!("/projects/{}/search", project.id)
        } else if let Some(org) = &org_scope {
            let id = self.group_id(org).map_err(|e| WireError::from_api(&e))?;
            format!("/groups/{id}/search")
        } else {
            "/search".to_string()
        };
        let hits = self
            .gl
            .search_blobs(&scope_path, &search_terms)
            .map_err(|e| WireError::from_api(&e))?;

        let mut items = Vec::new();
        for hit in hits {
            let path = hit.path.to_lowercase();
            if let Some(p) = &path_filter
                && !path.contains(p.as_str())
            {
                continue;
            }
            if let Some(ext) = &ext_filter
                && !path.ends_with(&format!(".{ext}"))
            {
                continue;
            }
            // Resolve project ids to paths once each (cached).
            let repo = match self.project_by_id(hit.project_id) {
                Ok(p) => p,
                Err(_) => continue,
            };
            let line = hit.startline.unwrap_or(1);
            let data = hit.data.clone().unwrap_or_default();
            let match_count = terms
                .iter()
                .filter(|t| data.to_lowercase().contains(&t.to_lowercase()))
                .count() as u32;
            items.push(json!({
                "repo": repo,
                "path": hit.path,
                "sha": "",
                "branch": hit.branch(),
                "line": line,
                "preview": [[line, data]],
                "match_count": match_count,
                "located": true,
            }));
        }
        Ok(json!({ "items": items, "truncated": false }))
    }

    fn project_by_id(&self, id: u64) -> ApiResult<String> {
        let p: crate::api::Project = self.gl.get_json(&format!("/projects/{id}"), &[])?;
        self.cache.put_project(&p.cache_fields());
        Ok(p.path_with_namespace)
    }
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}
