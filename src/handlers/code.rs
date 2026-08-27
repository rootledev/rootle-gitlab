//! `search/code`: the q grammar (repo:/org:/path:/extension:
//! qualifiers + free terms) translated onto GitLab's blob search,
//! with client-side path/extension filtering.

use super::{Handler, WireError, WireResult};
use crate::api::ApiResult;
use serde_json::json;

impl Handler {
    /// q grammar (the protocol's PROTOCOL SURFACE): repo:/org:/path:/
    /// extension: qualifiers + free terms. repo: and org: scope the
    /// GitLab search endpoint server-side; path:/extension: filter
    /// client-side (no server equivalent). Hits carry GitLab's real
    /// startline — located, no client-side locating needed.
    pub(super) fn search_code(&self, q: &str) -> WireResult {
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
        self.cache.read().put_project(&p);
        Ok(p.path_with_namespace)
    }
}

#[cfg(test)]
mod tests {
    use crate::handlers::testkit::{
        ask, error, mock_project_lookup, project_json, result, tempdir, token_env_set,
    };
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn code_search_scopes_qualifiers_and_locates() {
        let server = MockServer::start().await;
        token_env_set();
        mock_project_lookup(&server, 42, "g/r").await;
        // search hits resolve project_id → path through /projects/{id}.
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(project_json(42, "g/r")))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/42/search"))
            .and(query_param("scope", "blobs"))
            .and(query_param("search", "render"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"project_id": 42, "path": "src/lib.rs", "ref": "main",
                 "startline": 12, "data": "pub fn render() {}"},
                {"project_id": 42, "path": "docs/render.md", "ref": "main",
                 "startline": 3, "data": "# render docs"},
                {"project_id": 42, "path": "src/other.rs", "ref": "main",
                 "startline": 9, "data": "pub fn render_thing() {}"}
            ])))
            .mount(&server)
            .await;
        let cache = tempdir();
        // repo: scopes server-side; extension: filters client-side.
        let r = result(ask(
            &server.uri(),
            &cache,
            "search/code",
            json!({"q": "render repo:g/r extension:rs"}),
        ));
        let items = r["items"].as_array().unwrap();
        assert_eq!(items.len(), 2, "the .md hit is filtered by extension:");
        assert_eq!(items[0]["repo"], "g/r");
        assert_eq!(items[0]["path"], "src/lib.rs");
        assert_eq!(items[0]["line"], 12, "GitLab startline is the real line");
        assert_eq!(items[0]["located"], true);
        assert_eq!(items[0]["branch"], "main");
        assert_eq!(items[0]["preview"][0][1], "pub fn render() {}");
        assert_eq!(items[0]["match_count"], 1);
    }

    #[tokio::test]
    async fn code_search_403_is_an_honest_auth_error() {
        let server = MockServer::start().await;
        token_env_set();
        Mock::given(method("GET"))
            .and(path("/api/v4/search"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "message": "403 Forbidden - advanced search not enabled"
            })))
            .mount(&server)
            .await;
        let cache = tempdir();
        let e = error(ask(
            &server.uri(),
            &cache,
            "search/code",
            json!({"q": "anything"}),
        ));
        // Self-managed instances without a license land here: the toast
        // carries the real reason instead of a silent nothing.
        assert_eq!(e["data"]["kind"], "auth");
        assert!(e["message"].as_str().unwrap().contains("advanced search"));
    }
}
