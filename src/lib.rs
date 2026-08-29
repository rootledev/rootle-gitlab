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

/// The transcript sink every in-flight request shares. The mutex is
/// held across one `writeln!` + flush, so a line written by one
/// request can never interleave mid-line with another's — NDJSON
/// line atomicity while whole transcripts interleave freely.
type SharedOut<W> = std::sync::Arc<parking_lot::Mutex<W>>;

/// One worker thread's whole job: run a request to completion, push
/// each of its transcript lines through the shared sink, one line
/// per lock.
fn write_transcript<W: std::io::Write>(handler: &Handler, line: &str, out: &SharedOut<W>) {
    if let Some(reply) = respond_transcript(handler, line) {
        for one in reply {
            // parking_lot: no poisoning — a writer that panicked
            // mid-line can't wedge the transport for the others.
            let mut w = out.lock();
            let _ = writeln!(w, "{one}");
            let _ = w.flush();
        }
    }
}

/// The serve loop: lines are read on the calling thread, but each
/// request runs on its own scoped worker thread — a slow request
/// never head-of-line blocks the requests behind it (protocol v1.3:
/// rootle pipelines by request id, and every transcript line is
/// id-tagged, so interleaving is legal on the wire). One request's
/// own lines stay ordered — they come from one thread. Workers join
/// when input closes; `serve_stdio` binds this to stdin/stdout, the
/// concurrency test drives it with an in-memory writer.
fn serve<W: std::io::Write + Send>(
    handler: &Handler,
    input: &mut dyn std::io::BufRead,
    out: SharedOut<W>,
) {
    use std::io::BufRead;
    std::thread::scope(|scope| {
        for line in input.lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue; // blank chatter earns no thread
            }
            let out = std::sync::Arc::clone(&out);
            scope.spawn(move || write_transcript(handler, &line, &out));
        }
    });
}

/// The stdin loop: one line in on this thread, one worker thread per
/// request out through a shared line-atomic stdout writer, flushed
/// per line. Shared by the binary and the forge-conformance harness
/// example — the loop is the transport, the `Handler` is the adapter
/// (and is `Sync` by construction — see its audit note).
pub fn serve_stdio(handler: &Handler) {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    serve(
        handler,
        &mut input,
        std::sync::Arc::new(parking_lot::Mutex::new(std::io::stdout())),
    );
}

#[cfg(test)]
mod tests {
    use crate::handlers::testkit::{project_json, tempdir, token_env_set};
    use crate::{Handler, respond};
    use serde_json::{Value, json};
    use std::sync::Arc;
    use std::time::Duration;
    use wiremock::MockServer;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

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

    /// The serve loop is concurrent: the request read FIRST from
    /// stdin (a 400ms endpoint) must not head-of-line block the
    /// instant request read after it. Under the old serial loop the
    /// slow reply always landed first — ordering here is the
    /// regression discriminator, and it is deterministic: the 400ms
    /// server-side gap swallows thread-spawn jitter, so the fast
    /// reply always lands first no matter how the workers schedule.
    #[tokio::test]
    async fn serve_answers_fast_requests_while_slow_ones_run() {
        let server = MockServer::start().await;
        token_env_set();
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/slow%2Fr"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(project_json(7, "slow/r"))
                    .set_delay(Duration::from_millis(400)),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/fast%2Fr"))
            .respond_with(ResponseTemplate::new(200).set_body_json(project_json(8, "fast/r")))
            .expect(1)
            .mount(&server)
            .await;

        let uri = server.uri();
        let cache = tempdir();
        let input = format!(
            "{}\n{}\n",
            json!({"jsonrpc":"2.0","id":1,"method":"repo/clone_url","params":{"repo":"slow/r"}}),
            json!({"jsonrpc":"2.0","id":2,"method":"repo/clone_url","params":{"repo":"fast/r"}}),
        );
        // reqwest::blocking may neither be created nor dropped on a
        // tokio worker (testkit rule): the whole serve loop, handler
        // included, runs on a plain scoped thread.
        let sink: Arc<parking_lot::Mutex<Vec<u8>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        {
            let sink = Arc::clone(&sink);
            std::thread::scope(|s| {
                s.spawn(move || {
                    let handler = Handler::new(&uri, "GL_TEST_TOKEN", Some(cache));
                    crate::serve(&handler, &mut input.as_bytes(), sink);
                })
                .join()
                .expect("serve thread");
            });
        }
        let transcript = String::from_utf8(std::mem::take(&mut *sink.lock())).unwrap();
        let replies: Vec<Value> = transcript
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            replies.len(),
            2,
            "one whole-line reply per request: {transcript}"
        );
        assert_eq!(replies[0]["id"], json!(2), "fast request answers first");
        assert_eq!(
            replies[0]["result"]["clone_url"],
            json!("https://gitlab.example.com/fast/r.git")
        );
        assert_eq!(replies[1]["id"], json!(1), "slow request lands after");
        assert_eq!(
            replies[1]["result"]["clone_url"],
            json!("https://gitlab.example.com/slow/r.git")
        );
        server.verify().await;
    }
}
