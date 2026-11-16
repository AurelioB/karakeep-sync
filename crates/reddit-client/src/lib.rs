use reqwest::header;
use serde::Deserialize;

const TOKEN_URL: &str = "https://www.reddit.com/api/v1/access_token";
const APP_URL: &str = "https://oauth.reddit.com";

pub struct RedditClientRefresher {
    refresh_token: String,
    client: reqwest::Client,

    client_id: String,
    client_secret: String,
}

pub struct RedditClient {
    access_token: String,
    client: reqwest::Client,
    username: String,
}

impl RedditClientRefresher {
    pub fn new(client_id: String, client_secret: String, refresh_token: String) -> Self {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::USER_AGENT,
            header::HeaderValue::from_static("karakeep-sync/0.1 by u/doshisid"),
        );
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap();

        Self {
            refresh_token,
            client,
            client_id,
            client_secret,
        }
    }

    pub async fn refresh(&self, username: String) -> anyhow::Result<RedditClient> {
        let params = [
            ("grant_type", "refresh_token"),
            ("refresh_token", &self.refresh_token),
        ];

        let resp = self
            .client
            .post(TOKEN_URL)
            .basic_auth(&self.client_id, Some(&self.client_secret))
            .form(&params)
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;
        let access_token = resp
            .get("access_token")
            .and_then(|t| t.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!("Reddit returned no access token; check the OAuth authorization")
            })?;

        tracing::info!("Reddit authenticated as u/{}", username);

        Ok(RedditClient {
            access_token: access_token.to_string(),
            client: self.client.clone(),
            username,
        })
    }
}

#[derive(Debug, Deserialize)]
pub struct ListingResponse {
    pub data: ListingData,
}

#[derive(Debug, Deserialize)]
pub struct ListingData {
    pub children: Vec<ListingChild>,
    pub after: Option<String>,
}
#[derive(Debug, Deserialize)]
pub struct ListingChild {
    pub kind: String,
    pub data: ListingChildData,
}
#[derive(Debug, Deserialize)]
pub struct ListingChildData {
    pub title: Option<String>,
    pub permalink: String,
    #[serde(flatten)]
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug)]
pub struct SavedPost {
    pub preview_image: Option<String>,
    pub title: String,
    pub url: String,
}

#[derive(Debug)]
pub struct ListSavedResponse {
    pub posts: Vec<SavedPost>,
    pub after: Option<String>,
}

impl RedditClient {
    pub async fn list_saved(&self, after: Option<&str>) -> anyhow::Result<ListSavedResponse> {
        let mut req = self
            .client
            .get(format!("{}/user/{}/saved", APP_URL, self.username))
            .bearer_auth(&self.access_token)
            .query(&[("limit", "100"), ("raw_json", "1")]);

        if let Some(after) = after {
            req = req.query(&[("after", after)]);
        }

        let resp = req.send().await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            return Err(anyhow::anyhow!("Reddit API returned {status}"));
        }
        let resp = serde_json::from_str::<ListingResponse>(&body)
            .map_err(|e| anyhow::anyhow!("Failed to parse Reddit response: {e}"))?;

        let posts = resp
            .data
            .children
            .into_iter()
            .map(|child| {
                let data = child.data;
                SavedPost {
                    preview_image: preview_image(&serde_json::Value::Object(data.metadata)),
                    title: data
                        .title
                        .unwrap_or_else(|| "(unknown title reddit post)".to_string()),
                    url: format!("https://reddit.com{}", data.permalink),
                }
            })
            .collect::<Vec<_>>();

        Ok({
            ListSavedResponse {
                posts,
                after: resp.data.after,
            }
        })
    }
}

/// Reddit supplies previews separately from the permalink HTML. Prefer a useful
/// still image (including a gallery's first image and a video's poster), not placeholders.
pub fn preview_image(data: &serde_json::Value) -> Option<String> {
    preview_at_depth(data, 0)
}

