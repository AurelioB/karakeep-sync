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
                        // Reddt does not provide timestamp for when the post was saved
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
        if !settings::get_settings().reddit.previews {
            return super::sync_with_client(self, crate::karakeep::get_client()).await;
        }
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
        loop {
            let page = reddit.list_saved(after.as_deref()).await?;
            for post in page.posts {
                if post.preview_image.is_some() {
                    previews += 1;
                }
                let bookmark = BookmarkCreate {
                    title: post.title,
                    url: post.url,
                    preview_image: post.preview_image,
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
