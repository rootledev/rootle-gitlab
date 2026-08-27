//! Protocol method handlers: one function per method, mapping between
//! the wire shapes (doc/provider-protocol.md) and the GitLab API.
//! Handlers are pure request→result — stdin plumbing lives in main.
//!
//! This surface file owns dispatch, the wire error taxonomy, and the
//! helpers shared across methods; the per-method handlers live in
//! sibling submodules (`handlers/`), each beside the wiremock tests
//! that cover it (the house rule).

mod blame;
mod blob;
mod code;
mod initialize;
mod log;
mod refs;
mod search;
mod tree;
mod urls;

use crate::api::{ApiError, ApiResult, GitLab};
use crate::cache::Cache;
use serde_json::{Value, json};

pub struct Handler {
    pub gl: GitLab,
    /// Rooted at the handshake's cache_dir when rootle passes one
    /// (the documented contract); otherwise the XDG default. Interior
    /// mutability because initialize is the first message — &self
    /// throughout.
    pub cache: parking_lot::RwLock<Cache>,
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

/// An optional string param: JSON null and `""` both mean absent
/// (rootle sends null for missing optionals — empty string is never
/// a meaningful ref or path here).
fn opt_str<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params[key].as_str().filter(|s| !s.is_empty())
}

fn w<T>(r: ApiResult<T>, f: impl FnOnce(T) -> Value) -> WireResult {
    r.map(f).map_err(|e| WireError::from_api(&e))
}

impl Handler {
    pub fn new(instance: &str, token_env: &str, cache_base: Option<std::path::PathBuf>) -> Self {
        Handler {
            gl: GitLab::new(instance, token_env),
            cache: parking_lot::RwLock::new(Cache::new(cache_base)),
        }
    }

    pub fn dispatch(&self, method: &str, params: &Value) -> WireResult {
        match method {
            "initialize" => self.initialize(params),
            "search/repos" => self.search_repos(params["query"].as_str().unwrap_or("")),
            "org/repos" => self.org_repos(params["org"].as_str().unwrap_or("")),
            "repo/tree" => self.repo_tree(
                params["repo"].as_str().unwrap_or(""),
                opt_str(params, "ref"),
            ),
            "repo/blob" => self.repo_blob(
                params["repo"].as_str().unwrap_or(""),
                params["sha"].as_str().unwrap_or(""),
            ),
            "repo/blob_at" => self.repo_blob_at(
                params["repo"].as_str().unwrap_or(""),
                params["path"].as_str().unwrap_or(""),
                opt_str(params, "ref"),
            ),
            "repo/refs" => self.repo_refs(params["repo"].as_str().unwrap_or("")),
            "repo/log" => self.repo_log(
                params["repo"].as_str().unwrap_or(""),
                opt_str(params, "path"),
                opt_str(params, "ref"),
                params["limit"].as_u64(),
            ),
            "repo/blame" => self.repo_blame(
                params["repo"].as_str().unwrap_or(""),
                params["path"].as_str().unwrap_or(""),
                opt_str(params, "ref"),
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

    /// The streaming entry (v1.3 progressive results): `search/code`
    /// with `partial: true` emits its batch(es) through `emit` — each
    /// call receives the `$/partial` params object, already carrying
    /// the request id — and answers metadata-only. Everything else
    /// falls through to the one-shot `dispatch` unchanged.
    pub fn dispatch_streaming(
        &self,
        method: &str,
        params: &Value,
        id: &Value,
        emit: &mut dyn FnMut(Value),
    ) -> WireResult {
        if method == "search/code" && params["partial"].as_bool().unwrap_or(false) {
            self.search_code_streamed(params["q"].as_str().unwrap_or(""), id, emit)
        } else {
            self.dispatch(method, params)
        }
    }

    /// Project metadata through the cache; a 404 invalidates a stale
    /// entry once (repo moved/renamed) and retries fresh.
    fn project(&self, path: &str) -> ApiResult<crate::api::Project> {
        let cached = self.cache.read().project(path);
        if let Some(p) = cached {
            return Ok(p);
        }
        match self.gl.project(path) {
            Ok(p) => {
                self.cache.read().put_project(&p);
                Ok(p)
            }
            Err(ApiError::Api { status: 404, .. }) => {
                self.cache.read().drop_project(path);
                self.gl.project(path)
            }
            Err(e) => Err(e),
        }
    }

    fn group_id(&self, org: &str) -> ApiResult<u64> {
        Ok(self.gl.group(org)?.id)
    }
}

/// Shared scaffolding for the wiremock suites living beside the
/// handlers: the offline conformance harness. No network beyond the
/// mock, deterministic, runs in CI. Tests drive the same `respond()`
/// the binary's stdin loop uses.
#[cfg(test)]
pub(crate) mod testkit {
    use crate::handlers::Handler;
    use crate::respond;
    use serde_json::{Value, json};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// reqwest::blocking's client owns a runtime and may neither be
    /// created nor dropped inside a tokio worker — so the handler is
    /// constructed, used, and dropped on a plain (scoped) thread. The
    /// disk cache (tempdir) carries state across asks.
    pub(crate) fn ask(uri: &str, cache: &std::path::Path, method: &str, params: Value) -> Value {
        let line = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}).to_string();
        let reply = std::thread::scope(|s| {
            s.spawn(move || {
                let handler = Handler::new(uri, "GL_TEST_TOKEN", Some(cache.to_path_buf()));
                respond(&handler, &line)
            })
            .join()
            .expect("probe thread")
        })
        .expect("request line must produce a reply");
        serde_json::from_str(&reply).unwrap()
    }

    /// The full stdout transcript for one request line, in order —
    /// streaming requests yield their `$/partial` lines before the
    /// reply. Same probe-thread rules as `ask`.
    pub(crate) fn ask_lines(
        uri: &str,
        cache: &std::path::Path,
        method: &str,
        params: Value,
    ) -> Vec<Value> {
        let line = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}).to_string();
        let lines = std::thread::scope(|s| {
            s.spawn(move || {
                let handler = Handler::new(uri, "GL_TEST_TOKEN", Some(cache.to_path_buf()));
                crate::respond_transcript(&handler, &line)
            })
            .join()
            .expect("probe thread")
        })
        .expect("request line must produce a reply");
        lines
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    pub(crate) fn result(reply: Value) -> Value {
        reply["result"].clone()
    }

