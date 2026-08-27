//! `repo/refs`: branches + tags with the default flagged — the refs
//! popup's data (protocol v1.5, plans/0016 M1a).

use super::{Handler, WireError, WireResult};
use serde_json::{Value, json};

impl Handler {
    pub(super) fn repo_refs(&self, repo: &str) -> WireResult {
        let project = self.project(repo).map_err(|e| WireError::from_api(&e))?;
        let (branches, branches_cut) = self
            .gl
            .branches(project.id)
            .map_err(|e| WireError::from_api(&e))?;
        let (tags, tags_cut) = self
            .gl
            .tags(project.id)
            .map_err(|e| WireError::from_api(&e))?;
        let default_name = project.default_branch.clone().unwrap_or_default();
        let branches: Vec<Value> = branches
            .into_iter()
            .filter_map(|b| {
                let name = b.name?; // an unnameable branch can't be switched to
                let is_default = b.default.unwrap_or(false) || name == default_name;
                Some(if is_default {
                    json!({"name": name, "sha": b.commit.id, "default": true})
                } else {
                    json!({"name": name, "sha": b.commit.id})
                })
            })
            .collect();
        let tags: Vec<Value> = tags
            .into_iter()
            .filter_map(|t| Some(json!({"name": t.name, "sha": t.commit?.id})))
            .collect();
        let mut body = json!({ "branches": branches, "tags": tags });
        // The wire shape has no truncated slot; a listing that hit the
        // aggregation cap still says so — reader tolerance carries it.
        if branches_cut || tags_cut {
            body["truncated"] = json!(true);
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use crate::handlers::testkit::{
        ask, error, mock_project_lookup, result, tempdir, token_env_set,
    };
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn refs_list_branches_and_tags_with_one_default_flag() {
        let server = MockServer::start().await;
        token_env_set();
        mock_project_lookup(&server, 42, "g/big").await;
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/42/repository/branches"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"name": "main", "default": true, "commit": {"id": "aaa"}},
                {"name": "dev", "default": false, "commit": {"id": "bbb"}},
                {"name": "release/2.7", "commit": {"id": "ccc"}}
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/42/repository/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"name": "v1.0", "commit": {"id": "ddd"}}
            ])))
            .mount(&server)
            .await;

        let r = result(ask(
            &server.uri(),
            &tempdir(),
            "repo/refs",
            json!({"repo": "g/big"}),
        ));
        let branches = r["branches"].as_array().unwrap();
        assert_eq!(branches.len(), 3);
        assert_eq!(branches[0]["name"], "main");
        assert_eq!(branches[0]["sha"], "aaa");
        assert_eq!(branches[0]["default"], true);
        assert_eq!(branches[1]["name"], "dev");
        assert_eq!(branches[1]["sha"], "bbb");
        assert!(
            branches[1].get("default").is_none(),
            "exactly one default flag"
        );
        // A branch missing GitLab's own flag still matches by name:
        // the project entity says the default branch is "main" only,
        // so release/2.7 carries no flag but proves slash names pass.
        assert_eq!(branches[2]["name"], "release/2.7");
        assert!(branches[2].get("default").is_none());
        let tags = r["tags"].as_array().unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0], json!({"name": "v1.0", "sha": "ddd"}));
        assert!(r.get("truncated").is_none());
    }

    #[tokio::test]
    async fn refs_flags_default_by_name_when_the_listing_omits_the_flag() {
        let server = MockServer::start().await;
        token_env_set();
        mock_project_lookup(&server, 42, "g/big").await;
        // Old instances omit `default` on branch listings; the project
        // entity's default_branch ("main" from the fixture) still
        // identifies it.
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/42/repository/branches"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"name": "main", "commit": {"id": "aaa"}},
                {"name": "trunk", "commit": {"id": "bbb"}}
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/42/repository/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;

        let r = result(ask(
            &server.uri(),
            &tempdir(),
            "repo/refs",
            json!({"repo": "g/big"}),
        ));
        let branches = r["branches"].as_array().unwrap();
        assert_eq!(branches[0]["default"], true, "matched by name");
        assert!(branches[1].get("default").is_none());
        assert_eq!(r["tags"], json!([]));
    }

    #[tokio::test]
    async fn refs_aggregate_pages() {
        let server = MockServer::start().await;
        token_env_set();
        mock_project_lookup(&server, 42, "g/big").await;
        let page1: Vec<_> = (0..100)
            .map(|i| json!({"name": format!("b{i:03}"), "commit": {"id": format!("s{i:03}")}}))
            .collect();
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/42/repository/branches"))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page1))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/42/repository/branches"))
            .and(query_param("page", "2"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!([{"name": "last", "commit": {"id": "zzz"}}])),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v4/projects/42/repository/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;

        let r = result(ask(
            &server.uri(),
            &tempdir(),
            "repo/refs",
            json!({"repo": "g/big"}),
        ));
        let branches = r["branches"].as_array().unwrap();
        assert_eq!(branches.len(), 101);
        assert_eq!(branches[100]["name"], "last");
    }

    #[tokio::test]
    async fn refs_on_unknown_repo_is_not_found() {
        let server = MockServer::start().await;
        token_env_set();
        let enc = "g%2Fgone";
        Mock::given(method("GET"))
            .and(path(format!("/api/v4/projects/{enc}")))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(json!({"message": "404 Project Not Found"})),
            )
            .mount(&server)
            .await;
        let e = error(ask(
            &server.uri(),
            &tempdir(),
            "repo/refs",
            json!({"repo": "g/gone"}),
        ));
        assert_eq!(e["data"]["kind"], "not_found");
    }
}
