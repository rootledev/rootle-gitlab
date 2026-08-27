//! `repo/tree`: branch-head revalidation, keyset page aggregation up
//! to the entry cap, and the sha-keyed immutable tree cache.

use super::{ApiError, Handler, WireError, WireResult};
use crate::api::TREE_ENTRY_CAP;
use serde_json::{Value, json};

impl Handler {
    pub(super) fn repo_tree(&self, repo: &str, ref_: Option<&str>) -> WireResult {
        let project = self.project(repo).map_err(|e| WireError::from_api(&e))?;
        // v1.5 revision awareness: an explicit ref resolves to its
        // commit sha (the cache key — ref→sha is the one mutable
        // mapping, revalidated per call); pages are then fetched at
        // that sha so served content and cache key always agree. The
        // default branch flows through the same head lookup as ever.
        let (label, served_ref) = match ref_ {
            None => {
                let branch = project.branch();
                let head = match self.gl.branch_head(project.id, &branch) {
                    Ok(h) => h,
                    Err(e) => return Err(WireError::from_api(&e)),
                };
                (branch, head)
            }
            Some(r) => (r.to_string(), self.resolve_ref(&project, r)?),
        };
        let cached = self.cache.read().tree(&served_ref);
        if let Some(cached) = cached
            && let Ok(mut v) = serde_json::from_slice::<Value>(&cached)
        {
            // The cached body names whatever ref stored it first; the
            // reply names what THIS call served.
            v["branch"] = json!(label);
            return Ok(v);
        }
        let mut entries = Vec::new();
        let mut truncated = false;
        let mut page = 1u32;
        loop {
            let batch = match self.gl.tree_page(project.id, &served_ref, page) {
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
            "branch": label,
        });
        self.cache
            .read()
            .put_tree(&served_ref, body.to_string().as_bytes());
        Ok(body)
    }

    /// Ref → commit sha. Sha-shaped strings pass through untouched
    /// (one string test, zero lookups — GitLab's tree endpoint takes
    /// commit shas as refs); names resolve through their endpoints,
    /// a 404 on one falling through to the other; anything else is an
    /// honest not_found.
    fn resolve_ref(&self, project: &crate::api::Project, r: &str) -> Result<String, WireError> {
        if is_commit_sha(r) {
            return Ok(r.to_string());
        }
        match self.gl.branch_head(project.id, r) {
            Ok(sha) => return Ok(sha),
            Err(ApiError::Api { status: 404, .. }) => {} // not a branch — try tag
            Err(e) => return Err(WireError::from_api(&e)),
        }
        match self.gl.tag_commit(project.id, r) {
            Ok(sha) => Ok(sha),
            Err(ApiError::Api { status: 404, .. }) => Err(WireError {
                kind: "not_found",
                message: format!("no ref {r:?} in {}", project.path_with_namespace),
                retry_after_s: None,
            }),
            Err(e) => Err(WireError::from_api(&e)),
        }
    }
}

/// 40- or 64-hex — commit-sha shaped (sha1 repos today, sha256 repos
/// exist). Anything else must resolve by name.
fn is_commit_sha(s: &str) -> bool {
    let n = s.len();
    (n == 40 || n == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use crate::handlers::testkit::{
        ask, error, mock_project_lookup, result, tempdir, token_env_set,
    };
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

    #[tokio::test]
    async fn tree_at_branch_ref_resolves_the_head_and_labels_the_reply() {
        let server = MockServer::start().await;
        token_env_set();
        mock_project_lookup(&server, 42, "g/big").await;
        Mock::given(method("GET"))
            .and(path(
                "/api/v4/projects/42/repository/branches/release%2F2.7",
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"commit": {"id": "cafe"}})),
            )
            .expect(2..) // ref→sha revalidated per call, like the default head
            .mount(&server)
            .await;
        // Pages are fetched at the RESOLVED sha, never the name: the
        // cache key and the served content can't drift apart.
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/42/repository/tree"))
            .and(query_param("ref", "cafe"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"id": "blobX", "type": "blob", "path": "src/rel.rs"}
            ])))
            .expect(1) // cached by sha thereafter
            .mount(&server)
            .await;
        // Same commit is ALSO the default branch's head — one cache
        // entry, two labels.
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/42/repository/branches/main"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"commit": {"id": "cafe"}})),
            )
            .mount(&server)
            .await;

        let cache = tempdir();
        let at_ref = result(ask(
            &server.uri(),
            &cache,
            "repo/tree",
            json!({"repo": "g/big", "ref": "release/2.7"}),
        ));
        assert_eq!(at_ref["branch"], "release/2.7");
        assert_eq!(at_ref["entries"][0]["path"], "src/rel.rs");

        // Second call at the ref: cache hit, ref still revalidated —
        // and the reply names the ref, not whatever stored the tree.
        let again = result(ask(
            &server.uri(),
            &cache,
            "repo/tree",
            json!({"repo": "g/big", "ref": "release/2.7"}),
        ));
        assert_eq!(again["branch"], "release/2.7");

        // Default-branch call at the same commit: same cache entry,
        // relabeled "main".
        let plain = result(ask(
            &server.uri(),
            &cache,
            "repo/tree",
            json!({"repo": "g/big"}),
        ));
        assert_eq!(plain["branch"], "main");
        assert_eq!(plain["entries"][0]["path"], "src/rel.rs");
    }

    #[tokio::test]
    async fn tree_at_sha_shaped_ref_skips_name_resolution() {
        let server = MockServer::start().await;
        token_env_set();
        mock_project_lookup(&server, 42, "g/big").await;
        // No branch/tag mocks exist: any name resolution would hit
        // the unmocked server and 404 — passing proves none happens.
        let sha = "0123456789abcdef0123456789abcdef01234567";
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/42/repository/tree"))
            .and(query_param("ref", sha))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"id": "blobY", "type": "blob", "path": "src/at-commit.rs"}
            ])))
            .expect(1)
            .mount(&server)
            .await;

        let r = result(ask(
            &server.uri(),
            &tempdir(),
            "repo/tree",
            json!({"repo": "g/big", "ref": sha}),
        ));
        assert_eq!(r["branch"], sha, "the reply names what was served");
        assert_eq!(r["entries"][0]["path"], "src/at-commit.rs");
    }

    #[tokio::test]
    async fn tree_at_unknown_ref_is_not_found() {
        let server = MockServer::start().await;
        token_env_set();
        mock_project_lookup(&server, 42, "g/big").await;
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/42/repository/branches/nope"))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(json!({"message": "404 Branch Not Found"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/42/repository/tags/nope"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(json!({"message": "404 Tag Not Found"})),
            )
            .mount(&server)
            .await;

        let e = error(ask(
            &server.uri(),
            &tempdir(),
            "repo/tree",
            json!({"repo": "g/big", "ref": "nope"}),
        ));
        assert_eq!(e["data"]["kind"], "not_found");
        assert!(e["message"].as_str().unwrap().contains("nope"));
    }
}
