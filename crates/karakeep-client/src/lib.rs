use reqwest::{Client, Method, Response, Url};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

const SOURCE_PREFIX: &str = "karakeep-sync-source: ";

fn list_source(list: &serde_json::Value) -> Option<&str> {
    list.get("description")?
        .as_str()?
        .lines()
        .find_map(|line| line.strip_prefix(SOURCE_PREFIX))
}

pub struct KarakeepClient {
    url: String,
    client: Client,
    images: Client,
    state: PathBuf,
    index: Mutex<Option<HashMap<String, Value>>>,
    duplicates: Mutex<HashMap<String, Vec<Value>>>,
    upload_interval: Duration,
    previews: Mutex<Option<Instant>>,
}

#[derive(Default)]
pub struct BookmarkCreate {
    pub title: String,
    pub url: String,
    pub created_at: Option<String>,
    pub preview_image: Option<String>,
    pub reader_html: Option<String>,
    /// Only native metadata fields; notes and user flags are never accepted here.
    pub metadata: Value,
    pub tags: Vec<String>,
}

async fn parse_response(resp: Response) -> anyhow::Result<Value> {
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("Karakeep API returned {status}");
    }
    let body = resp.text().await?;
    if body.is_empty() {
        return Ok(Value::Null);
    }
    Ok(serde_json::from_str(&body)?)
}

/// Match equivalent Reddit permalinks without their changing title slug, and GitHub's
/// case-insensitive repository names. Query strings don't distinguish these bookmarks.
pub fn bookmark_key(url: &str) -> String {
    let Ok(mut u) = Url::parse(url) else {
        return url.into();
    };
    let host = u.host_str().unwrap_or("").to_ascii_lowercase();
    let parts: Vec<_> = u.path().trim_matches('/').split('/').collect();
    if host == "github.com" && parts.len() == 2 {
        return format!(
            "github:{}/{}",
            parts[0].to_lowercase(),
            parts[1].trim_end_matches(".git").to_lowercase()
        );
    }
    if host == "redd.it" && parts.len() == 1 {
        return format!("reddit:{}", parts[0].to_lowercase());
    }
    if host == "reddit.com" || host.ends_with(".reddit.com") {
        if let Some(i) = parts.iter().position(|p| *p == "comments") {
            if let Some(id) = parts.get(i + 1) {
                let comment = parts
                    .get(i + 3)
                    .filter(|c| !c.is_empty())
                    .map(|c| format!(":{}", c.to_lowercase()))
                    .unwrap_or_default();
                return format!("reddit:{}{comment}", id.to_lowercase());
            }
        }
    }
    u.set_fragment(None);
    let path = u.path().trim_end_matches('/').to_owned();
    u.set_path(&path);
    u.to_string()
}

pub fn reddit_image_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw).ok()?;
    let host = url.host_str()?;
    (url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port_or_known_default() == Some(443)
        && (host.ends_with(".redd.it") || host.ends_with(".redditmedia.com")))
    .then_some(url)
}

pub fn source_image_url(raw: &str) -> Option<Url> {
    reddit_image_url(raw).or_else(|| {
        let url = Url::parse(raw).ok()?;
        (url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.port_or_known_default() == Some(443)
            && matches!(
                url.host_str()?,
                "opengraph.githubassets.com"
                    | "repository-images.githubusercontent.com"
                    | "user-images.githubusercontent.com"
            ))
        .then_some(url)
    })
}

pub fn blocked_reddit_capture(row: &Value) -> bool {
    let Some(url) = row["content"]["url"]
        .as_str()
        .and_then(|u| Url::parse(u).ok())
    else {
        return false;
    };
    let host = url.host_str().unwrap_or("");
    if host != "redd.it" && host != "reddit.com" && !host.ends_with(".reddit.com") {
        return false;
    }
    let title = row["content"]["title"]
        .as_str()
        .unwrap_or("")
        .to_lowercase();
    [
        "prove your humanity",
        "too many requests",
        "blocked by network security",
        "whoa there, pardner",
        "access denied",
        "just a moment",
    ]
    .iter()
    .any(|marker| title.contains(marker))
}

fn blocked_summary(summary: &str) -> bool {
    let summary = summary.to_lowercase();
    [
        "prove your humanity",
        "too many requests",
        "rate limit",
        "rate-limit",
        "http 429",
        "http error 429",
        "429 error",
        "access denied",
        "verify you are human",
        "verify that you are human",
        "human verification",
        "captcha",
        "blocked by network security",
        "prove they are human",
        "prove you are human",
        "security check",
        "bot-verification",
        "anti-bot",
        "humanity",
        "verify they are human",
        "excessive requests",
        "excessive number of requests",
        "temporarily blocked",
        "temporary block",
    ]
    .iter()
    .any(|marker| summary.contains(marker))
}

fn reddit_excerpt(metadata: &Value) -> Option<String> {
    let text = metadata["description"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| metadata["title"].as_str().filter(|s| !s.trim().is_empty()))?;
    let first = text.split("\n\n").find(|s| !s.trim().is_empty())?.trim();
    let mut excerpt: String = first.chars().take(600).collect();
    if first.chars().count() > 600 {
        if let Some(space) = excerpt.rfind(char::is_whitespace) {
            excerpt.truncate(space);
        }
        excerpt.push('…');
    }
    Some(excerpt)
}

fn metadata_patch(row: &Value, metadata: &Value) -> anyhow::Result<Value> {
    let mut patch = serde_json::Map::new();
    if metadata.is_null() {
        return Ok(Value::Object(patch));
    }
    let fields = metadata
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Invalid metadata"))?;
    for (key, value) in fields {
        anyhow::ensure!(
            matches!(
                key.as_str(),
                "title"
                    | "description"
                    | "author"
                    | "publisher"
                    | "datePublished"
                    | "dateModified"
                    | "createdAt"
            ),
            "Field is not source metadata: {key}"
        );
        let Some(text) = value.as_str().filter(|s| !s.trim().is_empty()) else {
            continue;
        };
        let old = if matches!(key.as_str(), "title" | "createdAt") {
            &row[key]
        } else {
            &row["content"][key]
        };
        let equal = old == value
            || (matches!(key.as_str(), "datePublished" | "dateModified" | "createdAt")
                && old.as_str().is_some_and(|old| {
                    match (
                        chrono::DateTime::parse_from_rfc3339(old),
                        chrono::DateTime::parse_from_rfc3339(text),
                    ) {
                        (Ok(a), Ok(b)) => a == b,
                        _ => false,
                    }
                }));
        if !equal {
            patch.insert(key.clone(), value.clone());
        }
    }
    Ok(Value::Object(patch))
}

pub fn write_private_json(path: &Path, value: &Value) -> anyhow::Result<()> {
    std::fs::create_dir_all(
        path.parent()
            .ok_or_else(|| anyhow::anyhow!("State path needs a parent"))?,
    )?;
    let temporary = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    serde_json::to_writer(&mut file, value)?;
    file.flush()?;
    file.sync_all()?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

pub fn read_state(path: &Path) -> anyhow::Result<Value> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e.into()),
    }
}

