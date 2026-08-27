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
    use wiremock::matchers::{method, path};
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
}
