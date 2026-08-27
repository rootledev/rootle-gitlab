//! forge-conformance harness (rootledev/forge-conformance, plans/0015):
//! serve the canonical fixture through a local GitLab REST v4 mock and
//! run the real adapter against it on stdio.
//!
//! The suite spawns this binary as PROVIDER with the materialized
//! fixture directory appended as the final argv element. We bind an
//! ephemeral loopback port, back it with the fixture bytes, and point
//! the ordinary `Handler` at it — the wire the suite exercises is the
//! wire production speaks against gitlab.com.
//!
//! Content ids are git-style blob sha1s computed fresh from disk, so
//! they are stable across respawns (FC-013), move when content moves
//! (FC-011), and never collide across different content (FC-012) —
//! the §Content ids contract, by construction. Credentials are
//! satisfied by an env var we set ourselves (the mock never checks
//! it), so the suite's scrubbed and hermetic environments still work;
//! the adapter still reads it lazily, exactly like production.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rootle_gitlab::{Handler, serve_stdio};
use serde_json::{Value, json};

/// The token var this harness provides. Deliberately NOT one of the
/// names forge-conformance scrubs (GITLAB_TOKEN, FORGE_TOKEN, …): we
/// set it ourselves so it survives even the suite's hermetic env.
const TOKEN_ENV: &str = "FORGE_GITLAB_FC_TOKEN";

/// The suite's lifecycle group roots its cache *inside* the fixture
/// copy; it is bookkeeping, not a repo (the repo set is snapshotted at
/// spawn, before any initialize could create it in generation 1).
const SKIP_DIRS: [&str; 3] = ["cache", ".git", "__pycache__"];

fn main() {
    let fixture: PathBuf = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("FORGE_FIXTURE_DIR").map(PathBuf::from))
        .unwrap_or_else(|| {
            eprintln!("forge: usage: forge <fixture-dir> (or FORGE_FIXTURE_DIR)");
            std::process::exit(2);
        });
    let org = std::env::var("FORGE_ORG").unwrap_or_else(|_| "local".to_string());

    // Snapshot the repo set at spawn: dirs of the fixture root.
    let mut names: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&fixture).expect("read fixture dir") {
        let Ok(e) = entry else { continue };
        if e.path().is_dir()
            && let Some(n) = e.file_name().to_str()
            && !SKIP_DIRS.contains(&n)
        {
            names.push(n.to_string());
        }
    }
    names.sort();
    assert!(!names.is_empty(), "fixture {fixture:?} holds no repos");
    let repos: Vec<Repo> = names
        .into_iter()
        .enumerate()
        .map(|(i, name)| Repo {
            dir: fixture.join(&name),
            name,
            id: (i + 1) as u64,
        })
        .collect();

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    let state = Arc::new(ServerState {
        org,
        group_id: 4242,
        repos,
    });
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(s) => state.serve(s),
                Err(_) => break,
            }
        }
    });

    // Lazy credentials, satisfied locally (the mock never validates).
    // SAFETY: single-threaded setup, before the handler exists.
    unsafe { std::env::set_var(TOKEN_ENV, "forge-conformance-fixture") };
    let handler = Handler::new(&format!("http://127.0.0.1:{port}"), TOKEN_ENV, None);

    // The stdin loop, shared with src/main.rs — v1.3 progressive
    // results stream through it exactly as production would.
    serve_stdio(&handler);
}

// ---------------------------------------------------------------------
// The mock GitLab: repos, trees, blobs, search — all computed fresh
// from the fixture bytes on every request (FC-011 mutates files).
// ---------------------------------------------------------------------

struct Repo {
    dir: PathBuf,
    name: String,
    id: u64,
}

struct ServerState {
    org: String,
    group_id: u64,
    repos: Vec<Repo>,
}

type Query = BTreeMap<String, String>;