impl KarakeepClient {
    pub fn new(url: &str, auth_token: &str) -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        let mut auth = reqwest::header::HeaderValue::from_str(&format!("Bearer {auth_token}"))
            .expect("Invalid API key");
        auth.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, auth);
        let client = Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(40))
            .build()
            .unwrap();
        // A separate, unauthenticated client prevents leaking Karakeep credentials to Reddit.
        let images = Client::builder()
            .user_agent("karakeep-sync/0.1 by u/doshisid")
            .timeout(Duration::from_secs(40))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 5
                    || source_image_url(attempt.url().as_str()).is_none()
                {
                    attempt.stop()
                } else {
                    attempt.follow()
                }
            }))
            .build()
            .unwrap();
        Self {
            url: url.trim_end_matches('/').into(),
            client,
            images,
            state: "/data".into(),
            index: Mutex::new(None),
            duplicates: Mutex::new(HashMap::new()),
            upload_interval: Duration::from_millis(
                std::env::var("KS_UPLOAD_INTERVAL_MS")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(2200),
            ),
            previews: Mutex::new(None),
        }
    }

    pub fn with_state(mut self, path: &str) -> Self {
        self.state = path.into();
        self
    }
    pub fn state_path(&self, name: &str) -> PathBuf {
        self.state.join(name)
    }
    pub async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> anyhow::Result<Value> {
        for attempt in 0..4 {
            let mut request = self
                .client
                .request(method.clone(), format!("{}/api/v1{path}", self.url));
            if let Some(body) = body {
                request = request.json(body);
            }
            let response = request.send().await?;
            if response.status() != reqwest::StatusCode::TOO_MANY_REQUESTS || attempt == 3 {
                return parse_response(response).await;
            }
            let delay = response
                .headers()
                .get("retry-after")
                .and_then(|h| h.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(30)
                .clamp(1, 60);
            tracing::warn!("Karakeep rate limit; pausing metadata requests");
            tokio::time::sleep(Duration::from_secs(delay)).await;
        }
        unreachable!()
    }
    pub async fn lists(&self) -> anyhow::Result<Vec<Value>> {
        let value = self.request(Method::GET, "/lists", None).await?;
        value["lists"]
            .as_array()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Invalid Karakeep lists response"))
    }
    pub async fn list_bookmarks(&self, list: Option<&str>) -> anyhow::Result<Vec<Value>> {
        let mut out = vec![];
        let mut cursor: Option<String> = None;
        let mut seen = std::collections::HashSet::new();
        loop {
            let path = list
                .map(|id| format!("/lists/{id}/bookmarks"))
                .unwrap_or_else(|| "/bookmarks".into());
            let mut url = Url::parse(&format!("{}/api/v1{path}", self.url))?;
            url.query_pairs_mut()
                .append_pair("limit", "100")
                .append_pair("includeContent", "false");
            if let Some(cursor) = &cursor {
                url.query_pairs_mut().append_pair("cursor", cursor);
            }
            let d = parse_response(self.client.get(url).send().await?).await?;
            out.extend(
                d["bookmarks"]
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("Invalid bookmark page"))?
                    .iter()
                    .cloned(),
            );
            cursor = d["nextCursor"].as_str().map(str::to_owned);
            let Some(ref c) = cursor else { break };
            anyhow::ensure!(seen.insert(c.clone()), "Repeated Karakeep cursor");
        }
        Ok(out)
    }
    pub async fn refresh_bookmarks(&self) -> anyhow::Result<()> {
        let mut index = self.index.lock().await;
        let bookmarks = self.list_bookmarks(None).await?;
        let mut map = HashMap::new();
        let mut duplicates: HashMap<String, Vec<Value>> = HashMap::new();
        for b in bookmarks {
            if let Some(url) = b["content"]["url"].as_str() {
                let key = bookmark_key(url);
                match map.entry(key.clone()) {
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(b);
                    }
                    std::collections::hash_map::Entry::Occupied(_) => {
                        duplicates.entry(key).or_default().push(b);
                    }
                }
            }
        }
        *self.duplicates.lock().await = duplicates;
        *index = Some(map);
        Ok(())
    }
    pub async fn alias_bookmark(&self, original: &str, canonical: &str) {
        if let Some(index) = self.index.lock().await.as_mut() {
            if let Some(row) = index.get(&bookmark_key(original)).cloned() {
                index.entry(bookmark_key(canonical)).or_insert(row);
            }
        }
    }
    pub async fn create_bookmark(&self, bookmark: &BookmarkCreate) -> anyhow::Result<String> {
        let api_url = format!("{}/api/v1/bookmarks", self.url);
        let mut params = serde_json::json!({
            "type": "link",
            "title": bookmark.title,
            "url": bookmark.url,
        });

        if bookmark.created_at.is_some() {
            params.as_object_mut().unwrap().insert(
                "createdAt".to_string(),
                bookmark.created_at.clone().unwrap().into(),
            );
        }

        let resp = parse_response(self.client.post(&api_url).json(&params).send().await?).await?;

        resp.get("id")
            .and_then(|id| id.as_str())
            .map(|id| id.to_string())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Failed to create bookmark, response did not contain an ID: {resp:?}"
                )
            })
    }

    pub async fn check_exists_bookmark(
        &self,
        bookmark_url: &str,
    ) -> anyhow::Result<Option<String>> {
        // Full-text search with limit=1 can return another repository ahead of the
        // exact URL. Recreating an existing URL re-saves it and changes its date.
        let exact = self
            .client
            .get(format!("{}/api/v1/bookmarks/check-url", self.url))
            .query(&[("url", bookmark_url)])
            .send()
            .await?;
        if exact.status() != reqwest::StatusCode::NOT_FOUND {
            let exact = parse_response(exact).await?;
            match exact.get("bookmarkId") {
                // Some Karakeep versions miss a stored bare origin after adding
                // a trailing slash to their lookup. Check normalized URL equality
                // through the compatibility path before creating a bookmark.
                Some(Value::Null) => {}
                Some(Value::String(id)) => return Ok(Some(id.clone())),
                _ => anyhow::bail!("Invalid exact-URL lookup response"),
            }
        }
        // Compatibility for Karakeep versions that predate exact-URL lookup.
        let url = format!("{}/api/v1/bookmarks/search", self.url);

        let resp = parse_response(
            self.client
                .get(&url)
                .query(&[
                    ("q", bookmark_url),
                    ("includeContent", "false"),
                    ("limit", "1"),
                ])
                .send()
                .await?,
        )
        .await?;

        let bookmarks = resp.get("bookmarks").and_then(|b| b.as_array()).unwrap();

        if bookmarks.is_empty() {
            return Ok(None);
        }

        let url_to_check = bookmark_url.parse();
        if url_to_check.is_err() {
            return Ok(None);
        }
        let url_to_check: Url = url_to_check.unwrap();
        let bookmark_url = bookmarks[0]
            .get("content")
            .and_then(|c| c.get("url"))
            .and_then(|u| u.as_str())
            .unwrap_or("")
            .parse();
        if bookmark_url.is_err() {
            return Ok(None);
        }
        let bookmark_url: Url = bookmark_url.unwrap();

        if bookmark_url == url_to_check {
            let id = bookmarks[0]
                .get("id")
                .and_then(|id| id.as_str())
                .map(|s| s.to_string());
            return Ok(id);
        }

        Ok(None)
    }

    pub async fn ensure_list_exists(&self, list_name: &str) -> anyhow::Result<String> {
        self.ensure_list_exists_under(list_name, None, None).await
    }

    pub async fn ensure_tracked_child_list_exists(
        &self,
        list_name: &str,
        parent_id: &str,
        source_key: &str,
    ) -> anyhow::Result<String> {
        self.ensure_list_exists_under(list_name, Some(parent_id), Some(source_key))
            .await
    }

    async fn ensure_list_exists_under(
        &self,
        list_name: &str,
        parent_id: Option<&str>,
        source_key: Option<&str>,
    ) -> anyhow::Result<String> {
        let url = format!("{}/api/v1/lists", self.url);

        let resp = parse_response(self.client.get(&url).send().await?).await?;

        let lists = resp.get("lists").and_then(|l| l.as_array()).unwrap();

        let children: Vec<_> = lists
            .iter()
            .filter(|list| list.get("parentId").and_then(|p| p.as_str()) == parent_id)
            .collect();
        let mut matches: Vec<_> = children
            .iter()
            .copied()
            .filter(|list| source_key.is_some_and(|key| list_source(list) == Some(key)))
            .collect();
        if matches.is_empty() {
            matches = children
                .into_iter()
                .filter(|list| {
                    list.get("name").and_then(|n| n.as_str()) == Some(list_name)
                        && (source_key.is_none() || list_source(list).is_none())
                })
                .collect();
        }
        if source_key.is_some() && matches.len() > 1 {
            anyhow::bail!("Multiple Karakeep lists match source {source_key:?}");
        }
        if let Some(list) = matches.first() {
            let id = list
                .get("id")
                .and_then(|id| id.as_str())
                .ok_or_else(|| anyhow::anyhow!("Karakeep list has no ID"))?;
            let mut changes = serde_json::Map::new();
            if list.get("name").and_then(|n| n.as_str()) != Some(list_name) {
                changes.insert("name".into(), list_name.into());
            }
            if let Some(key) = source_key.filter(|_| list_source(list).is_none()) {
                let description = list
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("");
                let separator = if description.is_empty() { "" } else { "\n" };
                changes.insert(
                    "description".into(),
                    format!("{description}{separator}{SOURCE_PREFIX}{key}").into(),
                );
            }
            if !changes.is_empty() {
                parse_response(
                    self.client
                        .patch(format!("{url}/{id}"))
                        .json(&changes)
                        .send()
                        .await?,
                )
                .await?;
            }
            return Ok(id.to_string());
        }

        // If not, create it
        let mut params = serde_json::json!({
            "name": list_name,
            "description": "Auto-created list from karakeep-sync",
            "icon": "🚀"
        });
        if let Some(parent_id) = parent_id {
            params["parentId"] = parent_id.into();
        }
        if let Some(key) = source_key {
            params["description"] =
                format!("Auto-created list from karakeep-sync\n{SOURCE_PREFIX}{key}").into();
        }

        let resp = parse_response(self.client.post(&url).json(&params).send().await?).await?;

        resp.get("id")
            .and_then(|id| id.as_str())
            .map(|id| id.to_string())
            .ok_or_else(|| {
                anyhow::anyhow!("Failed to create list, response did not contain an ID: {resp:?}")
            })
    }

    pub async fn ensure_bookmark_in_list(
        &self,
        bookmark_id: &str,
        list_id: &str,
    ) -> anyhow::Result<()> {
        let url = format!(
            "{}/api/v1/lists/{}/bookmarks/{}",
            self.url, list_id, bookmark_id
        );
        self.client.put(&url).send().await?.error_for_status()?;

        Ok(())
    }

    pub async fn upsert_bookmark_to_list(
        &self,
        bookmark: &BookmarkCreate,
        list_id: &str,
    ) -> anyhow::Result<bool> {
        // Check if bookmark exists by URL
        tracing::debug!("checking if bookmark exists: {}", &bookmark.url);
        let cached = self
            .index
            .lock()
            .await
            .as_ref()
            .and_then(|index| index.get(&bookmark_key(&bookmark.url)))
            .and_then(|row| row["id"].as_str())
            .map(str::to_owned);
        let exists = match cached {
            Some(id) => Some(id),
            None => self.check_exists_bookmark(&bookmark.url).await?,
        };
        let to_create = exists.is_none();
        tracing::debug!("bookmark exists: {}", !to_create);

        // If it doesn't exist, create it
        let bookmark_id: String = if to_create {
            tracing::info!("creating bookmark: {} - {}", &bookmark.title, &bookmark.url);
            self.create_bookmark(bookmark).await?
        } else {
            exists.unwrap()
        };

        tracing::debug!("adding bookmark: {} to list: {}", &bookmark_id, list_id);
        // Either way, make sure that the bookmark is in the specified list
        self.ensure_bookmark_in_list(&bookmark_id, list_id).await?;
        // Return true if created, false if already existed
        Ok(to_create)
    }
    pub async fn upsert(
        &self,
        bookmark: &BookmarkCreate,
        list: &str,
    ) -> anyhow::Result<(String, bool)> {
        if self.index.lock().await.is_none() {
            self.refresh_bookmarks().await?;
        }
        let key = bookmark_key(&bookmark.url);
        let (mut row, created) = {
            let mut guard = self.index.lock().await;
            let index = guard.as_mut().unwrap();
            if let Some(row) = index.get(&key) {
                (row.clone(), false)
            } else {
                let mut params = json!({"type":"link","title":bookmark.title,"url":bookmark.url});
                if let Some(date) = &bookmark.created_at {
                    params["createdAt"] = date.clone().into();
                }
                let row = self
                    .request(Method::POST, "/bookmarks", Some(&params))
                    .await?;
                index.insert(key.clone(), row.clone());
                (row, true)
            }
        };
        let bookmark_id = id(&row)?;
        self.ensure_bookmark_in_list(&bookmark_id, list).await?;
        row = self
            .sync_metadata(&row, &bookmark.metadata, &bookmark.tags)
            .await?;
        self.enrich_content(&row, bookmark).await?;
        let duplicates = self
            .duplicates
            .lock()
            .await
            .get(&key)
            .cloned()
            .unwrap_or_default();
        for duplicate in duplicates {
            let duplicate = self
                .sync_metadata(&duplicate, &bookmark.metadata, &bookmark.tags)
                .await?;
            self.enrich_content(&duplicate, bookmark).await?;
        }
        Ok((bookmark_id, created))
    }
    async fn enrich_content(&self, row: &Value, bookmark: &BookmarkCreate) -> anyhow::Result<()> {
        if let Some(image) = &bookmark.preview_image {
            if let Err(error) = self.sync_preview(row, image).await {
                tracing::warn!(bookmark=%id(row)?, "Source preview deferred: {error}");
            }
        }
        self.repair_reddit_capture(row, &bookmark.metadata).await?;
        if let Some(html) = &bookmark.reader_html {
            if let Err(error) = self.sync_reader(row, html).await {
                tracing::warn!(bookmark=%id(row)?, "Reddit reader deferred: {error}");
            }
        }
        Ok(())
    }
    async fn cache_row(&self, row: &Value) {
        if let Some(cache) = self.index.lock().await.as_mut() {
            for entry in cache.values_mut().filter(|e| e["id"] == row["id"]) {
                *entry = row.clone();
            }
        }
        for entries in self.duplicates.lock().await.values_mut() {
            for entry in entries.iter_mut().filter(|e| e["id"] == row["id"]) {
                *entry = row.clone();
            }
        }
    }
    async fn sync_reader(&self, row: &Value, html: &str) -> anyhow::Result<bool> {
        let already_pending = row["content"]["crawlStatus"] == "pending";
        use sha2::{Digest, Sha256};
        let bookmark_id = id(row)?;
        let path = self.state_path("reddit-reader.json");
        let mut cache = read_state(&path)?;
        let recorded = &cache[&bookmark_id];
        let needs_reader = blocked_reddit_capture(row)
            || recorded.is_object()
            || row["content"]["crawlStatus"] == "failure";
        if !needs_reader {
            return Ok(false);
        }
        let hash = format!("{:x}", Sha256::digest(html.as_bytes()));
        if recorded["hash"] == hash
            && recorded["queued"] == true
            && row["content"]["crawlStatus"] != "failure"
        {
            return Ok(false);
        }
        let fresh = self
            .request(
                Method::GET,
                &format!("/bookmarks/{bookmark_id}?includeContent=true"),
                None,
            )
            .await?;
        let backup = self
            .state_path("reddit-reader-before")
            .join(format!("{bookmark_id}.json"));
        if !backup.exists() {
            write_private_json(&backup, &fresh)?;
        }
        let stored = self
            .state_path("reddit-reader-documents")
            .join(format!("{bookmark_id}.json"));
        write_private_json(&stored, &json!({"html":html,"hash":hash}))?;
        let asset = if recorded["hash"] == hash && recorded["asset_id"].is_string() {
            recorded["asset_id"].as_str().unwrap().to_owned()
        } else {
            let mut last = self.previews.lock().await;
            if let Some(instant) = *last {
                tokio::time::sleep(self.upload_interval.saturating_sub(instant.elapsed())).await;
            }
            let part = reqwest::multipart::Part::text(html.to_owned())
                .file_name("reddit-reader.html")
                .mime_str("text/html")?;
            let response = self
                .client
                .post(format!("{}/api/v1/assets", self.url))
                .multipart(reqwest::multipart::Form::new().part("file", part))
                .send()
                .await?;
            *last = Some(Instant::now());
            let response = parse_response(response).await?;
            response["assetId"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Reader upload has no asset ID"))?
                .to_owned()
        };
        cache[&bookmark_id] = json!({"hash":hash,"asset_id":asset,"queued":false});
        write_private_json(&path, &cache)?;
        let attached = fresh["assets"]
            .as_array()
            .is_some_and(|a| a.iter().any(|a| a["id"] == asset));
        if !attached {
            let own_old = fresh["assets"].as_array().into_iter().flatten().find(|a| {
                a["assetType"] == "precrawledArchive" && a["fileName"] == "reddit-reader.html"
            });
            if let Some(old) = own_old {
                self.request(
                    Method::PUT,
                    &format!("/bookmarks/{bookmark_id}/assets/{}", id(old)?),
                    Some(&json!({"assetId":asset})),
                )
                .await?;
            } else {
                self.request(
                    Method::POST,
                    &format!("/bookmarks/{bookmark_id}/assets"),
                    Some(&json!({"id":asset,"assetType":"precrawledArchive"})),
                )
                .await?;
            }
        }
        // Unlike the SingleFile create endpoint, attachment + recrawl never resaves
        // the bookmark, changes its date, or clears its archive flag.
        if !already_pending {
            let response = self
                .client
                .post(format!("{}/api/trpc/bookmarks.recrawlBookmark", self.url))
                .json(&json!({"json":{"bookmarkId":bookmark_id}}))
                .send()
                .await?;
            let response = parse_response(response).await?;
            anyhow::ensure!(
                response.get("error").is_none(),
                "Reader rebuild was rejected"
            );
        }
        cache[&bookmark_id]["queued"] = true.into();
        write_private_json(&path, &cache)?;
        tracing::info!(bookmark=%bookmark_id,"Reddit full reader content queued");
        Ok(true)
    }
    pub async fn retry_readers(&self) -> anyhow::Result<()> {
        let directory = self.state_path("reddit-reader-documents");
        if !directory.exists() {
            return Ok(());
        }
        let cache = read_state(&self.state_path("reddit-reader.json"))?;
        for file in std::fs::read_dir(directory)? {
            let file = file?;
            let path = file.path();
            let Some(bookmark_id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if cache[bookmark_id]["queued"] == true {
                continue;
            }
            let document = read_state(&path)?;
            let Some(html) = document["html"].as_str() else {
                continue;
            };
            let row = self
                .request(Method::GET, &format!("/bookmarks/{bookmark_id}"), None)
                .await?;
            if let Err(error) = self.sync_reader(&row, html).await {
                tracing::warn!("Reddit reader retry deferred: {error}");
            }
        }
        Ok(())
    }
    pub async fn repair_reddit_capture(
        &self,
        row: &Value,
        metadata: &Value,
    ) -> anyhow::Result<bool> {
        if !blocked_reddit_capture(row) || row["content"]["crawlStatus"] == "pending" {
            return Ok(false);
        }
        let replacement = reddit_excerpt(metadata)
            .map(Value::String)
            .unwrap_or(Value::Null);
        let needs_summary = |row: &Value| {
            (row["summary"].as_str().is_some_and(blocked_summary) || row["summary"].is_null())
                && row["summary"] != replacement
        };
        if row["content"]["screenshotAssetId"].is_null() && !needs_summary(row) {
            return Ok(false);
        }
        let bookmark_id = id(row)?;
        // Read current assets before detaching anything; previews can finish while syncing.
        let fresh = self
            .request(
                Method::GET,
                &format!("/bookmarks/{bookmark_id}?includeContent=true"),
                None,
            )
            .await?;
        let screenshot = fresh["content"]["screenshotAssetId"].as_str().filter(|id| {
            !fresh["assets"].as_array().is_some_and(|assets| {
                assets.iter().any(|asset| {
                    asset["id"].as_str() == Some(*id)
                        && asset["fileName"].as_str().is_some_and(|s| !s.is_empty())
                })
            })
        });
        let summary_change = needs_summary(&fresh);
        if screenshot.is_none() && !summary_change {
            return Ok(false);
        }
        let backup = self.state_path("reddit-capture-before").join(&bookmark_id);
        if !backup.with_extension("json").exists() {
            write_private_json(&backup.with_extension("json"), &fresh)?;
        }
        if let Some(asset) = screenshot {
            std::fs::create_dir_all(&backup)?;
            let image = backup.join(format!("screenshot-{asset}"));
            if !image.exists() {
                self.backup_original_asset(asset, &image).await?;
            }
            self.request(
                Method::DELETE,
                &format!("/bookmarks/{bookmark_id}/assets/{asset}"),
                None,
            )
            .await?;
        }
        if summary_change {
            self.request(
                Method::PATCH,
                &format!("/bookmarks/{bookmark_id}"),
                Some(&json!({"summary":replacement})),
            )
            .await?;
        }
        let updated = self
            .request(Method::GET, &format!("/bookmarks/{bookmark_id}"), None)
            .await?;
        self.cache_row(&updated).await;
        tracing::info!(bookmark=%bookmark_id, "Reddit blocked capture repaired");
        Ok(true)
    }
    pub async fn sync_metadata(
        &self,
        row: &Value,
        metadata: &Value,
        tags: &[String],
    ) -> anyhow::Result<Value> {
        let patch = metadata_patch(row, metadata)?;
        let mut names = std::collections::BTreeMap::new();
        for name in tags.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            names.entry(name.to_lowercase()).or_insert(name);
        }
        let current: std::collections::HashSet<_> = row["tags"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| t["name"].as_str())
            .map(str::to_lowercase)
            .collect();
        let missing: Vec<_> = names
            .into_iter()
            .filter(|(key, _)| !current.contains(key))
            .map(|(_, name)| json!({"tagName":name}))
            .collect();
        if patch.as_object().unwrap().is_empty() && missing.is_empty() {
            return Ok(row.clone());
        }
        let bookmark_id = id(row)?;
        let backup = self
            .state_path("metadata-before")
            .join(format!("{bookmark_id}.json"));
        if !backup.exists() {
            write_private_json(&backup, row)?;
        }
        let mut updated = if !patch.as_object().unwrap().is_empty() {
            self.request(
                Method::PATCH,
                &format!("/bookmarks/{bookmark_id}"),
                Some(&patch),
            )
            .await?
        } else {
            row.clone()
        };
        if !missing.is_empty() {
            self.request(
                Method::POST,
                &format!("/bookmarks/{bookmark_id}/tags"),
                Some(&json!({"tags":missing})),
            )
            .await?;
            updated = self
                .request(Method::GET, &format!("/bookmarks/{bookmark_id}"), None)
                .await?;
        }
        self.cache_row(&updated).await;
        tracing::info!(bookmark=%bookmark_id, "Source metadata updated");
        Ok(updated)
    }
    async fn bounded_bytes(mut response: Response) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(
            response.status().is_success(),
            "Image request returned {}",
            response.status()
        );
        const MAX: u64 = 20 * 1024 * 1024;
        anyhow::ensure!(
            response.content_length().is_none_or(|n| n <= MAX),
            "Preview exceeds 20 MiB"
        );
        let mut bytes = vec![];
        while let Some(chunk) = response.chunk().await? {
            anyhow::ensure!(
                (bytes.len() + chunk.len()) as u64 <= MAX,
                "Preview exceeds 20 MiB"
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
    async fn backup_original_asset(&self, asset: &str, path: &Path) -> anyhow::Result<()> {
        use tokio::io::AsyncWriteExt;
        let mut response = self
            .client
            .get(format!("{}/api/v1/assets/{asset}", self.url))
            .send()
            .await?;
        anyhow::ensure!(
            response.status().is_success(),
            "Original-image backup returned {}",
            response.status()
        );
        // Old covers may be large animated GIFs. Stream them to private storage;
        // the 20 MiB limit applies to new preview payloads, not original backups.
        const MAX: u64 = 128 * 1024 * 1024;
        anyhow::ensure!(
            response.content_length().is_none_or(|n| n <= MAX),
            "Original-image backup exceeds 128 MiB"
        );
        let temporary = path.with_extension("partial");
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temporary).await?;
        let mut total = 0_u64;
        while let Some(chunk) = response.chunk().await? {
            total += chunk.len() as u64;
            anyhow::ensure!(total <= MAX, "Original-image backup exceeds 128 MiB");
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        file.sync_all().await?;
        tokio::fs::rename(temporary, path).await?;
        Ok(())
    }
    pub async fn sync_preview(&self, row: &Value, image: &str) -> anyhow::Result<bool> {
        let Some(url) = source_image_url(image) else {
            anyhow::bail!("Invalid Reddit preview URL")
        };
        let bookmark_id = id(row)?;
        let mut last_upload = self.previews.lock().await;
        let github = reddit_image_url(image).is_none();
        let prefix = if github {
            "github-preview"
        } else {
            "reddit-preview"
        };
        let path = self.state_path(&format!("{prefix}s.json"));
        // Karakeep already crawls GitHub reliably. Reuse the same native banner,
        // including its downloaded asset, instead of uploading an identical copy.
        if github {
            let old = row["content"]["imageUrl"]
                .as_str()
                .and_then(|s| Url::parse(s).ok());
            let same = old.as_ref().is_some_and(|old| {
                old == &url
                    || (old.host_str() == Some("opengraph.githubassets.com")
                        && url.host_str() == old.host_str()
                        && old
                            .path_segments()
                            .map(|s| s.rev().take(2).collect::<Vec<_>>())
                            == url
                                .path_segments()
                                .map(|s| s.rev().take(2).collect::<Vec<_>>()))
            });
            if same {
                return Ok(false);
            }
        }
        let mut state = read_state(&path)?;
        let current = row["content"]["imageAssetId"].as_str();
        let recorded = &state[&bookmark_id];
        if recorded["url"] == image && recorded["asset_id"].as_str() == current && current.is_some()
        {
            if recorded["pending"] == true {
                state[&bookmark_id]["pending"] = false.into();
                write_private_json(&path, &state)?;
            }
            return Ok(false);
        }
        // Uploaded files have a filename; crawler-generated banners do not. Keep
        // an owner's custom image, while repairing a crawler that replaced our banner.
        if let Some(assets) = row["assets"].as_array() {
            if assets.iter().any(|asset| {
                asset["id"].as_str() == current
                    && asset["fileName"].as_str().is_some_and(|name| {
                        !name.is_empty()
                            && !name.starts_with("reddit-preview.")
                            && !name.starts_with("github-preview.")
                    })
            }) {
                tracing::info!(bookmark=%bookmark_id,"Keeping the owner's custom preview");
                return Ok(false);
            }
        }
        if recorded["pending"] == true
            && recorded["url"] == image
            && recorded["previous_asset_id"].as_str() == current
        {
            let asset = recorded["asset_id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Pending upload has no asset ID"))?
                .to_owned();
            self.attach_preview(&bookmark_id, current, &asset).await?;
            state[&bookmark_id]["pending"] = false.into();
            write_private_json(&path, &state)?;
            self.cache_preview(row, &asset).await;
            return Ok(true);
        }
        let bytes = Self::bounded_bytes(self.images.get(url).send().await?).await?;
        let (mime, ext) = image_type(&bytes)
            .ok_or_else(|| anyhow::anyhow!("Reddit returned an unsupported image"))?;
        let backup = self.state_path("original-previews").join(&bookmark_id);
        if !backup.with_extension("json").exists() {
            if let Some(asset) = current {
                std::fs::create_dir_all(&backup)?;
                self.backup_original_asset(asset, &backup.join("image"))
                    .await?;
            }
            write_private_json(&backup.with_extension("json"), row)?;
        }
        // Karakeep limits uploads to 30/minute. Serialize and leave headroom for the UI.
        if let Some(last) = *last_upload {
            let wait = self.upload_interval.saturating_sub(last.elapsed());
            tokio::time::sleep(wait).await;
        }
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(format!("{prefix}.{ext}"))
            .mime_str(mime)?;
        let response = self
            .client
            .post(format!("{}/api/v1/assets", self.url))
            .multipart(reqwest::multipart::Form::new().part("file", part))
            .send()
            .await?;
        *last_upload = Some(Instant::now());
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            tokio::time::sleep(Duration::from_secs(65)).await;
            anyhow::bail!("Upload rate limit; will retry next sync");
        }
        let uploaded = parse_response(response).await?;
        let new_asset = uploaded["assetId"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Upload returned no asset ID"))?;
        // Persist a pending upload before attachment so a restart can be diagnosed/recovered.
        state[&bookmark_id] =
            json!({"url":image,"asset_id":new_asset,"previous_asset_id":current,"pending":true});
        write_private_json(&path, &state)?;
        self.attach_preview(&bookmark_id, current, new_asset)
            .await?;
        state[&bookmark_id]["pending"] = false.into();
        write_private_json(&path, &state)?;
        self.cache_preview(row, new_asset).await;
        tracing::info!(bookmark=%bookmark_id,"Source preview installed");
        Ok(true)
    }
    async fn attach_preview(
        &self,
        bookmark_id: &str,
        _current: Option<&str>,
        asset: &str,
    ) -> anyhow::Result<()> {
        // The crawler may have finished while the image was downloading. Re-read
        // the current banner so an initial import cannot leave two competing images.
        let fresh = self
            .request(Method::GET, &format!("/bookmarks/{bookmark_id}"), None)
            .await?;
        let current = fresh["content"]["imageAssetId"].as_str();
        if current == Some(asset) {
            return Ok(());
        }
        if fresh["assets"].as_array().is_some_and(|assets| {
            assets.iter().any(|a| {
                a["id"].as_str() == current
                    && a["fileName"].as_str().is_some_and(|name| {
                        !name.is_empty()
                            && !name.starts_with("reddit-preview.")
                            && !name.starts_with("github-preview.")
                    })
            })
        }) {
            anyhow::bail!("The owner selected a custom preview during synchronization");
        }
        if let Some(old) = current {
            self.request(
                Method::PUT,
                &format!("/bookmarks/{bookmark_id}/assets/{old}"),
                Some(&json!({"assetId":asset})),
            )
            .await?;
        } else {
            self.request(
                Method::POST,
                &format!("/bookmarks/{bookmark_id}/assets"),
                Some(&json!({"id":asset,"assetType":"bannerImage"})),
            )
            .await?;
        }
        Ok(())
    }
    async fn cache_preview(&self, row: &Value, asset: &str) {
        let mut updated = row.clone();
        updated["content"]["imageAssetId"] = asset.into();
        self.cache_row(&updated).await;
    }
}

pub fn id(value: &Value) -> anyhow::Result<String> {
    let id = value["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("API returned no ID"))?;
    anyhow::ensure!(
        !id.is_empty()
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "Invalid bookmark ID"
    );
    Ok(id.into())
}

fn image_type(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if bytes.starts_with(b"\xff\xd8\xff") {
        Some(("image/jpeg", "jpg"))
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("image/png", "png"))
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(("image/gif", "gif"))
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some(("image/webp", "webp"))
    } else {
        None
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, method, path},
    };

    #[test]
    fn equivalent_saved_urls_match_without_collapsing_distinct_comments() {
        assert_eq!(
            bookmark_key("https://old.reddit.com/r/rust/comments/abc/title/?x=1"),
            bookmark_key("https://www.reddit.com/r/rust/comments/abc/changed/")
        );
        assert_eq!(
            bookmark_key("https://redd.it/abc"),
            bookmark_key("https://reddit.com/comments/abc/title")
        );
        assert_ne!(
            bookmark_key("https://reddit.com/r/rust/comments/abc/title/comment1"),
            bookmark_key("https://reddit.com/r/rust/comments/abc/title/comment2")
        );
        assert_eq!(
            bookmark_key("https://github.com/Owner/Repo/"),
            bookmark_key("https://github.com/owner/repo")
        );
    }
    #[test]
    fn preview_hosts_and_image_payloads_are_bounded_to_reddit_images() {
        assert!(reddit_image_url("https://preview.redd.it/p.jpg?s=abc").is_some());
        for url in [
            "https://127.0.0.1/image",
            "https://preview.redd.it.evil.example/image",
            "http://i.redd.it/image",
            "https://user:password@i.redd.it/image",
            "https://i.redd.it:444/image",
        ] {
            assert!(reddit_image_url(url).is_none(), "{url}");
        }
        assert_eq!(image_type(b"\xff\xd8\xffjpeg"), Some(("image/jpeg", "jpg")));
        assert!(image_type(b"<!doctype html>blocked").is_none());
    }
    fn row() -> Value {
        json!({"id":"bookmark1","createdAt":"2020-01-01T00:00:00Z","content":{"url":"https://reddit.com/r/test/comments/abc/title/","imageAssetId":"oldimage"},"assets":[]})
    }

    #[tokio::test]
    async fn existing_dates_are_preserved_when_reusing_a_reddit_alias() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/bookmarks"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"bookmarks":[row()],"nextCursor":null})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/lists/saved/bookmarks/bookmark1"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let client = KarakeepClient::new(&server.uri(), "test-token");
        let bookmark = BookmarkCreate {
            title: "new title".into(),
            url: "https://old.reddit.com/r/test/comments/abc/different-title/".into(),
            created_at: Some("2026-11-16T00:00:00Z".into()),
            ..Default::default()
        };
        assert_eq!(
            client.upsert(&bookmark, "saved").await.unwrap(),
            ("bookmark1".into(), false)
        );
        assert!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|r| r.method != Method::POST && r.method != Method::PATCH)
        );
    }
    #[tokio::test]
    async fn cached_previews_do_not_download_or_upload_again() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let client = KarakeepClient::new(&server.uri(), "test-token")
            .with_state(dir.path().to_str().unwrap());
        let url = "https://preview.redd.it/unreachable.jpg";
        write_private_json(
            &client.state_path("reddit-previews.json"),
            &json!({"bookmark1":{"url":url,"asset_id":"oldimage","pending":false}}),
        )
        .unwrap();
        assert!(!client.sync_preview(&row(), url).await.unwrap());
        assert!(server.received_requests().await.unwrap().is_empty());
    }
    #[tokio::test]
    async fn pending_attachment_is_recovered_without_another_upload() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let client = KarakeepClient::new(&server.uri(), "test-token")
            .with_state(dir.path().to_str().unwrap());
        let url = "https://preview.redd.it/unreachable.jpg";
        write_private_json(&client.state_path("reddit-previews.json"),&json!({"bookmark1":{"url":url,"asset_id":"newimage","previous_asset_id":"oldimage","pending":true}})).unwrap();
        Mock::given(method("GET"))
            .and(path("/api/v1/bookmarks/bookmark1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(row()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/bookmarks/bookmark1/assets/oldimage"))
            .and(body_json(json!({"assetId":"newimage"})))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        assert!(client.sync_preview(&row(), url).await.unwrap());
        assert_eq!(
            read_state(&client.state_path("reddit-previews.json")).unwrap()["bookmark1"]["pending"],
            false
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }
    #[tokio::test]
    async fn an_owner_uploaded_preview_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let client = KarakeepClient::new("http://127.0.0.1:1", "test-token")
            .with_state(dir.path().to_str().unwrap());
        let mut r = row();
        r["assets"] = json!([{"id":"oldimage","fileName":"my-cover.png"}]);
        assert!(
            !client
                .sync_preview(&r, "https://preview.redd.it/unreachable.jpg")
                .await
                .unwrap()
        );
    }
    #[tokio::test]
    async fn reddit_downloader_never_sends_the_karakeep_api_key() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"image"))
            .mount(&server)
            .await;
        let client = KarakeepClient::new("http://127.0.0.1:1", "test-token");
        client.images.get(server.uri()).send().await.unwrap();
        assert!(
            server.received_requests().await.unwrap()[0]
                .headers
                .get("authorization")
                .is_none()
        );
    }
}

