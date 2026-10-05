mod ads;
mod api;
mod config;
mod control;
mod ingest;
mod model;
mod overlay;
mod pipeline;
mod manage;

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    gstreamer::init()?;
    tracing::info!("gstreamer initialized: {}", gstreamer::version_string());

    let video = tokio::spawn(ingest::udp_video::run());
    let audio = tokio::spawn(ingest::udp_audio::run());
    let control = tokio::spawn(control::run());
    let manage = tokio::spawn(manage::run());
//add manage net
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
/// a view-count snapshot for every live session to the backend.
///
/// There's no real viewer-count instrumentation in this codebase yet (no
/// platform-side viewer webhook, no per-platform breakdown), so
/// `current_views` here is a documented stand-in: the number of connected
/// control-socket clients (camera operators + companion apps) for the
/// session. It's a proxy for activity, not actual audience size — swap it
/// out once real viewer metrics exist. `award_rank_points` awards the
/// match's author (`session.broadcaster_id`, resolved by Laravel from
/// `matches.author_id` at session-creation time — see
/// `api::SessionCreateResult`), independent of the session id itself.
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
                let current_views = session.clients.len() as u64;
                api::CLIENT.report_views(&id, &session.stream_key, "internal", current_views);
                let totals = api::ViewTotals { stream_key: session.stream_key.clone(), total_views: current_views };
                tracing::info!(stream_key = %totals.stream_key, total_views = totals.total_views, "view metrics reported");
                if let Some(broadcaster_id) = session.broadcaster_id() {
                    api::CLIENT.award_rank_points(&broadcaster_id, current_views);
                }
            }
        }
    }
}

//views will be derived(fetched) from the social media via the social media streamkeys supplied under urls map sent when registering for youtube use the youtube streamkey and facebook the same. 