impl ServerState {
    fn serve(self: &Arc<Self>, mut stream: TcpStream) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    let done = buf.windows(4).position(|w| w == b"\r\n\r\n").is_some();
                    if done || buf.len() > 64 * 1024 {
                        break;
                    }
                }
            }
        }
        let head = String::from_utf8_lossy(&buf).into_owned();
        let mut parts = head.split_whitespace();
        let method = parts.next().unwrap_or_default();
        let target = parts.next().unwrap_or_default();
        if method != "GET" {
            let body = br#"{"message":"405 method not allowed"}"#.to_vec();
            write_reply(stream, 405, "application/json", &body);
            return;
        }
        let (raw_path, raw_query) = match target.split_once('?') {
            Some((p, q)) => (p, q),
            None => (target, ""),
        };
        let mut query = Query::new();
        for pair in raw_query.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                query.insert(percent_decode(k), percent_decode(v));
            }
        }
        // GitLab addresses projects by whole-path encoding
        // (`group%2Fproject`) — decode per segment, after splitting,
        // or the slash comes back and splits the route.
        let segs: Vec<String> = raw_path
            .trim_matches('/')
            .split('/')
            .map(percent_decode)
            .collect();
        let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
        let (status, ctype, body) = self.route(&segs, &query);
        write_reply(stream, status, ctype, &body);
    }

    fn route(&self, segs: &[&str], q: &Query) -> (u16, &'static str, Vec<u8>) {
        match segs {
            ["api", "v4", "projects"] => self.json(200, self.search_projects(q)),
            ["api", "v4", "groups"] => self.json(200, self.search_groups(q)),
            ["api", "v4", "search"] => self.json(
                200,
                self.search_blobs(self.repos.iter().collect::<Vec<_>>().as_slice(), q),
            ),
            ["api", "v4", "projects", p] => match self.project(p) {
                Some(v) => self.json(200, v),
                None => self.not_found("Project"),
            },
            ["api", "v4", "groups", g] => match self.group(g) {
                Some(v) => self.json(200, v),
                None => self.not_found("Group"),
            },
            ["api", "v4", "groups", g, "projects"] => match self.group(g) {
                Some(_) => {
                    let list: Vec<Value> =
                        self.repos.iter().map(|r| self.project_json(r)).collect();
                    self.json(200, Value::Array(list))
                }
                None => self.not_found("Group"),
            },
            ["api", "v4", "groups", g, "search"] => match self.group(g) {
                Some(_) => self.json(
                    200,
                    self.search_blobs(self.repos.iter().collect::<Vec<_>>().as_slice(), q),
                ),
                None => self.not_found("Group"),
            },
            ["api", "v4", "projects", p, "search"] => match self.project(p) {
                Some(v) => {
                    let id = v["id"].as_u64().expect("project id");
                    let scoped: Vec<&Repo> = self.repos.iter().filter(|r| r.id == id).collect();
                    self.json(200, self.search_blobs(&scoped, q))
                }
                None => self.not_found("Project"),
            },
            ["api", "v4", "projects", p, "repository", "tree"] => match self.project(p) {
                Some(v) => {
                    let id = v["id"].as_u64().expect("project id");
                    match self.repos.iter().find(|r| r.id == id) {
                        Some(r) => {
                            let mut entries = Vec::new();
                            walk_tree(&r.dir, "", &mut entries);
                            self.json(200, Value::Array(entries))
                        }
                        None => self.not_found("Project"),
                    }
                }
                None => self.not_found("Project"),
            },
            ["api", "v4", "projects", p, "repository", "branches", b] => match self.project(p) {
                Some(v) => {
                    let id = v["id"].as_u64().expect("project id");
                    match self.repos.iter().find(|r| r.id == id) {
                        Some(r) => {
                            let mut entries = Vec::new();
                            let root = walk_tree(&r.dir, "", &mut entries);
                            let head = hex(&sha1(
                                format!("commit\n{root}\n{}", percent_decode(b)).as_bytes(),
                            ));
                            self.json(200, json!({ "commit": { "id": head } }))
                        }
                        None => self.not_found("Project"),
                    }
                }
                None => self.not_found("Project"),
            },
            [
                "api",
                "v4",
                "projects",
                p,
                "repository",
                "blobs",
                sha,
                "raw",
            ] => match self.project(p) {
                Some(v) => {
                    let id = v["id"].as_u64().expect("project id");
                    match self
                        .repos
                        .iter()
                        .find(|r| r.id == id)
                        .and_then(|r| blob_by_sha(&r.dir, sha))
                    {
                        Some(bytes) => (200, "application/octet-stream", bytes),
                        None => self.not_found("Blob"),
                    }
                }
                None => self.not_found("Project"),
            },
            _ => self.not_found("Route"),
        }
    }
    /// Project lookup: by numeric id or by full "{org}/{name}" path.
    fn project(&self, p: &str) -> Option<Value> {
        self.repos
            .iter()
            .find(|r| p == r.id.to_string() || p == format!("{}/{}", self.org, r.name))
            .map(|r| self.project_json(r))
    }

    fn group(&self, g: &str) -> Option<Value> {
        if g == self.org || g == self.group_id.to_string() {
            Some(json!({
                "id": self.group_id,
                "full_path": self.org,
                "web_url": format!("http://forge.local/{}", self.org),
            }))
        } else {
            None
        }
    }

    fn project_json(&self, r: &Repo) -> Value {
        json!({
            "id": r.id,
            "path_with_namespace": format!("{}/{}", self.org, r.name),
            "default_branch": "main",
            "web_url": format!("http://forge.local/{}/{}", self.org, r.name),
            "http_url_to_repo": format!("http://forge.local/{}/{}.git", self.org, r.name),
        })
    }

    fn search_projects(&self, q: &Query) -> Value {
        let needle = q.get("search").cloned().unwrap_or_default().to_lowercase();
        let list: Vec<Value> = self
            .repos
            .iter()
            .filter(|r| r.name.to_lowercase().contains(&needle))
            .map(|r| self.project_json(r))
            .collect();
        Value::Array(list)
    }

    fn search_groups(&self, q: &Query) -> Value {
        let needle = q.get("search").cloned().unwrap_or_default().to_lowercase();
        let list: Vec<Value> = if self.org.to_lowercase().contains(&needle) {
            vec![self.group(&self.org).expect("org group")]
        } else {
            Vec::new()
        };
        Value::Array(list)
    }

    /// Blob search (scope=blobs): every text file whose content
    /// contains ALL terms (case-insensitive); binaries are skipped by
    /// NUL sniff, exactly like the reference adapter. Empty terms
    /// match every text file — the path-only hits FC-030 elicits.
    /// `startline` is the real 1-based line of the first match
    /// (FC-031); `data` is that line.
    fn search_blobs(&self, scope: &[&Repo], q: &Query) -> Value {
        let terms: Vec<String> = q
            .get("search")
            .cloned()
            .unwrap_or_default()
            .split_whitespace()
            .map(|t| t.to_lowercase())
            .collect();
        let mut hits = Vec::new();
        for r in scope {
            for (rel, bytes) in files_under(&r.dir) {
                if bytes.iter().take(8192).any(|&b| b == 0) {
                    continue; // binary
                }
                let text = String::from_utf8_lossy(&bytes);
                let lower = text.to_lowercase();
                if !terms.is_empty() && !terms.iter().all(|t| lower.contains(t)) {
                    continue;
                }
                let (line, data) = match terms.first() {
                    Some(t) => text
                        .lines()
                        .enumerate()
                        .find(|(_, l)| l.to_lowercase().contains(t.as_str()))
                        .map(|(i, l)| ((i + 1) as u32, l.to_string()))
                        .unwrap_or((1, String::new())),
                    None => (1, text.lines().next().unwrap_or_default().to_string()),
                };
                hits.push(json!({
                    "project_id": r.id,
                    "path": rel,
                    "ref": "main",
                    "startline": line,
                    "data": data,
                }));
            }
        }
        Value::Array(hits)
    }

    fn json(&self, status: u16, v: Value) -> (u16, &'static str, Vec<u8>) {
        (status, "application/json", v.to_string().into_bytes())
    }

    fn not_found(&self, what: &str) -> (u16, &'static str, Vec<u8>) {
        (
            404,
            "application/json",
            json!({ "message": format!("404 {what} Not Found") })
                .to_string()
                .into_bytes(),
        )
    }
}

