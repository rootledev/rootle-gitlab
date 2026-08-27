//! `repo/blob`: sha-keyed immutable blob cache, the 1 MiB preview
//! cap, and base64 transport.

use super::{Handler, WireError, WireResult};
use crate::api;
use serde_json::json;

impl Handler {
    pub(super) fn repo_blob(&self, repo: &str, sha: &str) -> WireResult {
        let cached = self.cache.read().blob(sha);
        if let Some(bytes) = cached {
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
        self.cache.read().put_blob(sha, &bytes);
        Ok(json!({ "bytes_b64": b64(&bytes) }))
    }

    /// `repo/blob_at` (v1.5): bytes + content id for a path at a ref
    /// — the "open the file at this commit" call. GitLab's raw-files
    /// endpoint hands back bytes only, so the sha is the git blob id
    /// of those bytes (`api::git_blob_sha`) — the same id family the
    /// tree listings carry, and servable by `repo/blob` afterwards
    /// (the bytes land in the sha-keyed cache right here).
    pub(super) fn repo_blob_at(&self, repo: &str, path: &str, ref_: Option<&str>) -> WireResult {
        let project = self.project(repo).map_err(|e| WireError::from_api(&e))?;
        let ref_name = match ref_ {
            Some(r) => r.to_string(),
            None => project.branch(),
        };
        let bytes = self
            .gl
            .raw_file_at(project.id, path, &ref_name)
            .map_err(|e| WireError::from_api(&e))?;
        if bytes.len() > api::BLOB_CAP {
            return Err(WireError {
                kind: "provider",
                message: format!(
                    "{path} at {ref_name} is {} KiB — over the 1 MiB preview cap",
                    bytes.len() / 1024
                ),
                retry_after_s: None,
            });
        }
        let sha = api::git_blob_sha(&bytes);
        self.cache.read().put_blob(&sha, &bytes);
        Ok(json!({ "bytes_b64": b64(&bytes), "sha": sha }))
    }
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use crate::handlers::testkit::{
        ask, error, mock_project_lookup, result, tempdir, token_env_set,
    };
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn base64_decode(s: &str) -> Vec<u8> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.decode(s).unwrap()
    }

    #[tokio::test]
    async fn blob_roundtrips_b64_and_caches() {
        let server = MockServer::start().await;
        token_env_set();
        mock_project_lookup(&server, 42, "g/r").await;
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/42/repository/blobs/abc123/raw"))
            .respond_with(ResponseTemplate::new(200).set_body_string("fn main() {}\n"))
            .expect(1)
            .mount(&server)
            .await;
        let cache = tempdir();
        let r1 = result(ask(
            &server.uri(),
            &cache,
            "repo/blob",
            json!({"repo": "g/r", "sha": "abc123"}),
        ));
        let b64 = r1["bytes_b64"].as_str().unwrap();
        assert_eq!(base64_decode(b64), b"fn main() {}\n");
        // Cache hit: mock was expect(1).
        let r2 = result(ask(
            &server.uri(),
            &cache,
            "repo/blob",
            json!({"repo": "g/r", "sha": "abc123"}),
        ));
        assert_eq!(r2["bytes_b64"], r1["bytes_b64"]);
    }

    #[tokio::test]
    async fn oversized_blob_is_refused_with_the_cap_message() {
        let server = MockServer::start().await;
        token_env_set();
        mock_project_lookup(&server, 42, "g/r").await;
        let big = vec![b'x'; 1024 * 1024 + 1];
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/42/repository/blobs/big/raw"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(big))
            .mount(&server)
            .await;
        let cache = tempdir();
        let e = error(ask(
            &server.uri(),
            &cache,
            "repo/blob",
            json!({"repo": "g/r", "sha": "big"}),
        ));
        assert_eq!(e["data"]["kind"], "provider");
        assert!(e["message"].as_str().unwrap().contains("1 MiB preview cap"));
    }

    #[tokio::test]
    async fn blob_at_serves_bytes_with_a_git_blob_sha() {
        let server = MockServer::start().await;
        token_env_set();
        mock_project_lookup(&server, 42, "g/r").await;
        Mock::given(method("GET"))
            .and(path(
                "/api/v4/projects/42/repository/files/src%2Fmain.rs/raw",
            ))
            .and(query_param("ref", "main"))
            .respond_with(ResponseTemplate::new(200).set_body_string("fn main() {}\n"))
            .expect(1)
            .mount(&server)
            .await;

        let cache = tempdir();
        let r = result(ask(
            &server.uri(),
            &cache,
            "repo/blob_at",
            json!({"repo": "g/r", "path": "src/main.rs"}),
        ));
        assert_eq!(
            r["sha"], "f328e4d9d04c31d0d70d16d21a07d1613be9d577",
            "git hash-object of the bytes (pinned against real git)"
        );
        assert_eq!(
            base64_decode(r["bytes_b64"].as_str().unwrap()),
            b"fn main() {}\n"
        );

        // The content id is cross-method: repo/blob on that sha serves
        // from the cache with no GitLab blob call (no blob mock is
        // mounted — one would 404 the test).
        let blob = result(ask(
            &server.uri(),
            &cache,
            "repo/blob",
            json!({"repo": "g/r", "sha": "f328e4d9d04c31d0d70d16d21a07d1613be9d577"}),
        ));
        assert_eq!(blob["bytes_b64"], r["bytes_b64"]);
    }

    #[tokio::test]
    async fn blob_at_passes_ref_and_maps_not_found() {
        let server = MockServer::start().await;
        mock_project_lookup(&server, 42, "g/r").await;
        Mock::given(method("GET"))
            .and(path(
                "/api/v4/projects/42/repository/files/src%2Fmain.rs/raw",
            ))
            .and(query_param("ref", "release/2.7")) // wiremock matches decoded values
            .respond_with(ResponseTemplate::new(200).set_body_string("stable\n"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/42/repository/files/gone.rs/raw"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(json!({"message": "404 File Not Found"})),
            )
            .mount(&server)
            .await;

        let cache = tempdir();
        let at_ref = result(ask(
            &server.uri(),
            &cache,
            "repo/blob_at",
            json!({"repo": "g/r", "path": "src/main.rs", "ref": "release/2.7"}),
        ));
        assert_eq!(
            base64_decode(at_ref["bytes_b64"].as_str().unwrap()),
            b"stable\n"
        );
        assert_eq!(at_ref["sha"], "2bf5ad0447d3370461c6f32a0a5bc8a3177376aa");

        let e = error(ask(
            &server.uri(),
            &cache,
            "repo/blob_at",
            json!({"repo": "g/r", "path": "gone.rs", "ref": "main"}),
        ));
        assert_eq!(e["data"]["kind"], "not_found");
    }
}