fn preview_at_depth(data: &serde_json::Value, depth: usize) -> Option<String> {
    fn valid(raw: &str) -> Option<String> {
        let raw = raw.replace("&amp;", "&");
        let url = reqwest::Url::parse(&raw).ok()?;
        let host = url.host_str()?;
        (url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.port_or_known_default() == Some(443)
            && (host.ends_with(".redd.it") || host.ends_with(".redditmedia.com")))
        .then_some(raw)
    }
    fn resolution(items: &serde_json::Value, url_key: &str, width_key: &str) -> Option<String> {
        items
            .as_array()?
            .iter()
            .rev()
            .filter(|r| r[width_key].as_u64().is_some_and(|n| n <= 1600))
            .find_map(|r| r[url_key].as_str().and_then(valid))
    }
    if let Some(images) = data["preview"]["images"].as_array() {
        for image in images {
            if let Some(url) = resolution(&image["resolutions"], "url", "width")
                .or_else(|| image["source"]["url"].as_str().and_then(valid))
            {
                return Some(url);
            }
        }
    }
    if let Some(items) = data["gallery_data"]["items"].as_array() {
        for item in items {
            if let Some(id) = item["media_id"].as_str() {
                let media = &data["media_metadata"][id];
                if let Some(url) = resolution(&media["p"], "u", "x")
                    .or_else(|| media["s"]["u"].as_str().and_then(valid))
                {
                    return Some(url);
                }
            }
        }
    }
    if let Some(raw) = data["url_overridden_by_dest"].as_str() {
        if let Some(url) = valid(raw) {
            let path = reqwest::Url::parse(&url).ok()?.path().to_lowercase();
            if [".jpg", ".jpeg", ".png", ".gif", ".webp"]
                .iter()
                .any(|e| path.ends_with(e))
            {
                return Some(url);
            }
        }
    }
    if depth < 2 {
        if let Some(parents) = data["crosspost_parent_list"].as_array() {
            for parent in parents {
                if let Some(url) = preview_at_depth(parent, depth + 1) {
                    return Some(url);
                }
            }
        }
    }
    data["thumbnail"].as_str().and_then(valid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chooses_a_useful_preview_and_decodes_reddit_html_entities() {
        let data = json!({"preview":{"images":[{"source":{"url":"https://preview.redd.it/huge.jpg"},"resolutions":[
            {"width":320,"url":"https://preview.redd.it/small.jpg"},
            {"width":1080,"url":"https://preview.redd.it/post.jpg?x=1&amp;y=2"},
            {"width":3000,"url":"https://preview.redd.it/large.jpg"}]}]}});
        assert_eq!(
            preview_image(&data).as_deref(),
            Some("https://preview.redd.it/post.jpg?x=1&y=2")
        );
    }
    #[test]
    fn gallery_order_and_crossposts_use_reddit_metadata() {
        let gallery = json!({"gallery_data":{"items":[{"media_id":"second"},{"media_id":"first"}]},
            "media_metadata":{"first":{"s":{"u":"https://i.redd.it/first.png"}},
                "second":{"p":[{"x":1080,"u":"https://preview.redd.it/second.png"}]}}});
        assert_eq!(
            preview_image(&gallery).as_deref(),
            Some("https://preview.redd.it/second.png")
        );
        assert_eq!(
            preview_image(&json!({"crosspost_parent_list":[gallery]})).as_deref(),
            Some("https://preview.redd.it/second.png")
        );
    }
    #[test]
    fn does_not_treat_placeholders_or_external_pages_as_images() {
        for thumbnail in [
            "self",
            "default",
            "nsfw",
            "spoiler",
            "",
            "https://evil.example/image.jpg",
            "http://i.redd.it/image.png",
        ] {
            assert!(preview_image(&json!({"thumbnail":thumbnail})).is_none());
        }
        assert!(
            preview_image(&json!({"url_overridden_by_dest":"https://i.redd.it/video.mp4"}))
                .is_none()
        );
        assert_eq!(
            preview_image(&json!({"thumbnail":"https://b.thumbs.redditmedia.com/post.jpg"}))
                .as_deref(),
            Some("https://b.thumbs.redditmedia.com/post.jpg")
        );
    }
}
