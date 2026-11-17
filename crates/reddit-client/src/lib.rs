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
    pub reader_html: Option<String>,
    pub metadata: serde_json::Value,
    pub tags: Vec<String>,
    pub title: String,
    pub url: String,
}

#[derive(Debug)]
pub struct ListSavedResponse {
    pub posts: Vec<SavedPost>,
    pub after: Option<String>,
}

impl RedditClient {
    /// Requires Reddit's `read` scope in addition to the saved-list `history` scope.
    pub async fn posts_by_ids(&self, ids: &[String]) -> anyhow::Result<Vec<SavedPost>> {
        anyhow::ensure!(ids.len() <= 100, "Reddit info batch exceeds 100 items");
        let response = self
            .client
            .get(format!("{APP_URL}/api/info"))
            .bearer_auth(&self.access_token)
            .query(&[("id", ids.join(",")), ("raw_json", "1".into())])
            .send()
            .await?;
        anyhow::ensure!(
            response.status() != reqwest::StatusCode::FORBIDDEN,
            "Older Reddit item metadata requires reconnecting Reddit with the read scope"
        );
        let listing: ListingResponse = response.error_for_status()?.json().await?;
        Ok(listing.data.children.into_iter().map(saved_post).collect())
    }
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
            .map(saved_post)
            .collect::<Vec<_>>();

        Ok({
            ListSavedResponse {
                posts,
                after: resp.data.after,
            }
        })
    }
}

fn saved_post(child: ListingChild) -> SavedPost {
    use serde_json::{Value, json};
    let data = child.data;
    let meta = Value::Object(data.metadata);
    let title = data
        .title
        .or_else(|| meta["link_title"].as_str().map(str::to_owned));
    let mut metadata = json!({});
    if let Some(title) = &title {
        metadata["title"] = title.chars().take(1000).collect::<String>().into();
    }
    if let Some(author) = meta["author"]
        .as_str()
        .filter(|s| !s.is_empty() && *s != "[deleted]")
    {
        metadata["author"] = author.into();
    }
    let subreddit = meta["subreddit"].as_str().filter(|s| !s.is_empty());
    metadata["publisher"] = subreddit
        .map(|s| format!("Reddit · r/{s}"))
        .unwrap_or_else(|| "Reddit".into())
        .into();
    let text = if child.kind == "t1" {
        &meta["body"]
    } else {
        &meta["selftext"]
    };
    if let Some(text) = text
        .as_str()
        .filter(|s| !s.trim().is_empty() && !["[removed]", "[deleted]"].contains(s))
    {
        metadata["description"] = text.to_owned().into();
    }
    for (field, source) in [("datePublished", "created_utc"), ("dateModified", "edited")] {
        if let Some(seconds) = meta[source].as_f64().filter(|v| v.is_finite() && *v > 0.0) {
            if let Some(date) = chrono::DateTime::from_timestamp(seconds as i64, 0) {
                metadata[field] = date.to_rfc3339().into();
            }
        }
    }
    let mut tags = vec![];
    if let Some(sub) = subreddit {
        tags.push(format!("r/{sub}"));
    }
    if let Some(flair) = meta["link_flair_text"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
    {
        tags.push(flair.chars().take(100).collect());
    }
    let url = format!("https://reddit.com{}", data.permalink);
    let preview = preview_image(&meta);
    let reader_html = reader_document(
        &metadata,
        &url,
        meta["url_overridden_by_dest"].as_str(),
        preview.as_deref(),
    );
    SavedPost {
        reader_html,
        preview_image: preview,
        metadata,
        tags,
        title: title.unwrap_or_else(|| "(unknown title reddit post)".into()),
        url,
    }
}

fn reader_document(
    metadata: &serde_json::Value,
    url: &str,
    outbound: Option<&str>,
    preview: Option<&str>,
) -> Option<String> {
    use pulldown_cmark::{Event, LinkType, Options, Parser, Tag, TagEnd};
    fn escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&#39;")
    }
    fn safe_link(link: &str) -> bool {
        reqwest::Url::parse(link).is_ok_and(|u| matches!(u.scheme(), "http" | "https" | "mailto"))
            || link.starts_with('#')
    }
    let body = metadata["description"].as_str().unwrap_or("");
    // /api/info omits a saved comment's parent title. Its complete body is
    // still useful reader content; don't discard it for a missing heading.
    let unavailable = metadata["title"].as_str().is_none() && body.is_empty();
    let title = metadata["title"].as_str().unwrap_or("Reddit comment");
    let parser = Parser::new_ext(
        body,
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS,
    )
    .map(|event| match event {
        Event::Html(raw) | Event::InlineHtml(raw) => Event::Text(raw),
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Link {
            link_type,
            dest_url: if safe_link(&dest_url) {
                dest_url
            } else {
                "".into()
            },
            title,
            id,
        }),
        Event::Start(Tag::Image {
            dest_url,
            title,
            id,
            ..
        }) => Event::Start(Tag::Link {
            link_type: LinkType::Inline,
            dest_url: if safe_link(&dest_url) {
                dest_url
            } else {
                "".into()
            },
            title,
            id,
        }),
        Event::End(TagEnd::Image) => Event::End(TagEnd::Link),
        other => other,
    });
    let mut content = String::new();
    pulldown_cmark::html::push_html(&mut content, parser);
    if unavailable {
        content.push_str("<p>Reddit no longer provides the text of this saved item. Your saved link and notes are preserved.</p>");
    }
    if let Some(link) = outbound.filter(|s| safe_link(s)) {
        content.push_str(&format!(
            "<p><a href=\"{}\">{}</a></p>",
            escape(link),
            escape(link)
        ));
    }
    let mut metas = String::new();
    for (key, field) in [
        ("description", "description"),
        ("author", "author"),
        ("og:site_name", "publisher"),
        ("article:published_time", "datePublished"),
        ("article:modified_time", "dateModified"),
    ] {
        if let Some(value) = metadata[field].as_str() {
            metas.push_str(&format!(
                "<meta name=\"{key}\" property=\"{key}\" content=\"{}\">",
                escape(value)
            ));
        }
    }
    if let Some(image) = preview {
        metas.push_str(&format!(
            "<meta property=\"og:image\" content=\"{}\">",
            escape(image)
        ));
    }
    Some(format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{}</title>{metas}</head><body><article><h1>{}</h1>{content}<p><a href=\"{}\">Original Reddit post</a></p></article></body></html>",
        escape(title),
        escape(title),
        escape(url)
    ))
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

