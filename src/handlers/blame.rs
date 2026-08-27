//! `repo/blame`: per-line attribution coalesced into sha runs —
//! 1-based, inclusive, covering every line (protocol v1.5,
//! plans/0016 M1c).

use super::{Handler, WireError, WireResult};
use crate::api::BlameChunk;
use serde_json::{Value, json};

impl Handler {
    pub(super) fn repo_blame(&self, repo: &str, path: &str, ref_: Option<&str>) -> WireResult {
        let project = self.project(repo).map_err(|e| WireError::from_api(&e))?;
        // GitLab takes the ref verbatim and defaults to the default
        // branch when omitted — nothing to resolve, nothing cached.
        let ref_name = match ref_ {
            Some(r) => r.to_string(),
            None => project.branch(),
        };
        let chunks = self
            .gl
            .blame(project.id, path, &ref_name)
            .map_err(|e| WireError::from_api(&e))?;
        Ok(json!({ "ranges": coalesce(&chunks) }))
    }
}

/// Chunks → ranges: line numbers accumulate across chunks (1-based,
/// inclusive, every line covered), and adjacent chunks sharing a sha
/// merge into one run — GitLab usually pre-coalesces, but the wire
/// contract promises it, not the backend.
fn coalesce(chunks: &[BlameChunk]) -> Vec<Value> {
    let mut ranges: Vec<Value> = Vec::new();
    let mut line = 1usize;
    for chunk in chunks {
        let n = chunk.lines.len();
        if n == 0 {
            continue;
        }
        let (sha, author, date) = match &chunk.commit {
            Some(c) => (
                c.id.as_str(),
                c.author_name.clone().unwrap_or_default(),
                c.committed_date.clone().unwrap_or_default(),
            ),
            // A chunk without a commit still owns its lines — the
            // coverage guarantee outranks the attribution.
            None => ("", String::new(), String::new()),
        };
        let end = line + n - 1;
        match ranges.last_mut() {
            Some(prev) if prev["sha"] == json!(sha) => {
                prev["end_line"] = json!(end);
            }
            _ => ranges.push(json!({
                "start_line": line,
                "end_line": end,
                "sha": sha,
                "author": author,
                "date": date,
            })),
        }
        line = end + 1;
    }
    ranges
}

#[cfg(test)]
mod tests {
    use crate::handlers::testkit::{
        ask, error, mock_project_lookup, result, tempdir, token_env_set,
    };
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn chunk(id: &str, lines: usize) -> serde_json::Value {
        json!({
            "commit": {
                "id": id,
                "author_name": "Ada Lovelace",
                "committed_date": "2026-08-01T10:00:00.000+00:00"
            },
            "lines": (0..lines).map(|i| format!("line {i}")).collect::<Vec<_>>()
        })
    }

    #[tokio::test]
    async fn blame_coalesces_adjacent_same_sha_runs() {
        let server = MockServer::start().await;
        token_env_set();
        mock_project_lookup(&server, 42, "g/big").await;
        // sha1 owns 3+2 adjacent chunks (5 lines total — must merge);
        // sha2 owns the last 2. A 7-line file, every line covered.
        Mock::given(method("GET"))
            .and(path(
                "/api/v4/projects/42/repository/files/src%2Fmain.rs/blame",
            ))
            .and(query_param("ref", "main"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                chunk("sha1", 3),
                chunk("sha1", 2),
                chunk("sha2", 2)
            ])))
            .mount(&server)
            .await;

        let r = result(ask(
            &server.uri(),
            &tempdir(),
            "repo/blame",
            json!({"repo": "g/big", "path": "src/main.rs"}),
        ));
        assert_eq!(
            r["ranges"],
            json!([
                {"start_line": 1, "end_line": 5, "sha": "sha1",
                 "author": "Ada Lovelace", "date": "2026-08-01T10:00:00.000+00:00"},
                {"start_line": 6, "end_line": 7, "sha": "sha2",
                 "author": "Ada Lovelace", "date": "2026-08-01T10:00:00.000+00:00"}
            ])
        );
    }

    #[tokio::test]
    async fn blame_passes_ref_through_and_maps_not_found() {
        let server = MockServer::start().await;
        token_env_set();
        mock_project_lookup(&server, 42, "g/big").await;
        Mock::given(method("GET"))
            .and(path(
                "/api/v4/projects/42/repository/files/src%2Fmain.rs/blame",
            ))
            .and(query_param("ref", "release/2.7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([chunk("sha9", 1)])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/42/repository/files/nope.rs/blame"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(json!({"message": "404 File Not Found"})),
            )
            .mount(&server)
            .await;

        let r = result(ask(
            &server.uri(),
            &tempdir(),
            "repo/blame",
            json!({"repo": "g/big", "path": "src/main.rs", "ref": "release/2.7"}),
        ));
        assert_eq!(r["ranges"][0]["start_line"], 1);
        assert_eq!(r["ranges"][0]["end_line"], 1);
        assert_eq!(r["ranges"][0]["sha"], "sha9");

        let e = error(ask(
            &server.uri(),
            &tempdir(),
            "repo/blame",
            json!({"repo": "g/big", "path": "nope.rs"}),
        ));
        assert_eq!(e["data"]["kind"], "not_found");
    }
}
