use crate::settings;
use async_trait::async_trait;
use futures::Stream;
use karakeep_client::{BookmarkCreate, KarakeepClient};
use serde_json::{Value, json};
use std::{collections::HashSet, pin::Pin, time::Duration};

#[derive(Debug, Clone)]
pub struct GithubStars {}

const STARS_QUERY: &str = r#"
query StarMetadata($after: String) {
  viewer {
    starredRepositories(first:100, after:$after, orderBy:{field:STARRED_AT,direction:DESC}) {
      edges { starredAt node {
        url nameWithOwner description createdAt pushedAt openGraphImageUrl
        owner { login } primaryLanguage { name } licenseInfo { spdxId }
        repositoryTopics(first:100) { nodes { topic { name } } }
      } }
      pageInfo { hasNextPage endCursor }
    }
  }
}
"#;

fn bookmark(edge: &Value) -> anyhow::Result<BookmarkCreate> {
    let starred = edge["starredAt"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing star timestamp"))?;
    repository_bookmark(&edge["node"], Some(starred))
}

fn repository_bookmark(repo: &Value, starred: Option<&str>) -> anyhow::Result<BookmarkCreate> {
    let url = repo["url"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing repository URL"))?;
    let title = repo["nameWithOwner"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing repository name"))?;
    let mut metadata = json!({"title":title,"publisher":"GitHub"});
    if let Some(starred) = starred {
        chrono::DateTime::parse_from_rfc3339(starred)?;
        metadata["createdAt"] = starred.into();
    }
    for (field, value) in [
        ("description", &repo["description"]),
        ("author", &repo["owner"]["login"]),
        ("datePublished", &repo["createdAt"]),
        ("dateModified", &repo["pushedAt"]),
    ] {
        if let Some(text) = value.as_str().filter(|s| !s.trim().is_empty()) {
            metadata[field] = text.into();
        }
    }
    let mut tags: Vec<String> = repo["repositoryTopics"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|node| node["topic"]["name"].as_str())
        .map(str::to_owned)
        .collect();
    if let Some(language) = repo["primaryLanguage"]["name"].as_str() {
        tags.push(language.into());
    }
    if let Some(license) = repo["licenseInfo"]["spdxId"]
        .as_str()
        .filter(|s| !["NOASSERTION", "NONE", ""].contains(s))
    {
        tags.push(license.into());
    }
    Ok(BookmarkCreate {
        title: title.into(),
        url: url.into(),
        created_at: starred.map(str::to_owned),
        metadata,
        tags,
        preview_image: repo["openGraphImageUrl"].as_str().map(str::to_owned),
        ..Default::default()
    })
}

async fn fetch_stars(token: &str, endpoint: &str) -> anyhow::Result<Vec<BookmarkCreate>> {
    let client = reqwest::Client::builder()
        .user_agent("karakeep-sync/0.1")
        .timeout(Duration::from_secs(40))
        .build()?;
    let mut after: Option<String> = None;
    let mut seen = HashSet::new();
    let mut bookmarks = vec![];
    loop {
        let response: Value = client
            .post(endpoint)
            .bearer_auth(token)
            .json(&json!({"query":STARS_QUERY,"variables":{"after":after}}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        anyhow::ensure!(
            response["errors"].as_array().is_none_or(|e| e.is_empty()),
            "GitHub star metadata query failed"
        );
        let connection = &response["data"]["viewer"]["starredRepositories"];
        for edge in connection["edges"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("Missing GitHub star page"))?
        {
            bookmarks.push(bookmark(edge)?);
        }
        if connection["pageInfo"]["hasNextPage"] == false {
            break;
        }
        let cursor = connection["pageInfo"]["endCursor"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Invalid GitHub star cursor"))?
            .to_owned();
        anyhow::ensure!(seen.insert(cursor.clone()), "Repeated GitHub star cursor");
        after = Some(cursor);
    }
    Ok(bookmarks)
}

// Query old saved URLs as well: GitHub redirects repository renames and transfers.
// Reuse the original Karakeep bookmark rather than creating a second copy at the new URL.
async fn existing_repositories(
    token: &str,
    rows: &[Value],
    stars: &HashSet<String>,
) -> anyhow::Result<Vec<(String, Value)>> {
    let mut candidates = vec![];
    for row in rows {
        let Some(raw) = row["content"]["url"].as_str() else {
            continue;
        };
        if stars.contains(&karakeep_client::bookmark_key(raw)) {
            continue;
        }
        let Ok(url) = reqwest::Url::parse(raw) else {
            continue;
        };
        if url.host_str() != Some("github.com") {
            continue;
        }
        let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
        if parts.len() == 2 && parts.iter().all(|p| !p.is_empty()) {
            candidates.push((
                raw.to_owned(),
                parts[0].to_owned(),
                parts[1].trim_end_matches(".git").to_owned(),
            ));
        }
    }
    let client = reqwest::Client::builder()
        .user_agent("karakeep-sync/0.1")
        .timeout(Duration::from_secs(40))
        .build()?;
    let fields = "url nameWithOwner description createdAt pushedAt openGraphImageUrl owner{login} primaryLanguage{name} licenseInfo{spdxId} repositoryTopics(first:100){nodes{topic{name}}}";
    let mut found = vec![];
    for batch in candidates.chunks(20) {
        let queries: Vec<_> = batch
            .iter()
            .enumerate()
            .map(|(i, (_, owner, name))| {
                format!(
                    "r{i}:repository(owner:{},name:{}){{{fields}}}",
                    json!(owner),
                    json!(name)
                )
            })
            .collect();
        let response: Value = client
            .post("https://api.github.com/graphql")
            .bearer_auth(token)
            .json(&json!({"query":format!("query {{{}}}",queries.join(" "))}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if let Some(errors) = response["errors"].as_array() {
            anyhow::ensure!(
                errors.iter().all(|e| e["type"] == "NOT_FOUND"),
                "GitHub repository metadata query failed"
            );
        }
        anyhow::ensure!(
            response["data"].is_object(),
            "Missing GitHub repository metadata"
        );
        for (i, (url, _, _)) in batch.iter().enumerate() {
            let repo = &response["data"][format!("r{i}")];
            if repo.is_object() {
                found.push((url.clone(), repo.clone()));
            }
        }
    }
    Ok(found)
}

#[async_trait]
impl super::Plugin for GithubStars {
    fn list_name(&self) -> &'static str {
        "GitHub Starred"
    }
    async fn to_bookmark_stream(
        &self,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = Vec<BookmarkCreate>> + Send>>> {
        let token = settings::get_settings()
            .github
            .token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("GitHub token is not set"))?;
        let bookmarks = fetch_stars(token, "https://api.github.com/graphql").await?;
        Ok(Box::pin(futures::stream::iter([bookmarks])))
    }
    async fn sync(&self) -> anyhow::Result<i32> {
        let _guard = super::SYNC_LOCK.lock().await;
        let client = crate::karakeep::get_client();
        let token = settings::get_settings()
            .github
            .token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("GitHub token is not set"))?;
        let stars = fetch_stars(token, "https://api.github.com/graphql").await?;
        client.refresh_bookmarks().await?;
        let rows = client.list_bookmarks(None).await?;
        let keys = stars
            .iter()
            .map(|b| karakeep_client::bookmark_key(&b.url))
            .collect();
        let older = existing_repositories(token, &rows, &keys).await?;
        let mut extra = vec![];
        let mut renamed = 0;
        for (original_url, repo) in older {
            let canonical = repo["url"].as_str().unwrap_or("");
            if let Some(star) = stars.iter().find(|b| {
                karakeep_client::bookmark_key(&b.url) == karakeep_client::bookmark_key(canonical)
            }) {
                // A new URL can have an older saved alias; the existing ID wins.
                if !rows.iter().any(|row| {
                    row["content"]["url"].as_str().is_some_and(|u| {
                        karakeep_client::bookmark_key(u) == karakeep_client::bookmark_key(canonical)
                    })
                }) {
                    client.alias_bookmark(&original_url, &star.url).await;
                    renamed += 1;
                } else {
                    let mut older = repository_bookmark(&repo, star.created_at.as_deref())?;
                    older.url = original_url;
                    extra.push(older);
                }
            } else {
                let mut older = repository_bookmark(&repo, None)?;
                older.url = original_url;
                extra.push(older);
            }
        }
        let list = client.ensure_list_exists(self.list_name()).await?;
        let total = stars.len();
        let old_count = extra.len();
        let mut created = 0;
        for bookmark in stars {
            let (_, new) = client.upsert(&bookmark, &list).await?;
            created += i32::from(new);
        }
        // Older saved repositories can include manually added bookmarks. Enrich
        // their fields without assigning them to a list they did not belong to.
        for bookmark in extra {
            if let Some(row) = rows
                .iter()
                .find(|r| r["content"]["url"].as_str() == Some(&bookmark.url))
            {
                let row = client
                    .sync_metadata(row, &bookmark.metadata, &bookmark.tags)
                    .await?;
                if let Some(image) = bookmark.preview_image {
                    if let Err(error) = client.sync_preview(&row, &image).await {
                        tracing::warn!("Older GitHub preview deferred: {error}");
                    }
                }
            }
        }
        self.sync_additional_lists(client).await?;
        tracing::info!(
            "GitHub metadata sync complete (stars={}, older={}, renamed={}, created={})",
            total,
            old_count,
            renamed,
            created
        );
        Ok(created)
    }
    fn is_activated(&self) -> bool {
        settings::get_settings()
            .github
            .token
            .as_ref()
            .is_some_and(|t| !t.is_empty())
    }
    fn recurring_schedule(&self) -> String {
        settings::get_settings().github.schedule.clone()
    }
    async fn sync_additional_lists(&self, client: &KarakeepClient) -> anyhow::Result<()> {
        let settings = &settings::get_settings().github;
        if settings.synclists {
            let token = settings
                .token
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("GitHub token is not set"))?;
            super::github_lists::GithubLists::new(token, "https://api.github.com/graphql")?
                .sync(client)
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_partial_json, method},
    };
    fn edge(name: &str, date: &str) -> Value {
        json!({"starredAt":date,"node":{"url":format!("https://github.com/{name}"),"nameWithOwner":name,
            "description":"Repository description","createdAt":"2010-01-01T00:00:00Z","pushedAt":"2020-01-01T00:00:00Z",
            "openGraphImageUrl":"https://opengraph.githubassets.com/hash/owner/repo","owner":{"login":"owner"},
            "primaryLanguage":{"name":"Rust"},"licenseInfo":{"spdxId":"MIT"},
            "repositoryTopics":{"nodes":[{"topic":{"name":"reading"}}]}}})
    }
    #[tokio::test]
    async fn pages_all_stars_for_backfill_and_uses_star_time_instead_of_repository_creation() {
        let server = MockServer::start().await;
        for (after, next, name) in [
            (Value::Null, json!("two"), "owner/one"),
            (json!("two"), Value::Null, "owner/two"),
        ] {
            Mock::given(method("POST")).and(body_partial_json(json!({"variables":{"after":after}})))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":{"viewer":{"starredRepositories":{
                    "edges":[edge(name,"2022-03-04T05:06:07Z")],"pageInfo":{"hasNextPage":!next.is_null(),"endCursor":next}
                }}}}))).expect(1).mount(&server).await;
        }
        let stars = fetch_stars("fake", &server.uri()).await.unwrap();
        assert_eq!(stars.len(), 2);
        assert_eq!(stars[0].metadata["createdAt"], "2022-03-04T05:06:07Z");
        assert_eq!(stars[0].metadata["datePublished"], "2010-01-01T00:00:00Z");
        assert_eq!(stars[0].tags, vec!["reading", "Rust", "MIT"]);
        assert!(stars[0].metadata.get("note").is_none());
    }
    #[tokio::test]
    async fn graphql_partial_errors_fail_before_writing_any_bookmarks() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"errors":[{"message":"permission denied"}],"data":null})),
            )
            .mount(&server)
            .await;
        assert!(fetch_stars("fake", &server.uri()).await.is_err());
    }
}
