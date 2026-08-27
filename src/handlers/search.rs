//! Repo discovery: `search/repos` (projects + groups mixed) and
//! `org/repos` (one group's projects, the org component stripped).

use super::{Handler, WireResult, w};
use serde_json::{Value, json};

impl Handler {
    pub(super) fn search_repos(&self, query: &str) -> WireResult {
        w(self.gl.search_projects(query), |projects| {
            let mut items: Vec<Value> = projects
                .into_iter()
                .map(|p| json!({ "full_name": p.path_with_namespace }))
                .collect();
            if let Ok(groups) = self.gl.search_groups(query) {
                for g in groups {
                    items.push(json!({ "org": g.full_path }));
                }
            }
            json!({ "items": items.into_iter().take(20).collect::<Vec<_>>() })
        })
    }

    pub(super) fn org_repos(&self, org: &str) -> WireResult {
        let id = self.group_id(org)?;
        w(self.gl.group_projects(id), |(projects, truncated)| {
            // The repos level carries the path AFTER the org component
            // (rootle re-joins org + name); nested subgroups keep the
            // rest of their path — multi-slash ids are legal.
            let prefix = format!("{org}/");
            json!({
                "repos": projects
                    .into_iter()
                    .map(|p| {
                        p.path_with_namespace
                            .strip_prefix(&prefix)
                            .map(str::to_string)
                            .unwrap_or(p.path_with_namespace)
                    })
                    .collect::<Vec<String>>(),
                "truncated": truncated,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::handlers::testkit::{ask, project_json, result, tempdir, token_env_set};
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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
}
