use anyhow::Context;
use async_trait::async_trait;
use futures::Stream;
use karakeep_client::BookmarkCreate;
use std::{pin::Pin, sync::Arc};

use crate::settings;
use reddit_client::RedditClientRefresher;

#[derive(Debug, Clone)]
pub struct RedditSaves {}

#[async_trait]
impl super::Plugin for RedditSaves {
    fn list_name(&self) -> &'static str {
        "Reddit Saved"
    }

    async fn to_bookmark_stream(
        &self,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = Vec<BookmarkCreate>> + Send>>> {
        let client = Arc::new(authenticated().await?);

        enum StreamState {
            Init,
            Next(Option<String>),
        }

        let stream = futures::stream::unfold(StreamState::Init, move |state| {
            let client = client.clone();

            async move {
                let after = match &state {
                    StreamState::Init => None,
                    StreamState::Next(after) => after.as_ref().cloned(),
                };
                if after.is_none() && matches!(state, StreamState::Next(_)) {
                    return None;
                }

                let resp = match client.list_saved(after.as_deref()).await {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::error!("Reddit list_saved failed: {:#}", e);
                        return None;
                    }
                };

                let items = resp
                    .posts
                    .into_iter()
                    .map(|post| BookmarkCreate {
                        title: post.title,
                        url: post.url,
                        reader_html: post.reader_html,
                        metadata: post.metadata,
                        tags: post.tags,
                        // Reddit does not expose save timestamps.
                        created_at: None,
                        ..Default::default()
                    })
                    .collect::<Vec<_>>();

                Some((items, StreamState::Next(resp.after)))
            }
        });

        Ok(Box::pin(stream))
    }

    async fn sync(&self) -> anyhow::Result<i32> {
        let _guard = super::SYNC_LOCK.lock().await;
        let reddit = authenticated().await?;
        let karakeep = crate::karakeep::get_client();
        karakeep.refresh_bookmarks().await?;
        let list = karakeep.ensure_list_exists(self.list_name()).await?;
        let anchor = chrono::Utc::now() - chrono::Duration::seconds(1);
        let mut after = None;
        let mut seen = std::collections::HashSet::new();
        let mut rank = 0_i64;
        let mut created = 0;
        let mut previews = 0;
        let mut visited = std::collections::HashSet::new();
        loop {
            let page = reddit.list_saved(after.as_deref()).await?;
            for post in page.posts {
                if post.preview_image.is_some() {
                    previews += 1;
                }
                visited.insert(karakeep_client::bookmark_key(&post.url));
                let bookmark = BookmarkCreate {
                    title: post.title,
                    url: post.url,
                    reader_html: post.reader_html,
                    metadata: post.metadata,
                    tags: post.tags,
                    preview_image: if settings::get_settings().reddit.previews {
                        post.preview_image
                    } else {
                        None
                    },
                    // Reddit has no save timestamp. Preserve listing order for new saves;
                    // existing bookmarks, including repaired dates, are never re-dated.
                    created_at: Some((anchor - chrono::Duration::seconds(rank)).to_rfc3339()),
                };
                let (_, is_new) = karakeep.upsert(&bookmark, &list).await?;
                created += i32::from(is_new);
                rank += 1;
            }
            after = page.after;
            let Some(ref cursor) = after else { break };
            anyhow::ensure!(
                seen.insert(cursor.clone()),
                "Reddit repeated a saved-items cursor"
            );
        }
        if std::env::args().any(|a| a == "--backfill") {
            let older = karakeep.list_bookmarks(None).await?;
            let mut ids = std::collections::BTreeSet::new();
            for row in older {
                let Some(url) = row["content"]["url"].as_str() else {
                    continue;
                };
                let key = karakeep_client::bookmark_key(url);
                if visited.contains(&key) {
                    continue;
                }
                if let Some(fullname) = reddit_fullname(&key) {
                    ids.insert(fullname);
                }
            }
            let ids: Vec<_> = ids.into_iter().collect();
            let mut found = 0;
            for batch in ids.chunks(100) {
                let posts = reddit.posts_by_ids(batch).await?;
                for post in posts {
                    let bookmark = BookmarkCreate {
                        title: post.title,
                        url: post.url,
                        reader_html: post.reader_html,
                        metadata: post.metadata,
                        tags: post.tags,
                        preview_image: if settings::get_settings().reddit.previews {
                            post.preview_image
                        } else {
                            None
                        },
                        // Backfilling an older item must never invent a new saved date.
                        created_at: None,
                    };
                    // Existing manual bookmarks need recovery too, without changing
                    // their list memberships or creating posts absent from Karakeep.
                    karakeep.enrich_existing(&bookmark).await?;
                    found += 1;
                }
            }
            tracing::info!(
                "Reddit older-item backfill complete (requested={}, returned={})",
                ids.len(),
                found
            );
        }
        karakeep.retry_readers().await?;
        let mut repaired = 0;
        for row in karakeep.list_bookmarks(Some(&list)).await? {
            if karakeep_client::blocked_reddit_capture(&row) {
                // Metadata may be unavailable for a deleted post. Clear only an
                // identifiable error-page summary and keep its saved date and notes.
                let metadata = if row["content"]["publisher"]
                    .as_str()
                    .is_some_and(|s| s.starts_with("Reddit · r/"))
                {
                    serde_json::json!({"title":row["title"],"description":row["content"]["description"]})
                } else {
                    serde_json::json!({})
                };
                repaired += usize::from(karakeep.repair_reddit_capture(&row, &metadata).await?);
            }
        }
        repaired += karakeep.remove_retained_error_screenshots().await?;
        tracing::info!(
            "Reddit legacy capture cleanup complete (additional_repairs={})",
            repaired
        );
        tracing::info!(
            "Reddit sync complete (saved={}, created={}, previews_available={})",
            rank,
            created,
            previews
        );
        Ok(created)
    }

    fn is_activated(&self) -> bool {
        let settings = settings::get_settings();

        settings.reddit.clientid.is_some()
            && settings.reddit.clientsecret.is_some()
            && settings.reddit.refreshtoken.is_some()
            && settings.reddit.username.is_some()
    }

    fn recurring_schedule(&self) -> String {
        let settings = settings::get_settings();
        settings.reddit.schedule.clone()
    }
}

async fn authenticated() -> anyhow::Result<reddit_client::RedditClient> {
    let settings = &settings::get_settings().reddit;
    RedditClientRefresher::new(
        settings
            .clientid
            .clone()
            .context("Reddit client ID is not set")?,
        settings
            .clientsecret
            .clone()
            .context("Reddit client secret is not set")?,
        settings
            .refreshtoken
            .clone()
            .context("Reddit refresh token is not set")?,
    )
    .refresh(
        settings
            .username
            .clone()
            .context("Reddit username is not set")?,
    )
    .await
}

fn reddit_fullname(key: &str) -> Option<String> {
    let parts: Vec<_> = key.strip_prefix("reddit:")?.split(':').collect();
    let raw = if parts.len() == 2 { parts[1] } else { parts[0] };
    if raw.is_empty() || !raw.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(format!(
        "{}_{}",
        if parts.len() == 2 { "t1" } else { "t3" },
        raw
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn older_post_and_comment_urls_use_their_distinct_reddit_ids() {
        assert_eq!(reddit_fullname("reddit:abc").as_deref(), Some("t3_abc"));
        assert_eq!(reddit_fullname("reddit:abc:def").as_deref(), Some("t1_def"));
        assert!(reddit_fullname("https://example.com").is_none());
    }
}