    pub(crate) fn error(reply: Value) -> Value {
        reply["error"].clone()
    }

    pub(crate) fn tempdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "rootle-gitlab-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    pub(crate) fn project_json(id: u64, path: &str) -> Value {
        json!({
            "id": id,
            "path_with_namespace": path,
            "default_branch": "main",
            "web_url": format!("https://gitlab.example.com/{path}"),
            "http_url_to_repo": format!("https://gitlab.example.com/{path}.git"),
        })
    }

    pub(crate) async fn mock_project_lookup(server: &MockServer, id: u64, proj_path: &str) {
        let enc = proj_path.replace('/', "%2F");
        Mock::given(method("GET"))
            .and(path(format!("/api/v4/projects/{enc}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(project_json(id, proj_path)))
            .mount(server)
            .await;
    }

    /// Set once, process-wide (edition-2024 set_var is unsafe — fine in
    /// tests, done exactly here).
    pub(crate) fn token_env_set() {
        use std::sync::Once;
        static ONCE: Once = Once::new();
        ONCE.call_once(|| unsafe {
            std::env::set_var("GL_TEST_TOKEN", "glpat-test");
        });
    }
}

#[cfg(test)]
mod tests {
    use super::Handler;
    use crate::handlers::testkit::{ask, error, tempdir, token_env_set};
    use crate::respond;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn missing_token_is_a_lazy_auth_error() {
        let server = MockServer::start().await;
        // Distinct env name so the global GL_TEST_TOKEN can't leak in.
        let cache = tempdir();
        let line = json!({"jsonrpc":"2.0","id":1,"method":"repo/tree","params":{"repo":"g/r"}})
            .to_string();
        let reply = std::thread::scope(|s| {
            s.spawn(move || {
                let h = Handler::new(&server.uri(), "GL_TEST_TOKEN_ABSENT", Some(cache));
                respond(&h, &line)
            })
            .join()
            .unwrap()
            .unwrap()
        });
        let e = error(serde_json::from_str(&reply).unwrap());
        assert_eq!(e["data"]["kind"], "auth");
        assert!(e["message"].as_str().unwrap().contains("GL_TEST_TOKEN"));
    }

    #[tokio::test]
    async fn error_taxonomy_maps_status_to_kinds() {
        let server = MockServer::start().await;
        token_env_set();
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/g%2Fdenied"))
            .respond_with(
                ResponseTemplate::new(401).set_body_json(json!({"message": "401 Unauthorized"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/g%2Fgone"))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(json!({"message": "404 Project Not Found"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/g%2Fthrottled"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("Retry-After", "37")
                    .set_body_json(json!({"message": "429 Too Many Requests"})),
            )
            .mount(&server)
            .await;
        let cache = tempdir();
        assert_eq!(
            error(ask(
                &server.uri(),
                &cache,
                "repo/tree",
                json!({"repo": "g/denied"})
            ))["data"]["kind"],
            "auth"
        );
        assert_eq!(
            error(ask(
                &server.uri(),
                &cache,
                "repo/tree",
                json!({"repo": "g/gone"})
            ))["data"]["kind"],
            "not_found"
        );
        let limited = error(ask(
            &server.uri(),
            &cache,
            "repo/tree",
            json!({"repo": "g/throttled"}),
        ));
        assert_eq!(limited["data"]["kind"], "rate_limited");
        assert_eq!(limited["data"]["retry_after_s"], 37);
    }

    #[tokio::test]
    async fn unknown_method_is_a_provider_error() {
        let server = MockServer::start().await;
        let cache = tempdir();
        let e = error(ask(&server.uri(), &cache, "repo/issues", json!({})));
        assert_eq!(e["data"]["kind"], "provider");
        assert!(e["message"].as_str().unwrap().contains("unknown method"));
    }
}