#[cfg(test)]
mod exact_lookup_tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path, query_param},
    };
    #[tokio::test]
    async fn exact_github_match_is_not_resaved_or_redated() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/bookmarks/check-url"))
            .and(query_param("url", "https://github.com/owner/repo"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"bookmarkId":"existing"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/lists/list/bookmarks/existing"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let client = KarakeepClient::new(&server.uri(), "test-token");
        let bookmark = BookmarkCreate {
            title: "owner/repo".into(),
            url: "https://github.com/owner/repo".into(),
            created_at: Some("2026-11-16T00:00:00Z".into()),
            ..Default::default()
        };
        assert!(
            !client
                .upsert_bookmark_to_list(&bookmark, "list")
                .await
                .unwrap()
        );
        assert!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|r| r.method != Method::POST && r.method != Method::PATCH)
        );
    }
    #[tokio::test]
    async fn a_failed_exact_lookup_cannot_create_a_duplicate_or_reset_a_date() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/bookmarks/check-url"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let client = KarakeepClient::new(&server.uri(), "test-token");
        assert!(
            client
                .check_exists_bookmark("https://github.com/owner/repo")
                .await
                .is_err()
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[cfg(test)]
mod large_original_tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };
    #[tokio::test]
    async fn large_original_cover_is_backed_up_without_relaxing_the_new_preview_limit() {
        let server = MockServer::start().await;
        let bytes = vec![42_u8; 21 * 1024 * 1024];
        Mock::given(method("GET"))
            .and(path("/api/v1/assets/oldgif"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("original.gif");
        let client = KarakeepClient::new(&server.uri(), "test-token");
        client.backup_original_asset("oldgif", &file).await.unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), bytes);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let response = client
            .client
            .get(format!("{}/api/v1/assets/oldgif", server.uri()))
            .send()
            .await
            .unwrap();
        assert!(KarakeepClient::bounded_bytes(response).await.is_err());
    }
}

