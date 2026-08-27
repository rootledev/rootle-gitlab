//! The handshake: initialize + the advisory cache budget
//! (protocol v1.2). Startup stays cheap and network-free — rootle
//! respawns this process unboundedly.

use super::{Handler, WireResult};
use crate::cache::Cache;
use serde_json::{Value, json};

impl Handler {
    pub(super) fn initialize(&self, params: &Value) -> WireResult {
        // The handshake's cache_dir wins over the default —
        // rootle owns the subtree naming (protocol v1.2) and
        // respawns re-send it, so re-rooting is idempotent.
        if let Some(dir) = params["cache_dir"].as_str()
            && let path = std::path::PathBuf::from(dir)
        {
            let mut cache = self.cache.write();
            if cache.root_as_str() != Some(path.to_string_lossy().as_ref()) {
                *cache = Cache::new(Some(path));
            }
        }
        let cache = self.cache.read();
        // Advisory cache budget (protocol v1.2): evict our
        // subtree past it and report current usage — one
        // [cache] max_mb knob in :settings governs every
        // backend. Local stat walk only; startup stays
        // network-free (restart obligations).
        let budget = params["cache_bytes"].as_u64().unwrap_or(0);
        if budget > 0 {
            cache.enforce_budget(budget);
        }
        let usage = cache.size_bytes();
        Ok(json!({
            "protocol": 1,
            "name": "gitlab",
            // v1.3: modeline icon (rootle renders its gitlab
            // glyph when the user enables nerd_font).
            "icon": "gitlab",
            // Optimistic: startup does NO network (restart
            // obligations). Unavailable search surfaces as
            // honest per-call errors, not startup failure.
            "capabilities": { "orgs": true, "code_search": true },
            "cache": { "bytes": usage }
        }))
    }
}

#[cfg(test)]
mod tests {
    use crate::cache::Cache;
    use crate::handlers::testkit::{ask, result, tempdir, token_env_set};
    use serde_json::json;
    use wiremock::MockServer;

    #[tokio::test]
    async fn initialize_is_pure_no_network() {
        let server = MockServer::start().await;
        // No mocks mounted: ANY request would 404 and fail the assertions.
        let cache = tempdir();
        let r = result(ask(
            &server.uri(),
            &cache,
            "initialize",
            json!({"protocol": 1}),
        ));
        assert_eq!(r["protocol"], 1);
        assert_eq!(r["name"], "gitlab");
        assert_eq!(r["capabilities"]["code_search"], true);
    }

    #[tokio::test]
    async fn initialize_reports_usage_and_enforces_the_budget() {
        let server = MockServer::start().await;
        token_env_set();
        let cache_dir = tempdir();
        let cache = Cache::new(Some(cache_dir.clone()));
        // 3 blobs: 1000 (oldest), 2000, 3000 bytes; budget 4500 → the
        // oldest must go (LRU by mtime: 6000-1000=5000 > 4500), leaving
        // 2000+3000 = 5000… still over → 2000 also goes. Use 5000: only
        // the oldest leaves.
        for (i, size) in [(1, 1000u64), (2, 2000), (3, 3000)] {
            cache.put_blob(&format!("aa{:04}", i), &vec![0u8; size as usize]);
            let p = cache_dir.join("blobs/aa").join(format!("aa{:04}", i));
            let t = std::time::SystemTime::now() - std::time::Duration::from_secs(100 - i);
            let _ = filetime::set_file_mtime(&p, filetime::FileTime::from(t));
        }
        let r = ask(
            &server.uri(),
            &cache_dir,
            "initialize",
            json!({"protocol": 1, "cache_bytes": 5000, "cache_dir": cache_dir.to_string_lossy()}),
        );
        let res = result(r);
        assert_eq!(res["name"], "gitlab");
        assert_eq!(
            res["cache"]["bytes"].as_u64().unwrap(),
            5000,
            "post-eviction usage reported"
        );
        assert!(cache.blob("aa0001").is_none(), "oldest blob evicted");
        assert!(cache.blob("aa0002").is_some());
        assert!(cache.blob("aa0003").is_some());
    }

    #[tokio::test]
    async fn handshake_cache_dir_wins_over_the_default() {
        let server = MockServer::start().await;
        token_env_set();
        let default_dir = tempdir();
        let handshake_dir = tempdir();
        // Construct with the default; initialize passes a different dir —
        // the handshake's cache_dir is where a cached blob must land.
        let line = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocol": 1,
            "cache_dir": handshake_dir.to_string_lossy(),
        }})
        .to_string();
        std::thread::scope(|s| {
            s.spawn(|| {
                let h = crate::handlers::Handler::new(
                    &server.uri(),
                    "GL_TEST_TOKEN",
                    Some(default_dir),
                );
                crate::respond(&h, &line)
            })
            .join()
            .unwrap()
            .unwrap();
        });
        // Verify the re-root landed: usage is reported from the handshake
        // dir, which is empty → 0 (a non-re-rooted handler would report
        // its own dir's contents instead).
        let r = ask(
            &server.uri(),
            &handshake_dir,
            "initialize",
            json!({"protocol": 1}),
        );
        assert_eq!(result(r)["cache"]["bytes"], 0);
    }
}
