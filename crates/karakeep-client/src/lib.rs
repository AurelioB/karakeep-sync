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
    previews: Mutex<Option<Instant>>,
}

#[derive(Default)]
pub struct BookmarkCreate {
    pub title: String,
    pub url: String,
    pub created_at: Option<String>,
    pub preview_image: Option<String>,
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
                    || reddit_image_url(attempt.url().as_str()).is_none()
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
        let mut request = self
            .client
            .request(method, format!("{}/api/v1{path}", self.url));
        if let Some(body) = body {
            request = request.json(body);
        }
        parse_response(request.send().await?).await
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
        for b in bookmarks {
            if let Some(url) = b["content"]["url"].as_str() {
                map.entry(bookmark_key(url)).or_insert(b);
            }
        }
        *index = Some(map);
        Ok(())
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
        let exists = self.check_exists_bookmark(&bookmark.url).await?;
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
        let (row, created) = {
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
        if let Some(image) = &bookmark.preview_image {
            if let Err(error) = self.sync_preview(&row, image).await {
                // A failed image must not stop the saved-post import or mutate dates.
                tracing::warn!(bookmark=%bookmark_id,"Reddit preview deferred: {error}");
            }
        }
        Ok((bookmark_id, created))
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
    pub async fn sync_preview(&self, row: &Value, image: &str) -> anyhow::Result<bool> {
        let Some(url) = reddit_image_url(image) else {
            anyhow::bail!("Invalid Reddit preview URL")
        };
        let bookmark_id = id(row)?;
        let mut last_upload = self.previews.lock().await;
        let path = self.state_path("reddit-previews.json");
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
                        !name.is_empty() && !name.starts_with("reddit-preview.")
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
                let original = Self::bounded_bytes(
                    self.client
                        .get(format!("{}/api/v1/assets/{asset}", self.url))
                        .send()
                        .await?,
                )
                .await?;
                std::fs::create_dir_all(&backup)?;
                let asset_file = backup.join("image");
                std::fs::write(&asset_file, original)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(asset_file, std::fs::Permissions::from_mode(0o600))?;
                }
            }
            write_private_json(&backup.with_extension("json"), row)?;
        }
        // Karakeep limits uploads to 30/minute. Serialize and leave headroom for the UI.
        if let Some(last) = *last_upload {
            let wait = Duration::from_millis(2200).saturating_sub(last.elapsed());
            tokio::time::sleep(wait).await;
        }
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(format!("reddit-preview.{ext}"))
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
        tracing::info!(bookmark=%bookmark_id,"Reddit preview installed");
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
                        !name.is_empty() && !name.starts_with("reddit-preview.")
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
        if let Some(cache) = self.index.lock().await.as_mut() {
            if let Some(row) =
                cache.get_mut(&bookmark_key(row["content"]["url"].as_str().unwrap_or("")))
            {
                row["content"]["imageAssetId"] = asset.into();
            }
        }
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
