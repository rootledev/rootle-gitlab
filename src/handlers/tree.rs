//! `repo/tree`: branch-head revalidation, keyset page aggregation up
//! to the entry cap, and the sha-keyed immutable tree cache.

use super::{Handler, WireError, WireResult};
use crate::api::TREE_ENTRY_CAP;
use serde_json::{Value, json};

impl Handler {
    pub(super) fn repo_tree(&self, repo: &str) -> WireResult {
        let project = self.project(repo).map_err(|e| WireError::from_api(&e))?;
        let branch = project.branch();
        // Branch head first (mutable, one cheap call): a cached tree
        // keyed by the head sha is immutable thereafter.
        let head = match self.gl.branch_head(project.id, &branch) {
            Ok(h) => h,
            Err(e) => return Err(WireError::from_api(&e)),
        };
        let cached = self.cache.read().tree(&head);
        if let Some(cached) = cached
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
        self.cache
            .read()
            .put_tree(&head, body.to_string().as_bytes());
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use crate::handlers::testkit::{ask, mock_project_lookup, result, tempdir, token_env_set};
    use serde_json::{Value, json};
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn tree_aggregates_pages_and_caches_by_head_sha() {
        let server = MockServer::start().await;
        token_env_set();
        mock_project_lookup(&server, 42, "g/big").await;
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/42/repository/branches/main"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"commit": {"id": "deadbeef"}})),
            )
            .expect(2..) // head revalidated on every tree call
            .mount(&server)
            .await;
        let page1: Vec<Value> = (0..100)
            .map(|i| json!({"id": format!("blob{i:03}"), "type": "blob", "path": format!("src/f{i:03}.rs")}))
            .collect();
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/42/repository/tree"))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page1))
            .expect(1) // the cache means page 1 is fetched ONCE
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/42/repository/tree"))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"id": "sha-last", "type": "tree", "path": "docs"}
            ])))
            .expect(1)
            .mount(&server)
            .await;

        let cache = tempdir();
        let first = result(ask(
            &server.uri(),
            &cache,
            "repo/tree",
            json!({"repo": "g/big"}),
        ));
        assert_eq!(first["entries"].as_array().unwrap().len(), 101);
        assert_eq!(first["truncated"], false);
        assert_eq!(first["branch"], "main");
        assert_eq!(first["entries"][100]["type"], "tree");

        // Second call: served from the cache (page mocks are expect(1)).
        let second = result(ask(
            &server.uri(),
            &cache,
            "repo/tree",
            json!({"repo": "g/big"}),
        ));
        assert_eq!(second["entries"].as_array().unwrap().len(), 101);
    }
}
