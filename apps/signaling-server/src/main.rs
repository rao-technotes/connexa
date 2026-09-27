use anyhow::Context;
use signaling_server::{AppState, Config, serve};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,tower_http=warn")),
        )
        .init();

    let config = Config::from_env()?;
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("failed to bind {}", config.bind))?;

    info!(
        "{} signaling server v{} (protocol v{}) listening on http://{}",
        connexa_core::PROJECT_NAME,
        env!("CARGO_PKG_VERSION"),
        connexa_core::PROTOCOL_VERSION,
        config.bind
    );
    info!(
        max_participants = config.hub.max_participants,
        stun = config.ice.stun_urls.len(),
        turn = config.ice.turn_urls.len(),
        "configuration loaded"
    );
    if !config.web_root.join("index.html").exists() {
        warn!(
            "web client not found at {} (run `npm run build` in clients/); only /ws and /healthz are served",
            config.web_root.display()
        );
    }
    if config.allowed_origins.is_empty() {
        warn!("CONNEXA_ALLOWED_ORIGINS is not set; accepting WebSockets from any origin");
    }

    serve(listener, AppState::new(config)).await?;
    Ok(())
}