#[cfg(test)]
mod normalized_origin_tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };
    #[tokio::test]
    async fn bare_origin_missed_by_exact_api_is_not_resaved_with_a_trailing_slash() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/bookmarks/check-url"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"bookmarkId":null})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/bookmarks/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"bookmarks":[{"id":"existing","content":{"url":"https://example.com"}}]}),
            ))
            .mount(&server)
            .await;
        let client = KarakeepClient::new(&server.uri(), "test-token");
        assert_eq!(
            client
                .check_exists_bookmark("https://example.com/")
                .await
                .unwrap()
                .as_deref(),
            Some("existing")
        );
        assert!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|r| r.method == Method::GET)
        );
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, method, path},
    };

    #[tokio::test]
    async fn backfill_preserves_notes_flags_and_tags_and_a_second_pass_does_not_rewrite() {
        let server = MockServer::start().await;
        let state = tempfile::tempdir().unwrap();
        let old = json!({"id":"saved","title":"Old","createdAt":"2026-11-16T00:00:00Z",
            "note":"My personal notes\nKeep all of this.","archived":true,"favourited":true,
            "tags":[{"id":"mine","name":"Personal","attachedBy":"human"}],
            "content":{"url":"https://github.com/owner/repo","author":null}});
        let mut updated = old.clone();
        updated["createdAt"] = "2020-01-02T03:04:05.000Z".into();
        updated["title"] = "owner/repo".into();
        updated["content"]["author"] = "owner".into();
        updated["tags"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":"rust","name":"Rust","attachedBy":"human"}));
        Mock::given(method("GET"))
            .and(path("/api/v1/bookmarks"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"bookmarks":[old],"nextCursor":null})),
            )
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/lists/list/bookmarks/saved"))
            .respond_with(ResponseTemplate::new(204))
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/api/v1/bookmarks/saved"))
            .and(body_json(
                json!({"title":"owner/repo","author":"owner","createdAt":"2020-01-02T03:04:05Z"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(updated.clone()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/bookmarks/saved/tags"))
            .and(body_json(json!({"tags":[{"tagName":"Rust"}]})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/bookmarks/saved"))
            .respond_with(ResponseTemplate::new(200).set_body_json(updated))
            .expect(1)
            .mount(&server)
            .await;
        let client =
            KarakeepClient::new(&server.uri(), "fake").with_state(state.path().to_str().unwrap());
        let bookmark = BookmarkCreate {
            url: "https://github.com/owner/repo".into(),
            metadata: json!({"title":"owner/repo","author":"owner","createdAt":"2020-01-02T03:04:05Z"}),
            tags: vec!["Rust".into(), "Personal".into()],
            ..Default::default()
        };
        client.upsert(&bookmark, "list").await.unwrap();
        client.upsert(&bookmark, "list").await.unwrap();
        let backup = read_state(&client.state_path("metadata-before/saved.json")).unwrap();
        assert_eq!(backup, old);
        for request in server.received_requests().await.unwrap() {
            if request.method == Method::PATCH {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                for field in ["note", "archived", "favourited", "url", "summary"] {
                    assert!(body.get(field).is_none());
                }
            }
            assert_ne!(request.method, Method::DELETE);
        }
        server.verify().await;
    }
    #[test]
    fn metadata_rejects_notes_and_does_not_clear_missing_fields() {
        assert!(metadata_patch(&json!({}), &json!({"note":"anything"})).is_err());
        assert_eq!(
            metadata_patch(
                &json!({"content":{"author":"existing"}}),
                &json!({"author":null,"description":""})
            )
            .unwrap(),
            json!({})
        );
    }
    #[tokio::test]
    async fn renamed_github_repositories_reuse_the_existing_id_including_in_child_lists() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/api/v1/bookmarks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"bookmarks":[{
                "id":"original","content":{"url":"https://github.com/old/repo"}}],"nextCursor":null}))).mount(&server).await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/lists/child/bookmarks/original"))
            .respond_with(ResponseTemplate::new(204))
            .expect(2)
            .mount(&server)
            .await;
        let client = KarakeepClient::new(&server.uri(), "fake");
        client.refresh_bookmarks().await.unwrap();
        client
            .alias_bookmark("https://github.com/old/repo", "https://github.com/new/repo")
            .await;
        let bookmark = BookmarkCreate {
            url: "https://github.com/new/repo".into(),
            ..Default::default()
        };
        assert_eq!(
            client.upsert(&bookmark, "child").await.unwrap().0,
            "original"
        );
        assert!(
            !client
                .upsert_bookmark_to_list(&bookmark, "child")
                .await
                .unwrap()
        );
        assert!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|r| r.method != Method::POST)
        );
    }
}

