//! rootle-gitlab — the GitLab provider for rootle.
//!
//! Speaks the rootle stdio provider protocol (NDJSON-RPC 2.0 over
//! stdin/stdout; the spec lives in rootledev/rootle,
//! doc/provider-protocol.md) against GitLab's REST v4 API. The first
//! out-of-tree provider (plans/0009): this crate shares no code with
//! rootle — the wire contract is the entire interface.
//!
//! Process-shape obligations (protocol: restart obligations): startup
//! is cheap and idempotent — no network, no token read; rootle may
//! kill and respawn this process an unbounded number of times per
//! session. Credentials are read lazily on first API call; caches are
//! on disk keyed by git content ids, so a respawn loses nothing.

pub mod api;
pub mod cache;
pub mod handlers;

pub use handlers::{Handler, WireError};

use serde_json::{Value, json};

/// One request line → one reply line. Used by the binary's stdin loop
/// and by tests driving the protocol surface directly.
pub fn respond(handler: &Handler, line: &str) -> Option<String> {
    let msg: Value = serde_json::from_str(line.trim()).ok()?;
    // Notifications (no id) are tolerated chatter; the advisory
    // cancel is the only one today and is ignored by contract.
    let id = msg.get("id")?.clone();
    let method = msg.get("method")?.as_str()?.to_string();
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
    let reply = match handler.dispatch(&method, &params) {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e.to_json() }),
    };
    Some(reply.to_string())
}
