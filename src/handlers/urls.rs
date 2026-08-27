//! URL builders: `repo/web_url` (GitLab's blob/tree grammar),
//! `repo/clone_url` (the http remote), `org/url`.

use super::{Handler, WireResult, w};
use crate::api;
use serde_json::json;

impl Handler {
    pub(super) fn clone_url(&self, repo: &str) -> WireResult {
        w(
            self.project(repo),
            |p| json!({ "clone_url": p.http_url_to_repo }),
        )
    }

    pub(super) fn web_url(
        &self,
        repo: &str,
        path: &str,
        branch: &str,
        line: Option<u64>,
        is_file: bool,
    ) -> WireResult {
        w(self.project(repo), |p| {
            let mut url = p.web();
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

    pub(super) fn org_url(&self, org: &str) -> WireResult {
        w(self.gl.group(org), |g| json!({ "url": g.web_url }))
    }
}

#[cfg(test)]
mod tests {
    use crate::handlers::testkit::{ask, mock_project_lookup, result, tempdir, token_env_set};
    use serde_json::json;
    use wiremock::MockServer;

    #[tokio::test]
    async fn web_url_uses_gitlab_blob_grammar_with_line() {
        let server = MockServer::start().await;
        token_env_set();
        mock_project_lookup(&server, 42, "g/r").await;
        let cache = tempdir();
        let file = result(ask(
            &server.uri(),
            &cache,
            "repo/web_url",
            json!({"repo": "g/r", "path": "src/main.rs", "branch": "main", "line": 42, "is_file": true}),
        ));
        assert_eq!(
            file["url"],
            "https://gitlab.example.com/g/r/-/blob/main/src/main.rs#L42"
        );
        let dir = result(ask(
            &server.uri(),
            &cache,
            "repo/web_url",
            json!({"repo": "g/r", "path": "src", "branch": "main", "line": null, "is_file": false}),
        ));
        assert_eq!(dir["url"], "https://gitlab.example.com/g/r/-/tree/main/src");
    }

    #[tokio::test]
    async fn clone_url_uses_the_http_remote() {
        let server = MockServer::start().await;
        token_env_set();
        mock_project_lookup(&server, 42, "g/r").await;
        let cache = tempdir();
        let r = result(ask(
            &server.uri(),
            &cache,
            "repo/clone_url",
            json!({"repo": "g/r"}),
        ));
        assert_eq!(r["clone_url"], "https://gitlab.example.com/g/r.git");
    }
}