fn write_reply(mut stream: TcpStream, status: u16, ctype: &str, body: &[u8]) {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "OK",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

/// Every file under `dir` as (repo-relative path, bytes), fresh from
/// disk, sorted by path.
fn files_under(dir: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(dir: &Path, prefix: &str, out: &mut Vec<(String, Vec<u8>)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut names: Vec<(String, PathBuf, bool)> = entries
            .flatten()
            .map(|e| {
                let is_dir = e.path().is_dir();
                (
                    e.file_name().to_string_lossy().into_owned(),
                    e.path(),
                    is_dir,
                )
            })
            .collect();
        names.sort();
        for (name, path, is_dir) in names {
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if is_dir {
                walk(&path, &rel, out);
            } else if let Ok(bytes) = std::fs::read(&path) {
                out.push((rel, bytes));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, "", &mut out);
    out
}

/// Recursive tree walk: blobs carry their git-style blob sha1, trees a
/// sha1 over their sorted children — ids change exactly when content
/// changes. Returns the root tree id (the branch head is derived from
/// it, so any mutation moves the head and busts the adapter's
/// content-keyed cache precisely).
fn walk_tree(dir: &Path, prefix: &str, out: &mut Vec<Value>) -> String {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return String::new();
    };
    let mut names: Vec<(String, PathBuf, bool)> = entries
        .flatten()
        .map(|e| {
            let is_dir = e.path().is_dir();
            (
                e.file_name().to_string_lossy().into_owned(),
                e.path(),
                is_dir,
            )
        })
        .collect();
    names.sort();
    let mut lines = Vec::new();
    for (name, path, is_dir) in names {
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if is_dir {
            let child = walk_tree(&path, &rel, out);
            out.push(json!({ "id": child, "type": "tree", "path": rel }));
            lines.push(format!("tree {name} {child}"));
        } else {
            let id = git_blob_id(&std::fs::read(&path).unwrap_or_default());
            out.push(json!({ "id": id, "type": "blob", "path": rel }));
            lines.push(format!("blob {name} {id}"));
        }
    }
    hex(&sha1(format!("tree\n{}\n", lines.join("\n")).as_bytes()))
}

fn blob_by_sha(dir: &Path, sha: &str) -> Option<Vec<u8>> {
    files_under(dir)
        .into_iter()
        .find(|(_, bytes)| git_blob_id(bytes) == sha)
        .map(|(_, bytes)| bytes)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hi = (b[i + 1] as char).to_digit(16);
            let lo = (b[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------
// Content ids: git-style blob sha1 ("blob <len>\0<bytes>") — the
// scheme gitlab itself uses, deterministic across processes.
// ---------------------------------------------------------------------

fn git_blob_id(data: &[u8]) -> String {
    let mut buf = format!("blob {}\0", data.len()).into_bytes();
    buf.extend_from_slice(data);
    hex(&sha1(&buf))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// SHA-1 (FIPS 180-1) — stdlib-only, verified against hashlib.
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let bitlen = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bitlen.to_be_bytes());
    for c in 0..msg.len() / 64 {
        let chunk = &msg[c * 64..c * 64 + 64];
        let mut w = [0u32; 80];
        for i in 0..16 {
            let b = &chunk[i * 4..i * 4 + 4];
            w[i] = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for (i, v) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}
