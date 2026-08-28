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
//! Two serving shapes, one per fixture kind:
//!
//! - plain directories (`alpha`, `beta`) are walked from disk — content
//!   ids are git-style blob sha1s (`api::git_blob_sha`) computed fresh
//!   per request, stable across respawns (FC-013), content-keyed by
//!   construction (FC-010..012); the branch head hashes the recursive
//!   tree so a mutation (FC-011) busts the adapter's cache precisely.
//! - `fixture/vcs` is a real git repo (the v1.5 revision fixture): its
//!   answers come from git itself (`ls-tree`, `rev-parse`, `log`,
//!   `show`, `blame --porcelain`), so served refs, logs, and blame
//!   match the expectations the suite derives from the same repo —
//!   ids included, since git blob ids ARE the content-id scheme.
//!
//! Credentials are satisfied by an env var we set ourselves (the mock
//! never checks it), so the suite's scrubbed and hermetic environments
//! still work; the adapter still reads it lazily, like production.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use rootle_gitlab::{Handler, api::git_blob_sha, serve_stdio};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};

/// The token var this harness provides. Deliberately NOT one of the
/// names forge-conformance scrubs (GITLAB_TOKEN, FORGE_TOKEN, …): we
/// set it ourselves so it survives even the suite's hermetic env.
const TOKEN_ENV: &str = "FORGE_GITLAB_FC_TOKEN";

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

    // Snapshot the repo set at spawn: every dir of the fixture root is
    // a repo (the suite roots adapter caches BESIDE the fixture, never
    // inside it — pinned suite revision v1.5.0).
    let mut names: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&fixture).expect("read fixture dir") {
        let Ok(e) = entry else { continue };
        if e.path().is_dir()
            && let Some(n) = e.file_name().to_str()
        {
            names.push(n.to_string());
        }
    }
    names.sort();
    assert!(!names.is_empty(), "fixture {fixture:?} holds no repos");
    let repos: Vec<Repo> = names
        .into_iter()
        .enumerate()
        .map(|(i, name)| {
            let dir = fixture.join(&name);
            let is_git = dir.join(".git").exists();
            Repo {
                dir,
                name,
                id: (i + 1) as u64,
                is_git,
            }
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
// The mock GitLab: projects, trees, blobs, search, and (for the git
// fixture) the v1.5 revision surface — computed fresh per request.
// ---------------------------------------------------------------------

struct Repo {
    dir: PathBuf,
    name: String,
    id: u64,
    /// A real git repo (fixture/vcs): refs, trees, logs, and blame are
    /// answered by git itself.
    is_git: bool,
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
        // GitLab addresses projects and file paths by whole-path
        // encoding (`group%2Fproject`, `src%2Fmain.rs`) — decode per
        // segment, after splitting, or the slashes come back and split
        // the route.
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
            ["api", "v4", "projects", p, "repository", "branches"] => match self.project(p) {
                Some(v) => self.json(200, self.branches(self.repo_of(&v))),
                None => self.not_found("Project"),
            },
            ["api", "v4", "projects", p, "repository", "branches", b] => match self.project(p) {
                Some(v) => match self.branch_head(self.repo_of(&v), b) {
                    Some(sha) => self.json(200, json!({ "name": b, "commit": { "id": sha } })),
                    None => self.not_found("Branch"),
                },
                None => self.not_found("Project"),
            },
            ["api", "v4", "projects", p, "repository", "tags"] => match self.project(p) {
                Some(v) => self.json(200, self.tags(self.repo_of(&v))),
                None => self.not_found("Project"),
            },
            ["api", "v4", "projects", p, "repository", "tags", t] => match self.project(p) {
                Some(v) => match self.tag_commit(self.repo_of(&v), t) {
                    Some(sha) => self.json(200, json!({ "name": t, "commit": { "id": sha } })),
                    None => self.not_found("Tag"),
                },
                None => self.not_found("Project"),
            },
            ["api", "v4", "projects", p, "repository", "commits"] => match self.project(p) {
                Some(v) => self.json(200, self.commit_log(self.repo_of(&v), q)),
                None => self.not_found("Project"),
            },
            ["api", "v4", "projects", p, "repository", "tree"] => match self.project(p) {
                Some(v) => {
                    let repo = self.repo_of(&v);
                    let entries = match repo.is_git {
                        // The ref arrives resolved to a commit sha (the
                        // adapter resolves names first); git answers,
                        // unknown shas 404.
                        true => match q.get("ref").map(String::as_str) {
                            Some(r) => match git_ls_tree(&repo.dir, r) {
                                Some(e) => e,
                                None => return self.not_found("Ref"),
                            },
                            None => Vec::new(),
                        },
                        false => {
                            let mut e = Vec::new();
                            walk_tree(&repo.dir, "", &mut e);
                            e
                        }
                    };
                    self.json(200, Value::Array(entries))
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
            ["api", "v4", "projects", p, "repository", "files", f, "raw"] => {
                match self.project(p) {
                    Some(v) => {
                        let repo = self.repo_of(&v);
                        let ref_name = q.get("ref").cloned().unwrap_or_else(|| "main".into());
                        match raw_file_at(repo, f, &ref_name) {
                            Some(bytes) => (200, "application/octet-stream", bytes),
                            None => self.not_found("File"),
                        }
                    }
                    None => self.not_found("Project"),
                }
            }
            [
                "api",
                "v4",
                "projects",
                p,
                "repository",
                "files",
                f,
                "blame",
            ] => match self.project(p) {
                Some(v) => {
                    let repo = self.repo_of(&v);
                    let ref_name = q.get("ref").cloned().unwrap_or_else(|| "main".into());
                    match blame(repo, f, &ref_name) {
                        Some(chunks) => self.json(200, Value::Array(chunks)),
                        None => self.not_found("File"),
                    }
                }
                None => self.not_found("Project"),
            },
            _ => self.not_found("Route"),
        }
    }

    fn repo_of(&self, project: &Value) -> &Repo {
        let id = project["id"].as_u64().expect("project id");
        self.repos.iter().find(|r| r.id == id).expect("repo by id")
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

    /// Branch listing: git repos enumerate real refs (HEAD flagged as
    /// the default); plain dirs report the one synthetic branch.
    fn branches(&self, r: &Repo) -> Value {
        if r.is_git {
            let head = git_text(&r.dir, &["symbolic-ref", "--short", "HEAD"])
                .unwrap_or_default()
                .trim()
                .to_string();
            let list: Vec<Value> = git_text(
                &r.dir,
                &[
                    "for-each-ref",
                    "refs/heads",
                    "--format=%(refname:short)%00%(objectname)",
                ],
            )
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let (name, sha) = l.split_once('\0')?;
                let mut b = json!({ "name": name, "commit": { "id": sha } });
                if name == head {
                    b["default"] = json!(true);
                }
                Some(b)
            })
            .collect();
            Value::Array(list)
        } else {
            Value::Array(vec![json!({
                "name": "main",
                "default": true,
                "commit": { "id": self.fs_head(r) },
            })])
        }
    }

    /// Ref → commit sha for the single-branch endpoint: `git
    /// rev-parse` for git repos, the synthetic tree-hash head for
    /// plain dirs (the adapter's cache key either way).
    fn branch_head(&self, r: &Repo, name: &str) -> Option<String> {
        if r.is_git {
            git_text(
                &r.dir,
                &["rev-parse", "--verify", &format!("refs/heads/{name}")],
            )
            .map(|s| s.trim().to_string())
        } else if name == "main" {
            Some(self.fs_head(r))
        } else {
            None
        }
    }

    /// Tag listing: peeled commits (what a tree-at-tag resolves to).
    fn tags(&self, r: &Repo) -> Value {
        if !r.is_git {
            return Value::Array(Vec::new());
        }
        let list: Vec<Value> = git_text(&r.dir, &["tag", "-l"])
            .unwrap_or_default()
            .lines()
            .filter_map(|name| {
                let sha = self.tag_commit(r, name)?;
                Some(json!({ "name": name, "commit": { "id": sha } }))
            })
            .collect();
        Value::Array(list)
    }

    fn tag_commit(&self, r: &Repo, name: &str) -> Option<String> {
        if !r.is_git {
            return None;
        }
        git_text(
            &r.dir,
            &["rev-parse", "--verify", &format!("{name}^{{commit}}")],
        )
        .map(|s| s.trim().to_string())
    }

    /// Commit log (newest first), path-filtered, paged by the query —
    /// git's own order and metadata, verbatim.
    fn commit_log(&self, r: &Repo, q: &Query) -> Value {
        if !r.is_git {
            return Value::Array(Vec::new());
        }
        let mut args = vec![
            "log".to_string(),
            "--format=%H\x1f%s\x1f%an\x1f%cI".to_string(),
        ];
        if let Some(ref_name) = q.get("ref_name") {
            args.push(ref_name.clone());
        }
        if let Some(path) = q.get("path") {
            args.push("--".to_string());
            args.push(path.clone());
        }
        let all: Vec<Value> =
            git_text(&r.dir, &args.iter().map(String::as_str).collect::<Vec<_>>())
                .unwrap_or_default()
                .lines()
                .filter_map(|l| {
                    let (id, rest) = l.split_once('\x1f')?;
                    let (title, rest) = rest.split_once('\x1f')?;
                    let (author, date) = rest.split_once('\x1f')?;
                    Some(json!({
                        "id": id,
                        "title": title,
                        "author_name": author,
                        "committed_date": date,
                    }))
                })
                .collect();
        let per_page: usize = q.get("per_page").and_then(|v| v.parse().ok()).unwrap_or(20);
        let page: usize = q
            .get("page")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1)
            .max(1);
        let start = (page - 1) * per_page;
        let slice: Vec<Value> = all.into_iter().skip(start).take(per_page).collect();
        Value::Array(slice)
    }

    /// The plain-dir branch head: a hash over the recursive tree, so
    /// any content mutation moves it (FC-011's cache-buster).
    fn fs_head(&self, r: &Repo) -> String {
        let mut entries = Vec::new();
        let root = walk_tree(&r.dir, "", &mut entries);
        let mut hasher = Sha1::new();
        hasher.update(format!("commit\n{root}\nmain"));
        format!("{:x}", hasher.finalize())
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

// ---------------------------------------------------------------------
// git plumbing (fixture/vcs) — the repo IS the fixture; git's answers
// ARE the expectations.
// ---------------------------------------------------------------------

fn git_text(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        None
    }
}

fn git_bytes(dir: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if out.status.success() {
        Some(out.stdout)
    } else {
        None
    }
}

/// `git ls-tree -r -t <ref>` → [{id, type, path}] — real git ids, so
/// blob entries agree with `repo/blob_at`'s content ids exactly.
fn git_ls_tree(dir: &Path, refspec: &str) -> Option<Vec<Value>> {
    let out = git_text(dir, &["ls-tree", "-r", "-t", refspec])?;
    let entries = out
        .lines()
        .filter_map(|l| {
            let (meta, path) = l.split_once('\t')?;
            let mut fields = meta.split_whitespace();
            let _mode = fields.next()?;
            let kind = fields.next()?.to_string();
            let id = fields.next()?.to_string();
            Some(json!({ "id": id, "type": kind, "path": path }))
        })
        .collect();
    Some(entries)
}

/// Raw bytes of `path` at `ref_name` (worktree for plain dirs).
fn raw_file_at(r: &Repo, path: &str, ref_name: &str) -> Option<Vec<u8>> {
    if r.is_git {
        git_bytes(&r.dir, &["show", &format!("{ref_name}:{path}")])
    } else {
        std::fs::read(r.dir.join(path)).ok()
    }
}

/// `git blame --porcelain` → GitLab chunk shape. Adjacent same-sha
/// chunks merge (the adapter coalesces too, but pre-merged is tidy);
/// author + author-date come from the commit itself (`%an` / `%aI`,
/// the instant FC-098 compares against).
fn blame(r: &Repo, path: &str, ref_name: &str) -> Option<Vec<Value>> {
    if !r.is_git {
        return None;
    }
    let out = git_text(&r.dir, &["blame", "--porcelain", ref_name, "--", path])?;
    let mut chunks: Vec<(String, Vec<String>)> = Vec::new();
    for line in out.lines() {
        if let Some(content) = line.strip_prefix('\t') {
            if let Some((_, lines)) = chunks.last_mut() {
                lines.push(content.to_string());
            }
        } else if let Some(header) = parse_blame_header(line) {
            let sha = header;
            match chunks.last_mut() {
                Some((prev, _)) if *prev == sha => {}
                _ => chunks.push((sha, Vec::new())),
            }
        }
    }
    let mut result = Vec::new();
    for (sha, lines) in chunks {
        if lines.is_empty() {
            continue;
        }
        let meta = git_text(&r.dir, &["show", "-s", "--format=%an%n%aI", &sha]).unwrap_or_default();
        let mut meta_lines = meta.lines();
        let author = meta_lines.next().unwrap_or_default().to_string();
        let date = meta_lines.next().unwrap_or_default().to_string();
        result.push(json!({
            "commit": {
                "id": sha,
                "author_name": author,
                "committed_date": date,
            },
            "lines": lines,
        }));
    }
    Some(result)
}

/// A porcelain header line: `<40-hex> <orig> <final> [<count>]`
/// (boundary commits carry a `^` prefix — stripped; the commit is the
/// same). Metadata lines don't match and are skipped by the caller.
fn parse_blame_header(line: &str) -> Option<String> {
    let body = line.strip_prefix('^').unwrap_or(line);
    let mut fields = body.split_whitespace();
    let sha = fields.next()?;
    let is_sha = sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit());
    let rest: Vec<_> = fields.collect();
    let shape = rest.len() == 2 || rest.len() == 3;
    if is_sha && shape {
        Some(sha.to_string())
    } else {
        None
    }
}

// ---------------------------------------------------------------------
// Filesystem fixture serving (alpha/beta) — fresh from disk per
// request, content-keyed ids.
// ---------------------------------------------------------------------

/// Every file under `dir` as (repo-relative path, bytes), sorted by
/// path. `.git` is never content.
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
            if name == ".git" {
                continue;
            }
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
/// changes. Returns the root tree id.
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
        if name == ".git" {
            continue;
        }
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
            let id = git_blob_sha(&std::fs::read(&path).unwrap_or_default());
            out.push(json!({ "id": id, "type": "blob", "path": rel }));
            lines.push(format!("blob {name} {id}"));
        }
    }
    let mut hasher = Sha1::new();
    hasher.update(format!("tree\n{}\n", lines.join("\n")));
    format!("{:x}", hasher.finalize())
}

fn blob_by_sha(dir: &Path, sha: &str) -> Option<Vec<u8>> {
    files_under(dir)
        .into_iter()
        .find(|(_, bytes)| git_blob_sha(bytes) == sha)
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
