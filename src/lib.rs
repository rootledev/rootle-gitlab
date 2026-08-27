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

/// One request line → its full stdout transcript, in order. Streaming
/// requests (v1.3 progressive results: `search/code` with
/// `partial: true`) interleave `$/partial` notifications — each
/// carrying the request id — before the final reply; every other
/// request yields exactly the reply line. Notifications (no id)
/// produce nothing.
pub fn respond_transcript(handler: &Handler, line: &str) -> Option<Vec<String>> {
    let msg: Value = serde_json::from_str(line.trim()).ok()?;
    // Notifications (no id) are tolerated chatter; the advisory
    // cancel is the only one today and is ignored by contract.
    let id = msg.get("id")?.clone();
    let method = msg.get("method")?.as_str()?.to_string();
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
    let mut lines = Vec::new();
    let reply = handler.dispatch_streaming(&method, &params, &id, &mut |partial| {
        lines.push(
            json!({ "jsonrpc": "2.0", "method": "$/partial", "params": partial }).to_string(),
        );
    });
    let body = match reply {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e.to_json() }),
    };
    lines.push(body.to_string());
    Some(lines)
}

/// One request line → the final reply line (streaming batches are the
/// caller's business — see `respond_transcript`). Used by tests
/// driving the protocol surface directly.
pub fn respond(handler: &Handler, line: &str) -> Option<String> {
    respond_transcript(handler, line)?.pop()
}

/// The stdin loop: one line in, its lines out, flushed per line.
/// Shared by the binary and the forge-conformance harness example —
/// the loop is the transport, the `Handler` is the adapter.
pub fn serve_stdio(handler: &Handler) {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    use std::io::BufRead;
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = respond_transcript(handler, &line) {
            for one in reply {
                println!("{one}");
                use std::io::Write;
                let _ = out.flush();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::handlers::testkit::tempdir;
    use crate::{Handler, respond};
    use serde_json::json;
    use wiremock::MockServer;

    #[tokio::test]
    async fn notifications_without_an_id_are_ignored() {
        let server = MockServer::start().await;
        let cache = tempdir();
        let line =
            json!({"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id": 7}}).to_string();
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
}
