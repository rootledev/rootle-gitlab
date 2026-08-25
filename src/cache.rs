//! Provider-scoped disk cache under
//! `~/.cache/rootle/providers/rootle-gitlab/` — the layout the
//! protocol doc recommends (rootle never touches it; the
//! `edit/` scratch belongs to the TUI).
//!
//! Trees and blobs are immutable and sha-keyed (git blob/tree ids are
//! content ids — the protocol's requirement holds for free). Project
//! metadata is keyed by path and revalidated lazily: a 404 on a
//! cached project invalidates it (the repo moved or was renamed).
//! Every path component is percent-encoded — values come from API
//! responses and are not trusted to be well-formed (branch names with
//! `/` are legitimate; `..` must never become path structure).

use crate::api::Project;
use serde::{Deserialize, Serialize};
use std::io;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchHead {
    pub commit: String,
}

pub struct Cache {
    root: Option<PathBuf>,
}

/// Percent-encode anything outside [A-Za-z0-9_-]: separators, `..`,
/// and NUL can never become path structure. Dots encode too — a
/// literal `..` must not survive as a component.
pub fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

impl Cache {
    /// `base` overrides the root (tests inject a tempdir); None when
    /// no home can be resolved — every op becomes a no-op miss.
    pub fn new(base: Option<PathBuf>) -> Self {
        Cache {
            root: base.or_else(|| {
                dirs::cache_dir().map(|d| d.join("rootle").join("providers").join("rootle-gitlab"))
            }),
        }
    }

    // -- project metadata (path-keyed, lazily revalidated) ----------

    pub fn project(&self, path: &str) -> Option<Project> {
        let text = std::fs::read_to_string(
            self.root
                .as_ref()?
                .join("projects")
                .join(format!("{}.json", encode_component(path))),
        )
        .ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn put_project(&self, p: &Project) {
        self.write(
            "projects",
            &format!("{}.json", encode_component(&p.path_with_namespace)),
            &serde_json::to_vec(p).unwrap_or_default(),
        );
    }

    pub fn drop_project(&self, path: &str) {
        if let Some(root) = &self.root {
            let _ = std::fs::remove_file(
                root.join("projects")
                    .join(format!("{}.json", encode_component(path))),
            );
        }
    }

    // -- branch heads (mutable, tiny: branch → commit sha) ----------

    pub fn branch_head(&self, project_id: u64, branch: &str) -> Option<String> {
        let text = std::fs::read_to_string(
            self.root
                .as_ref()?
                .join("branches")
                .join(project_id.to_string())
                .join(encode_component(branch)),
        )
        .ok()?;
        Some(text.trim_end().to_string())
    }

    pub fn put_branch_head(&self, project_id: u64, branch: &str, commit: &str) {
        self.write(
            &format!("branches/{}", project_id),
            &encode_component(branch),
            commit.as_bytes(),
        );
    }

    // -- trees (immutable, sha-keyed) --------------------------------

    pub fn tree(&self, sha: &str) -> Option<Vec<u8>> {
        std::fs::read(
            self.root
                .as_ref()?
                .join("trees")
                .join(format!("{}.json", encode_component(sha))),
        )
        .ok()
    }

    pub fn put_tree(&self, sha: &str, body: &[u8]) {
        self.write("trees", &format!("{}.json", encode_component(sha)), body);
    }

    // -- blobs (immutable, raw bytes, fanout by first 2 chars) -------

    pub fn blob(&self, sha: &str) -> Option<Vec<u8>> {
        std::fs::read(self.blob_path(sha)?).ok()
    }

    pub fn put_blob(&self, sha: &str, bytes: &[u8]) {
        let enc = encode_component(sha);
        let split = 2.min(enc.len());
        self.write(&format!("blobs/{}", &enc[..split]), &enc[split..], bytes);
    }

    fn blob_path(&self, sha: &str) -> Option<PathBuf> {
        let enc = encode_component(sha);
        let split = 2.min(enc.len());
        Some(
            self.root
                .as_ref()?
                .join("blobs")
                .join(&enc[..split])
                .join(&enc[split..]),
        )
    }

    /// Best-effort atomic write: tmp + rename, parents created.
    fn write(&self, dir: &str, name: &str, bytes: &[u8]) {
        let Some(root) = &self.root else { return };
        let dir = root.join(dir);
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let path = dir.join(name);
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// From the protocol: a cache read that cannot be satisfied is a
/// miss, not an error — every `None` above feeds a re-fetch.
pub fn _silence(_: io::Error) {}
