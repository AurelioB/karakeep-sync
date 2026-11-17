# karakeep-sync

A tool to sync links from various services to [Karakeep](https://github.com/hoarder-app/hoarder) to keep all your interesting links in one place.

## Overview

When looking up something interesting you found in the past, you probably check multiple places - Karakeep, HN upvotes, Reddit bookmarks, etc. This tool syncs all those links to Karakeep automatically, organizing them under lists for easy future access.

## Supported Services

- ✅ Hacker News upvotes
- ✅ Reddit saved posts
- ✅ Github stars
- ✅ Pinboard bookmarks
- 🚧 X bookmarks (planned)
- 🚧 Bluesky bookmarks (planned)

## Environment Variables

Configure these environment variables in your `docker-compose.yml`:

| Variable           | Required | Description                                                       |
| ------------------ | -------- | ----------------------------------------------------------------- |
| `KS_KARAKEEP_AUTH` | ✅       | Your Karakeep API token                                           |
| `KS_KARAKEEP_URL`  | ✅       | Your Karakeep instance URL (e.g., `https://karakeep.example.com`) |

### For Hacker News

| Variable         | Required | Description                                      |
| ---------------- | -------- | ------------------------------------------------ |
| `KS_HN_AUTH`     | ❌       | Your Hacker News authentication cookie value     |
| `KS_HN_SCHEDULE` | ❌       | Sync schedule in cron format (default: `@daily`) |

Hacker news auth cookie can be obtained by logging into your HN account and inspecting the cookies in your browser. Look for the `user` cookie.

Hacker News upvotes will be synced to a list named `HN Upvoted` in your Karakeep instance.

Hacker News sync will be skipped if `KS_HN_AUTH` is not set.

### For Reddit

| Variable                 | Required | Description                                      |
| ------------------------ | -------- | ------------------------------------------------ |
| `KS_REDDIT_CLIENTID`     | ❌        | Your Reddit app client ID                        |
| `KS_REDDIT_CLIENTSECRET` | ❌        | Your Reddit app client secret                    |
| `KS_REDDIT_REFRESHTOKEN` | ❌        | Your Reddit app refresh token                    |
| `KS_REDDIT_USERNAME`     | ❌        | Your Reddit username (without `u/` prefix)       |
| `KS_REDDIT_SCHEDULE`     | ❌        | Sync schedule in cron format (default: `@daily`) |


To obtain a refresh token, you can follow these steps:

1. Create a Reddit app [here](https://www.reddit.com/prefs/apps) (choose "script" as the app type).
2. You can use a tool like [this](https://github.com/not-an-aardvark/reddit-oauth-helper) to generate a refresh token using your app's client ID and client secret. Make sure that the redirect URI matches the one provided from reddit-oauth-helper.
3. Make sure to give the app `history` scope access.
4. Make sure to tick the "permanent" option to get a refresh token.

If you don't want to trust a third party tool, you can also implement the OAuth2 flow yourself using the [Reddit API docs](https://www.reddit.com/dev/api/), or [manually generate a token](/REDDIT_REFRESH_TOKEN.md).

Reddit saves will be synced to a list named `Reddit Saved` in your Karakeep instance.

Reddit sync will be skipped if any of the following are not set:
- `KS_REDDIT_CLIENTID`
- `KS_REDDIT_CLIENTSECRET`
- `KS_REDDIT_REFRESHTOKEN`
- `KS_REDDIT_USERNAME`


### GitHub Stars

| Variable             | Required | Description                                      |
| -------------------- | -------- | ------------------------------------------------ |
| `KS_GITHUB_TOKEN`    | ❌       | Your GitHub personal access token                |
| `KS_GITHUB_SCHEDULE` | ❌       | Sync schedule in cron format (default: `@daily`) |
| `KS_GITHUB_SYNCLISTS` | No | Import GitHub star lists in addition to the flat list (default: `false`) |

To obtain a GitHub personal access token, you can visit [this link](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/managing-your-personal-access-tokens#creating-a-fine-grained-personal-access-token) and create a new token with `Starring` user permission (read).

GitHub stars will be synced to a list named `GitHub Starred` in your Karakeep instance.

Set `KS_GITHUB_SYNCLISTS=true` to also import the authenticated user's GitHub star lists through the GraphQL API. Each source list becomes a child of `GitHub Starred`, using its original name. Bookmarks also remain directly in `GitHub Starred`. A repository can belong to multiple child lists.

List imports paginate every source list and its repositories on each run. They reuse existing bookmarks by URL and add memberships alongside the full metadata pass. This also distributes stars imported before the option was enabled. On first import, an untracked child with the same name and parent is reused. Lists elsewhere in Karakeep are not reused or moved.

Each imported child stores its GitHub list ID in a `karakeep-sync-source: github-list:<id>` line in its description. Existing description text is preserved. Keep this metadata line so source renames update the same Karakeep child without changing its ID or memberships. Names are not used to identify a list after it is linked to GitHub.

List imports are additive. They do not delete bookmarks, remove memberships, or modify GitHub. Only lists and repositories visible to the token can be imported. The token must support these GraphQL queries, not just the REST stars endpoint. HTTP errors and GraphQL permission errors fail the sync and appear in the logs.

GitHub sync will be skipped if `KS_GITHUB_TOKEN` is not set.

### Pinboard Bookmarks

| Variable               | Required | Description                                      |
| ---------------------- | -------- | ------------------------------------------------ |
| `KS_PINBOARD_TOKEN`    | ❌       | Your Pinboard API token                          |
| `KS_PINBOARD_SCHEDULE` | ❌       | Sync schedule in cron format (default: `@daily`) |

To obtain your Pinboard API token, visit your [Pinboard password page](https://pinboard.in/settings/password) and scroll down to the "API Token" section. The token will be in the format `username:TOKEN`.

Pinboard bookmarks will be synced to a list named `Pinboard` in your Karakeep instance.

Pinboard sync will be skipped if `KS_PINBOARD_TOKEN` is not set.

## Deployment

Create a `docker-compose.yml` file with the following content:

```yaml
services:
  karakeep-sync:
    image: karakeep-sync:local
    build:
      context: https://github.com/AurelioB/karakeep-sync.git#main
    pull_policy: build
    container_name: karakeep-sync
    restart: unless-stopped
    volumes:
      - ./sync-data:/data
    environment:
      - KS_KARAKEEP_AUTH=<your_karakeep_auth_cookie> # required
      - KS_KARAKEEP_URL=<your_karakeep_instance_url> # required

      - KS_HN_AUTH=<your_hn_auth_cookie> # optional
      - KS_HN_SCHEDULE=@daily # optional Cron format, e.g., "@hourly", "@daily", "0 0 * * *" default is "@daily"

      - KS_REDDIT_CLIENTID=<your_reddit_client_id> # optional
      - KS_REDDIT_CLIENTSECRET=<your_reddit_client_secret> # optional
      - KS_REDDIT_REFRESHTOKEN=<your_reddit_refresh_token> # optional
      - KS_REDDIT_USERNAME=<your_reddit_username> #optional
      - KS_REDDIT_SCHEDULE=@daily # optional Cron format, e.g., "@hourly", "@daily", "0 0 * * *" default is "@daily"

      - KS_GITHUB_TOKEN=<your_github_personal_access_token> # optional
      - KS_GITHUB_SCHEDULE=@daily # optional Cron format, e.g., "@hourly", "@daily", "0 0 * * *" default is "@daily"
      - KS_GITHUB_SYNCLISTS=false # optional: also import GitHub star lists

      - KS_PINBOARD_TOKEN=<your_pinboard_api_token> # optional
      - KS_PINBOARD_SCHEDULE=@daily # optional Cron format, e.g., "@hourly", "@daily", "0 0 * * *" default is "@daily"
```

Then run:

```bash
docker-compose up -d
```

You can also add this service definition alongside your existing Hoarder/Karakeep services.

## Contributing

Contributions are welcome! Please open issues or pull requests for any features, bug fixes, or improvements.

To add support for more services, implement the `Plugin` trait in a new module under `crates/sync/src/plugin/`. You can refer to the existing `hn_upvotes` and `reddit_saves` modules as examples. All plugins must be registered in `crates/sync/src/plugin.rs`. Make sure to add appropriate configuration options in `crates/sync/src/settings.rs`. Finally, update the documentation in this README to include the new service.

See this PR for adding GitHub stars support as an example: [#2](https://github.com/sidoshi/karakeep-sync/pull/2)

## License

MIT License. See `LICENSE` file for details.

## Fork additions

This fork incorporates [GitHub list support from upstream PR #10](https://github.com/sidoshi/karakeep-sync/pull/10), preserving its original commits and authorship. Enable `KS_GITHUB_SYNCLISTS=true` to import your GitHub lists beneath **GitHub Starred**. Lists track their stable GitHub IDs, so renames update the existing child. Imports are additive: bookmarks and existing memberships are not deleted.

### Reddit preview images

Set `KS_REDDIT_PREVIEWS=true` to use the preview images Reddit returns with your saved items. The sync chooses a useful still image, including gallery images and video posters, and uploads it to Karakeep as the bookmark's banner. It prefers an image up to 1600 pixels wide and rejects files over 20 MiB. Image downloads are restricted to HTTPS Reddit image hosts and use a separate client that never sends your Karakeep API key.

This mode scans all pages Reddit currently exposes, including already imported saves, so existing bookmarks receive previews too. Text-only items, deleted images, and comments without preview metadata retain their current images. The sync does not invent a preview when Reddit supplies none. Original image files and bookmark metadata are backed up before replacement. Existing covers are streamed to private backup storage (up to 128 MiB), while new preview payloads remain limited to 20 MiB. Owner-uploaded custom cover images are retained.

Mount a persistent writable directory at `/data`, or choose another path with `KS_STATE_PATH`. It holds the preview cache, pending-upload recovery state, original-image backups, and scheduler heartbeat. Keep this directory private and include it in backups. The preview cache prevents repeated downloads/uploads on later runs. A failed image is logged and retried later without stopping bookmark imports. Uploads are paced for Karakeep's API limits, so a large first backfill can take several minutes.

Existing bookmark IDs, titles, notes, tags, list memberships, and saved dates are preserved. For new Reddit saves only, synthetic timestamps spaced one second apart retain Reddit's newest-first order. Reddit does not provide historical save timestamps.

Configured schedules now override the `@daily` defaults correctly. For example, `KS_REDDIT_SCHEDULE=@hourly` and `KS_GITHUB_SCHEDULE=@hourly` run each hour. The Docker image runs as non-root and includes a scheduler heartbeat health check. Set the bind mount's ownership to the image's UID (65532), or override the container user/group for your environment.

For an explicit one-time pass, use `karakeep-sync --once reddit`, `--once github`, or `--once all` with the same environment and persistent state as the service. This does not start the recurring scheduler.

```yaml
# Add these to your existing karakeep-sync service. Keep credentials in a private
# environment file or secret manager; do not commit real keys or refresh tokens.
environment:
  KS_REDDIT_PREVIEWS: "true"
  KS_GITHUB_SYNCLISTS: "true"
  KS_REDDIT_SCHEDULE: "@hourly"
  KS_GITHUB_SCHEDULE: "@hourly"
  KS_STATE_PATH: /data
volumes:
  - ./sync-data:/data
```

## Native GitHub and Reddit metadata

Every GitHub and Reddit sync scans all pages, including existing bookmarks. Native
fields are updated only when their source values differ; an unchanged second pass
makes no metadata or tag writes. Notes, summaries, archive/favourite flags, and existing
tags are never replaced. Source tags are additive and case-insensitive when matching.
Private original metadata is saved under `/data/metadata-before` before the first change.

| Karakeep field | GitHub | Reddit |
| --- | --- | --- |
| Title | Repository name with owner | Post title; parent post title for saved comments |
| Description | Repository description | Complete post text or comment body |
| Author | Repository owner | Post/comment author, when available |
| Publisher | GitHub | Reddit, including the subreddit |
| Publication date | Repository creation | Post/comment creation |
| Modification date | Last push | Numeric edit timestamp, when provided |
| Saved date | Actual `starredAt` from GitHub | Existing date preserved; new saves use synthetic listing order |
| Tags | Topics, primary language, SPDX license | Subreddit (`r/name`) and post flair |
| Banner | GitHub Open Graph image | Reddit preview/gallery/thumbnail |

GitHub's GraphQL star edges expose the actual star timestamp independently of the
repository's creation date. The full pass also retrieves metadata for older saved
repository URLs, follows GitHub's rename/transfer identity, and reuses existing
Karakeep bookmark IDs in child lists. Saved repositories no longer in the current
star response keep their saved date: GitHub cannot supply a past star event after
it has been removed. Missing/private/deleted repositories are left intact.

Existing GitHub banners are reused when already sourced from the same Open Graph
image (the default image's changing cache hash is ignored). Replacements use the
same bounded upload, private backup, and custom-cover protection as Reddit.

To backfill Reddit bookmarks older than the saved-list API returns, run:

```sh
karakeep-sync --once reddit --backfill
```

This resolves the post/comment IDs from existing Reddit bookmarks through
Reddit's `/api/info`, in batches of up to 100. It requires `read` scope in addition
to `history` (reconnect once if necessary). It never infers a saved date from a
post's publication date. Items Reddit cannot return retain their current metadata.
Normal scheduled runs refresh the saved listing; the explicit older-item backfill
can be repeated safely. No metadata is written into notes.

### Repair Reddit verification and rate-limit captures

Reddit sync also recognizes crawled verification-page and HTTP 429 titles. Before
removing a crawler screenshot it saves the full original bookmark metadata/HTML and
screenshot privately under `/data/reddit-capture-before`. Uploaded custom screenshots,
real banners, notes, flags, saved dates, and useful existing summaries are preserved.
Summaries that describe the error page are replaced with a short extract from the
actual Reddit post/comment (or its source title for link-only posts); unavailable
source content results in clearing the bogus summary, not inventing one. The existing
native description holds the source excerpt as well. After a screenshot is removed,
Karakeep chooses its normal overview/real banner instead of the captured error page.

The full Reddit Saved list is checked for legacy captures on scheduled passes, so
older items can be cleaned even outside the current saved API listing. Cleanup is
idempotent and will repair a later recrawl that captures another error page. The original captured HTML is saved privately before repair. Blocked or failed
reader content is replaced through Karakeep’s native precrawled-archive attachment
and reader rebuild APIs, without re-saving bookmarks or direct database writes.

Full Reddit bodies are retained in descriptions and rendered as safe Markdown HTML
for reader recovery, including paragraphs, links, tables and code blocks. Raw active
HTML is escaped and unsafe link schemes are removed. Reader archives are private,
tracked by content hash, retried when attachment/queueing fails, and rebuilt only
when source content changes. A rebuild already pending is reused. Healthy captured
reader content and custom archives are not replaced merely to add metadata.

All existing copies of equivalent Reddit URLs receive metadata and previews;
separate bookmark IDs, notes, saved dates, and memberships remain separate.
`KS_UPLOAD_INTERVAL_MS` controls spacing between asset uploads (default 2200 ms,
about 27/minute). Set a shorter interval only when the target instance's upload
limits permit it. HTTP rate-limit responses are deferred and retried safely.

Explicit Reddit backfill also covers manually saved Reddit links outside the sync
list. It enriches existing records only, preserving every list membership and never
adding a manual bookmark to Reddit Saved. Failed reader builds are rechecked from
the API before retrying, including old items no longer in the saved listing.

If Reddit returns a removed comment with no recoverable text or parent title, reader
recovery shows an explicit availability notice instead of the old verification page.
A previously archived full body is kept if a later API response loses that text.
