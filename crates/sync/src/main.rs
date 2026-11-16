use std::sync::Arc;

use tokio::signal;
use tokio_cron_scheduler::{Job, JobScheduler};

use tracing_subscriber::{EnvFilter, FmtSubscriber};

mod karakeep;
mod plugin;
mod settings;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let state_path =
        std::path::PathBuf::from(std::env::var("KS_STATE_PATH").unwrap_or_else(|_| "/data".into()));
    let heartbeat = state_path.join("heartbeat.json");
    if std::env::args().any(|arg| arg == "--healthcheck") {
        let age = std::fs::metadata(heartbeat)?.modified()?.elapsed()?;
        anyhow::ensure!(
            age < std::time::Duration::from_secs(180),
            "The sync scheduler heartbeat is stale"
        );
        return Ok(());
    }
    karakeep_client::write_private_json(&heartbeat, &serde_json::json!({"running":true}))?;
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            if let Err(error) = karakeep_client::write_private_json(
                &heartbeat,
                &serde_json::json!({"running":true}),
            ) {
                tracing::error!("Could not write scheduler heartbeat: {error}");
            }
        }
    });
    let subscriber = FmtSubscriber::builder()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("tower=off".parse().unwrap())
                .add_directive("hyper=off".parse().unwrap())
                .add_directive("reqwest=off".parse().unwrap())
                .add_directive("karakeep_sync=trace".parse().unwrap())
                .add_directive("karakeep_client=trace".parse().unwrap())
                .add_directive("reddit_client=info".parse().unwrap()),
        )
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    let args: Vec<_> = std::env::args().collect();
    if let Some(index) = args.iter().position(|arg| arg == "--once") {
        let selected = args.get(index + 1).map(String::as_str).unwrap_or("all");
        anyhow::ensure!(
            ["all", "reddit", "github"].contains(&selected),
            "Use --once all, reddit or github"
        );
        for plugin in plugin::get_plugins() {
            let wanted = selected == "all"
                || (selected == "reddit" && plugin.list_name() == "Reddit Saved")
                || (selected == "github" && plugin.list_name() == "GitHub Starred");
            if wanted && plugin.is_activated() {
                plugin.sync().await?;
            }
        }
        return Ok(());
    }
    let mut scheduler = JobScheduler::new().await?;

    let plugins = plugin::get_plugins()
        .into_iter()
        .map(Arc::new)
        .collect::<Vec<_>>();

    for plugin in plugins {
        let list_name = plugin.list_name();

        if !plugin.is_activated() {
            tracing::info!("plugin for list '{}' is not activated, skipping", list_name);
            continue;
        }

        if plugin.run_immediate() {
            let plugin = plugin.clone();
            let job =
                Job::new_one_shot_async(std::time::Duration::from_millis(10), move |_uuid, _l| {
                    tracing::info!("starting immediate sync job for list: {}", list_name);
                    let p = plugin.clone();
                    Box::pin(async move {
                        if let Err(e) = p.sync().await {
                            tracing::error!("sync failed for list '{}': {:#}", list_name, e);
                        }
                    })
                })?;
            scheduler.add(job).await?;
        }

        tracing::info!(
            "scheduling recurring job for list: {}, period: {}",
            list_name,
            plugin.recurring_schedule()
        );

        let schedule = plugin.recurring_schedule().to_string();
        let job = Job::new_async(&schedule, move |_uuid, _l| {
            tracing::info!("starting recurring sync job for list: {}", list_name);
            let p = plugin.clone();
            Box::pin(async move {
                if let Err(e) = p.sync().await {
                    tracing::error!("sync failed for list '{}': {:#}", list_name, e);
                }
            })
        })?;
        scheduler.add(job).await?;
    }

    scheduler.start().await?;
    signal::ctrl_c().await?;
    scheduler.shutdown().await?;

    Ok(())
}
