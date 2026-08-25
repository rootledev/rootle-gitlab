//! Offline conformance: every protocol method against a scripted
//! GitLab API (wiremock). No network, deterministic, runs in CI.
//! The mock's URI is the adapter's --instance; tests drive the same
//! `respond()` the binary's stdin loop uses.

use rootle_gitlab::{Handler, cache::Cache, respond};
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// reqwest::blocking's client owns a runtime and may neither be
/// created nor dropped inside a tokio worker — so the handler is
/// constructed, used, and dropped on a plain (scoped) thread. The
/// disk cache (tempdir) carries state across asks.
fn ask(uri: &str, cache: &std::path::Path, method: &str, params: Value) -> Value {
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

fn result(reply: Value) -> Value {
    reply["result"].clone()
}

fn error(reply: Value) -> Value {
    reply["error"].clone()
}

fn tempdir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "rootle-gitlab-test-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn project_json(id: u64, path: &str) -> Value {
    json!({
        "id": id,
        "path_with_namespace": path,
        "default_branch": "main",
        "web_url": format!("https://gitlab.example.com/{path}"),
        "http_url_to_repo": format!("https://gitlab.example.com/{path}.git"),
    })
}

async fn mock_project_lookup(server: &MockServer, id: u64, proj_path: &str) {
    let enc = proj_path.replace('/', "%2F");
    Mock::given(method("GET"))
        .and(path(format!("/api/v4/projects/{enc}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(project_json(id, proj_path)))
        .mount(server)
        .await;
}

/// Set once, process-wide (edition-2024 set_var is unsafe — fine in
/// tests, done exactly here).
fn token_env_set() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        std::env::set_var("GL_TEST_TOKEN", "glpat-test");
    });
}

// ---------------------------------------------------------------------

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
async fn missing_token_is_a_lazy_auth_error() {
    let server = MockServer::start().await;
    // Distinct env name so the global GL_TEST_TOKEN can't leak in.
    let cache = tempdir();
    let line =
        json!({"jsonrpc":"2.0","id":1,"method":"repo/tree","params":{"repo":"g/r"}}).to_string();
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
async fn search_repos_mixes_projects_and_orgs() {
    let server = MockServer::start().await;
    token_env_set();
    Mock::given(method("GET"))
        .and(path("/api/v4/projects"))
        .and(query_param("search", "tool"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            project_json(10, "kit/tool"),
            project_json(11, "other/toolkit"),
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v4/groups"))
        .and(query_param("search", "tool"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 5, "full_path": "tools", "web_url": "u"}
        ])))
        .mount(&server)
        .await;
    let cache = tempdir();
    let r = result(ask(
        &server.uri(),
        &cache,
        "search/repos",
        json!({"query": "tool"}),
    ));
    let items = r["items"].as_array().unwrap();
    assert_eq!(items[0]["full_name"], "kit/tool");
    assert_eq!(items[2]["org"], "tools");
}

#[tokio::test]
async fn org_repos_tolerate_empty_repos() {
    let server = MockServer::start().await;
    token_env_set();
    Mock::given(method("GET"))
        .and(path("/api/v4/groups/tools"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id": 5, "full_path": "tools", "web_url": "u"})),
        )
        .mount(&server)
        .await;
    // An empty repo: default_branch null, urls absent — one bad shape
    // in the page must not sink the listing.
    Mock::given(method("GET"))
        .and(path("/api/v4/groups/5/projects"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 10, "path_with_namespace": "tools/empty", "default_branch": null},
            project_json(11, "tools/full"),
        ])))
        .mount(&server)
        .await;
    let cache = tempdir();
    let r = result(ask(
        &server.uri(),
        &cache,
        "org/repos",
        json!({"org": "tools"}),
    ));
    assert_eq!(r["repos"], json!(["empty", "full"]));
}

#[tokio::test]
async fn org_repos_strip_the_org_component_nested_kept() {
    let server = MockServer::start().await;
    token_env_set();
    Mock::given(method("GET"))
        .and(path("/api/v4/groups/tools"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id": 5, "full_path": "tools", "web_url": "u"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v4/groups/5/projects"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            project_json(10, "tools/alpha"),
            project_json(11, "tools/sub/nested"), // subgroup: multi-slash id
        ])))
        .mount(&server)
        .await;
    let cache = tempdir();
    let r = result(ask(
        &server.uri(),
        &cache,
        "org/repos",
        json!({"org": "tools"}),
    ));
    assert_eq!(r["repos"], json!(["alpha", "sub/nested"]));
}

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
            ResponseTemplate::new(404).set_body_json(json!({"message": "404 Project Not Found"})),
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

#[tokio::test]
async fn notifications_without_an_id_are_ignored() {
    let server = MockServer::start().await;
    let cache = tempdir();
    let line = json!({"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id": 7}}).to_string();
    let reply = std::thread::scope(|s| {
        s.spawn(move || {
            let h = Handler::new(&server.uri(), "GL_TEST_TOKEN", Some(cache));
            respond(&h, &line)
        })
        .join()
        .unwrap()
    });
    assert!(reply.is_none());
}

#[tokio::test]
async fn unknown_method_is_a_provider_error() {
    let server = MockServer::start().await;
    let cache = tempdir();
    let e = error(ask(&server.uri(), &cache, "repo/issues", json!({})));
    assert_eq!(e["data"]["kind"], "provider");
    assert!(e["message"].as_str().unwrap().contains("unknown method"));
}

fn base64_decode(s: &str) -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s).unwrap()
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
