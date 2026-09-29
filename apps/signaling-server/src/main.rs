use anyhow::Context;
use std::sync::Arc;

use signaling_server::cluster::RedisBus;
use signaling_server::store::postgres::PgStore;
use signaling_server::{AppState, Config, MediaFactory, Services, serve};
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

    let mut services = Services::default();
    if let Some(url) = &config.database_url {
        services.store = Some(Arc::new(PgStore::connect(url).await?));
        info!("using Postgres for devices, trust and audit log");
    } else {
        warn!("CONNEXA_DATABASE_URL is not set; device trust and audit log are kept in memory");
    }
    if let Some(url) = &config.redis_url {
        services.bus = Some(Arc::new(
            RedisBus::connect(url)
                .await
                .context("connecting to Redis")?,
        ));
        info!(slot = config.node_slot, "clustering through Redis");
    }
    if config.sfu.enabled {
        services.media = Some(sfu_factory(&config)?);
    }

    serve(listener, AppState::with_services(config, services).await?).await?;
    Ok(())
}

fn sfu_factory(config: &Config) -> anyhow::Result<MediaFactory> {
    let settings = config.sfu.clone();
    let ice = config.ice.clone();
    let sfu = signaling_server::sfu::start(settings, ice)?;
    info!(
        udp_port = config.sfu.udp_port,
        "SFU enabled for large rooms"
    );
    Ok(Box::new(move |hub| sfu.attach(hub)))
}