#[cfg(test)]
mod capture_tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, method, path, query_param},
    };
    #[tokio::test]
    async fn blocked_capture_loses_only_its_bad_screenshot_and_summary_with_a_recovery_backup() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let old = json!({"id":"bad","title":"Actual post title","note":"Keep my note","archived":true,"favourited":true,
            "createdAt":"2020-01-01T00:00:00Z","summary":"Reddit asks to prove your humanity.",
            "content":{"url":"https://reddit.com/r/test/comments/abc/title/","title":"Reddit - Prove your humanity",
                "screenshotAssetId":"challenge","imageAssetId":"real-preview"},
            "assets":[{"id":"challenge","assetType":"screenshot","fileName":null}]});
        let mut updated = old.clone();
        updated["content"]["screenshotAssetId"] = Value::Null;
        updated["summary"] = "Actual post body about useful tools.".into();
        Mock::given(method("GET"))
            .and(path("/api/v1/bookmarks/bad"))
            .and(query_param("includeContent", "true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(old.clone()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/assets/challenge"))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(b"original screenshot".to_vec()),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/api/v1/bookmarks/bad/assets/challenge"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/api/v1/bookmarks/bad"))
            .and(body_json(
                json!({"summary":"Actual post body about useful tools."}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(updated.clone()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/bookmarks/bad"))
            .respond_with(ResponseTemplate::new(200).set_body_json(updated.clone()))
            .with_priority(10)
            .expect(1)
            .mount(&server)
            .await;
        let client =
            KarakeepClient::new(&server.uri(), "fake").with_state(dir.path().to_str().unwrap());
        let metadata = json!({"title":"Actual post title","description":"Actual post body about useful tools."});
        assert!(client.repair_reddit_capture(&old, &metadata).await.unwrap());
        assert!(
            !client
                .repair_reddit_capture(&updated, &metadata)
                .await
                .unwrap()
        );
        assert_eq!(
            read_state(&client.state_path("reddit-capture-before/bad.json")).unwrap(),
            old
        );
        assert_eq!(
            std::fs::read(client.state_path("reddit-capture-before/bad/screenshot-challenge"))
                .unwrap(),
            b"original screenshot"
        );
        server.verify().await;
    }
    #[tokio::test]
    async fn useful_existing_summary_is_preserved_on_a_previously_cleaned_capture() {
        let server = MockServer::start().await;
        let row = json!({"id":"old","summary":"A useful summary I wrote myself.",
            "content":{"url":"https://reddit.com/r/test/comments/abc/title/","title":"Too Many Requests","screenshotAssetId":null}});
        let client = KarakeepClient::new(&server.uri(), "fake");
        assert!(
            !client
                .repair_reddit_capture(&row, &json!({"description":"Different source text"}))
                .await
                .unwrap()
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }
    #[test]
    fn successful_captures_are_untouched_and_rate_limit_pages_are_recognized() {
        let row = json!({"content":{"url":"https://reddit.com/r/test/comments/abc/title/","title":"A useful post : r/test"}});
        assert!(!blocked_reddit_capture(&row));
        let mut row = row;
        row["content"]["title"] = "Too Many Requests".into();
        assert!(blocked_reddit_capture(&row));
        row["content"]["url"] = "https://example.com/".into();
        assert!(!blocked_reddit_capture(&row));
        assert!(!blocked_summary("A useful summary I wrote myself."));
    }
}

#[cfg(test)]
mod duplicate_tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, method, path},
    };
    #[tokio::test]
    async fn all_existing_reddit_aliases_receive_metadata_without_merging_notes_or_dates() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let rows: Vec<Value> = [
            (
                "one",
                "reddit.com",
                "2020-01-01T00:00:00Z",
                "First personal note",
            ),
            (
                "two",
                "www.reddit.com",
                "2021-01-01T00:00:00Z",
                "Second personal note",
            ),
        ]
        .into_iter()
        .map(|(id, host, date, note)| {
            json!({"id":id,"note":note,"createdAt":date,
                "content":{"url":format!("https://{host}/r/test/comments/abc/title/")}})
        })
        .collect();
        Mock::given(method("GET"))
            .and(path("/api/v1/bookmarks"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"bookmarks":rows,"nextCursor":null})),
            )
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/lists/reddit/bookmarks/one"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        for row in &rows {
            let mut updated = row.clone();
            updated["content"]["author"] = "writer".into();
            Mock::given(method("PATCH"))
                .and(path(format!(
                    "/api/v1/bookmarks/{}",
                    row["id"].as_str().unwrap()
                )))
                .and(body_json(json!({"author":"writer"})))
                .respond_with(ResponseTemplate::new(200).set_body_json(updated))
                .expect(1)
                .mount(&server)
                .await;
        }
        let client =
            KarakeepClient::new(&server.uri(), "fake").with_state(dir.path().to_str().unwrap());
        let item = BookmarkCreate {
            url: "https://old.reddit.com/r/test/comments/abc/new-title/".into(),
            metadata: json!({"author":"writer"}),
            created_at: Some("2030-01-01T00:00:00Z".into()),
            ..Default::default()
        };
        assert!(!client.upsert(&item, "reddit").await.unwrap().1);
        for row in rows {
            assert_eq!(
                read_state(&client.state_path(&format!(
                    "metadata-before/{}.json",
                    row["id"].as_str().unwrap()
                )))
                .unwrap(),
                row
            );
        }
        server.verify().await;
    }
}

