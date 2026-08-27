//! `repo/log`: commit history, newest first, `path`-filtered, `limit`
//! under the bounded-compute contract (protocol v1.5, plans/0016 M1b).

use super::{Handler, WireError, WireResult};
use crate::api;
use serde_json::{Value, json};

impl Handler {
    pub(super) fn repo_log(
        &self,
        repo: &str,
        path: Option<&str>,
        ref_: Option<&str>,
        limit: Option<u64>,
    ) -> WireResult {
        let project = self.project(repo).map_err(|e| WireError::from_api(&e))?;
        // Bounded compute: the client's limit is the budget; absent
        // means our default, absurd asks cap at LOG_MAX_LIMIT —
        // `truncated: true` always names whichever budget stopped the
        // scan. GitLab takes the ref verbatim (branch, tag, or sha)
        // and omits it for the default branch — no resolution needed,
        // nothing here is cached.
        let want = limit
            .unwrap_or(api::LOG_DEFAULT_LIMIT as u64)
            .clamp(1, api::LOG_MAX_LIMIT as u64) as usize;
        let (commits, truncated) = self
            .gl
            .commit_log(project.id, path, ref_, want)
            .map_err(|e| WireError::from_api(&e))?;
        let items: Vec<Value> = commits
            .into_iter()
            .map(|c| {
                json!({
                    "sha": c.id,
                    // GitLab's `title` is the subject (first line);
                    // the date rides verbatim — ISO-8601 already.
                    "subject": c.title.unwrap_or_default(),
                    "author": c.author_name.unwrap_or_default(),
                    "date": c.committed_date.unwrap_or_default(),
                })
            })
            .collect();
        Ok(json!({ "items": items, "truncated": truncated }))
    }
}

#[cfg(test)]
mod tests {
    use crate::handlers::testkit::{ask, mock_project_lookup, result, tempdir, token_env_set};
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn commit(id: &str, title: &str) -> serde_json::Value {
        json!({
            "id": id,
            "title": title,
            "author_name": "Ada Lovelace",
            "committed_date": "2026-08-01T10:00:00.000+00:00"
        })
    }

    #[tokio::test]
    async fn log_maps_subject_author_date_newest_first() {
        let server = MockServer::start().await;
        token_env_set();
        mock_project_lookup(&server, 42, "g/big").await;
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/42/repository/commits"))
            .and(query_param_is_missing("path"))
            .and(query_param_is_missing("ref_name"))
            .and(query_param("per_page", "100")) // 500+1 > 100 → pages of 100
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!([commit("cc2", "second"), commit("cc1", "first")])),
            )
            .mount(&server)
            .await;

        let r = result(ask(
            &server.uri(),
            &tempdir(),
            "repo/log",
            json!({"repo": "g/big"}),
        ));
        let items = r["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0],
            json!({
                "sha": "cc2", "subject": "second", "author": "Ada Lovelace",
                "date": "2026-08-01T10:00:00.000+00:00"
            })
        );
        assert_eq!(items[1]["sha"], "cc1");
        assert_eq!(r["truncated"], false, "short page = complete");
    }

    #[tokio::test]
    async fn log_honors_limit_and_flags_truncated_at_the_cliff() {
        let server = MockServer::start().await;
        token_env_set();
        mock_project_lookup(&server, 42, "g/big").await;
        // limit 3 probes 4; four exist → stop at 3, truncated: true.
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/42/repository/commits"))
            .and(query_param("per_page", "4"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                commit("c1", "one"),
                commit("c2", "two"),
                commit("c3", "three"),
                commit("c4", "four")
            ])))
            .mount(&server)
            .await;

        let r = result(ask(
            &server.uri(),
            &tempdir(),
            "repo/log",
            json!({"repo": "g/big", "limit": 3}),
        ));
        assert_eq!(r["items"].as_array().unwrap().len(), 3);
        assert_eq!(r["items"][2]["sha"], "c3");
        assert_eq!(r["truncated"], true);
    }

    #[tokio::test]
    async fn log_passes_path_and_ref_through() {
        let server = MockServer::start().await;
        token_env_set();
        mock_project_lookup(&server, 42, "g/big").await;
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/42/repository/commits"))
            .and(query_param("path", "src/lib.rs"))
            .and(query_param("ref_name", "release/2.7"))
            .and(query_param("per_page", "3"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!([commit("cf", "touched lib")])),
            )
            .mount(&server)
            .await;

        let r = result(ask(
            &server.uri(),
            &tempdir(),
            "repo/log",
            json!({"repo": "g/big", "path": "src/lib.rs", "ref": "release/2.7", "limit": 2}),
        ));
        assert_eq!(r["items"][0]["sha"], "cf");
        assert_eq!(r["truncated"], false);
    }
    #[tokio::test]
    async fn log_caps_an_absurd_limit_under_bounded_compute() {
        let server = MockServer::start().await;
        token_env_set();
        mock_project_lookup(&server, 42, "g/big").await;
        // limit 100_000 clamps to LOG_MAX_LIMIT (1000): probe pages of
        // 100 until accumulation passes the cap — ten full pages,
        // then one more commit on page 11 to trip `truncated`.
        for page in 1..=10u32 {
            let body: Vec<_> = (0..100)
                .map(|i| commit(&format!("c{page:02}{i:02}"), "m"))
                .collect();
            Mock::given(method("GET"))
                .and(path("/api/v4/projects/42/repository/commits"))
                .and(query_param("per_page", "100"))
                .and(query_param("page", page.to_string()))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&server)
                .await;
        }
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/42/repository/commits"))
            .and(query_param("per_page", "100"))
            .and(query_param("page", "11"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!([commit("c-one-past", "m")])),
            )
            .mount(&server)
            .await;

        let r = result(ask(
            &server.uri(),
            &tempdir(),
            "repo/log",
            json!({"repo": "g/big", "limit": 100_000}),
        ));
        assert_eq!(r["items"].as_array().unwrap().len(), 1000);
        assert_eq!(r["items"][999]["sha"], "c1099");
        assert_eq!(r["truncated"], true, "the adapter's own cap speaks");
    }
}
