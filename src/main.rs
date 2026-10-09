mod ads;
mod api;
mod config;
mod control;
mod ingest;
mod model;
mod overlay;
mod manage;
mod pipeline;
mod social;

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    gstreamer::init()?;
    tracing::info!("gstreamer initialized: {}", gstreamer::version_string());
    social::log_configuration();

    let video = tokio::spawn(ingest::udp_video::run());
    let audio = tokio::spawn(ingest::udp_audio::run());
    let control = tokio::spawn(control::run());
    let manage = tokio::spawn(manage::run());
    tokio::spawn(inactivity_reaper());

    tokio::select! {
        r = video => tracing::error!("video ingest task exited: {r:?}"),
        r = audio => tracing::error!("audio ingest task exited: {r:?}"),
        r = control => tracing::error!("control socket task exited: {r:?}"),
        r = manage => tracing::error!("manage socket task exited: {r:?}"),
    }

    Ok(())
}

/// Periodically sweep sessions with no control/ingest activity for longer
/// than `INACTIVITY_CLEANUP_MS` (mirroring the Java cleanup timer), and —
/// on the same tick, since both share `METRICS_REPORT_INTERVAL_MS` — push
/// the latest view counts for every live session to the backend.
///
/// Views are the real audience on each platform the session streams to:
/// `social::spawn_poller` fetches them from YouTube/Facebook using the stream
/// keys supplied in the `urls` map of `register`, and this loop reports the
/// cached per-platform counts (one `report_views` row per platform). Until a
/// platform has been polled successfully there is nothing to report, so
/// nothing is sent — no stand-in number is made up. `award_rank_points`
/// awards the match's author (`session.broadcaster_id`, resolved by Laravel
/// from `matches.author_id` at session-creation time — see
/// `api::SessionCreateResult`) based on the total across platforms.
async fn inactivity_reaper() {
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(
        config::METRICS_REPORT_INTERVAL_MS,
    ));
    loop {
        interval.tick().await;
        let now = model::session::now_ms();
        for session in model::REGISTRY.all() {
            if now - session.last_activity_ms() > config::INACTIVITY_CLEANUP_MS as i64 {
                tracing::warn!(stream_key = %session.stream_key, "reaping inactive session");
                if let Some(session) = model::REGISTRY.remove(&session.stream_key) {
                    pipeline::teardown(&session);
                }
                continue;
            }

            if let Some(id) = session.laravel_id() {
                let per_platform = session.platform_views_snapshot();
                if per_platform.is_empty() {
                    continue;
                }
                let mut total_views = 0u64;
                for (platform, count) in &per_platform {
                    api::CLIENT.report_views(&id, &session.stream_key, platform, *count);
                    total_views += count;
                }
                let totals = api::ViewTotals { stream_key: session.stream_key.clone(), total_views };
                tracing::info!(stream_key = %totals.stream_key, total_views = totals.total_views, "view metrics reported");
                if let Some(broadcaster_id) = session.broadcaster_id() {
                    api::CLIENT.award_rank_points(&broadcaster_id, total_views);
                }
            }
        }
    }
}