#[cfg(test)]
mod reader_tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, method, path},
    };
    #[tokio::test]
    async fn full_reader_import_attaches_and_recrawls_without_resaving_or_changing_dates() {
        for pending in [false, true] {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let row = json!({"id":"saved","note":"Private notes","createdAt":"2020-01-01T00:00:00Z","archived":true,
                "content":{"url":"https://reddit.com/r/test/comments/abc/title/","title":"Reddit - Prove your humanity",
                    "crawlStatus":if pending {"pending"}else{"success"}},"assets":[]});
            Mock::given(method("GET"))
                .and(path("/api/v1/bookmarks/saved"))
                .respond_with(ResponseTemplate::new(200).set_body_json(row.clone()))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/api/v1/assets"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"assetId":"reader"})))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/api/v1/bookmarks/saved/assets"))
                .and(body_json(
                    json!({"id":"reader","assetType":"precrawledArchive"}),
                ))
                .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id":"reader"})))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/api/trpc/bookmarks.recrawlBookmark"))
                .and(body_json(json!({"json":{"bookmarkId":"saved"}})))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"result":{"data":{"json":null}}})),
                )
                .expect(if pending { 0 } else { 1 })
                .mount(&server)
                .await;
            let client =
                KarakeepClient::new(&server.uri(), "fake").with_state(dir.path().to_str().unwrap());
            assert!(
                client
                    .sync_reader(&row, "<article>Complete original message</article>")
                    .await
                    .unwrap()
            );
            assert!(
                !client
                    .sync_reader(&row, "<article>Complete original message</article>")
                    .await
                    .unwrap()
            );
            assert_eq!(
                read_state(&client.state_path("reddit-reader-before/saved.json")).unwrap(),
                row
            );
            for request in server.received_requests().await.unwrap() {
                assert_ne!(request.method, Method::PATCH);
                assert_ne!(request.url.path(), "/api/v1/bookmarks");
            }
            server.verify().await;
        }
    }
}