#[cfg(test)]
mod metadata_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn saved_comment_has_parent_title_comment_author_and_publication_time_but_no_save_time() {
        let child: ListingChild = serde_json::from_value(json!({"kind":"t1","data":{
            "permalink":"/r/test/comments/abc/title/def/","link_title":"The parent post",
            "body":"Useful comment","author":"writer","subreddit":"test","created_utc":1609459200.0,"edited":false
        }})).unwrap();
        let comment = saved_post(child);
        assert_eq!(comment.title, "The parent post");
        assert_eq!(comment.metadata["author"], "writer");
        assert_eq!(comment.metadata["description"], "Useful comment");
        assert_eq!(
            comment.metadata["datePublished"],
            "2021-01-01T00:00:00+00:00"
        );
        assert!(comment.metadata.get("dateModified").is_none());
        assert!(comment.metadata.get("createdAt").is_none());
        assert!(comment.metadata.get("note").is_none());
        assert_eq!(comment.tags, vec!["r/test"]);
    }
    #[test]
    fn deleted_content_does_not_erase_metadata_and_numeric_edit_time_is_separate() {
        let child: ListingChild = serde_json::from_value(json!({"kind":"t3","data":{
            "title":"Post","permalink":"/r/test/comments/abc/title/","author":"[deleted]",
            "selftext":"[removed]","link_flair_text":"Discussion","edited":1609459200
        }}))
        .unwrap();
        let post = saved_post(child);
        assert!(post.metadata.get("author").is_none());
        assert!(post.metadata.get("description").is_none());
        assert_eq!(post.metadata["dateModified"], "2021-01-01T00:00:00+00:00");
        assert_eq!(post.tags, vec!["Discussion"]);
    }
}

#[cfg(test)]
mod reader_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn reader_and_description_include_the_end_of_long_messages_and_escape_active_html() {
        let body = format!(
            "{}\n\nFINAL PARAGRAPH is present.\n\n<script>alert(1)</script>\n\n[unsafe](javascript:alert(1))",
            "A useful paragraph. ".repeat(100)
        );
        let child: ListingChild =
            serde_json::from_value(json!({"kind":"t3","data":{"title":"Actual title",
            "permalink":"/r/test/comments/abc/title/","selftext":body,"subreddit":"test"}}))
            .unwrap();
        let post = saved_post(child);
        assert_eq!(post.metadata["description"], body);
        let html = post.reader_html.unwrap();
        assert!(html.contains("FINAL PARAGRAPH is present."));
        assert!(!html.contains("<script>"));
        assert!(!html.contains("href=\"javascript:"));
        assert!(html.contains("&lt;script&gt;"));
        let comment = reader_document(
            &json!({"description":"A complete saved comment."}),
            "https://reddit.com/comments/abc/title/def",
            None,
            None,
        )
        .unwrap();
        assert!(comment.contains("A complete saved comment."));
        let removed = reader_document(
            &json!({}),
            "https://reddit.com/comments/abc/title/def",
            None,
            None,
        )
        .unwrap();
        assert!(removed.contains("Reddit no longer provides the text"));
    }
}
