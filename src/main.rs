use std::{sync::atomic::Ordering, time::Duration};

use ai_gateway::{app, auth, config::Config, metrics as gw_metrics};
use anyhow::Context;
use tokio::{net::TcpListener, sync::watch};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "\
ai-gateway — OpenAI-compatible AI gateway

USAGE:
  ai-gateway [serve] [CONFIG]      Run the gateway (default config: $GATEWAY_CONFIG or ./config.yaml)
  ai-gateway check-config [CONFIG] Validate a config file and exit
  ai-gateway generate-key          Print a new random key and its SHA-256 (client keys and admin.token_sha256)
  ai-gateway hash-key              Read a key on stdin and print its SHA-256
";

fn default_config_path() -> String {
    std::env::var("GATEWAY_CONFIG").unwrap_or_else(|_| "config.yaml".into())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let first = args.next();
    let config_path = match first.as_deref() {
        Some("-h") | Some("--help") | Some("help") => {
            print!("{USAGE}");
            return Ok(());
        }
        Some("generate-key") => {
            let key = auth::generate_key();
            println!("api_key:    {key}\nkey_sha256: {}", auth::hash_key(&key));
            println!("\nGive api_key to the client. Put only key_sha256 in the config.");
            return Ok(());
        }
        Some("hash-key") => {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            println!("{}", auth::hash_key(line.trim()));
            return Ok(());
        }
        Some("check-config") => {
            let path = args.next().unwrap_or_else(default_config_path);
            let cfg = Config::load(&path)?;
            for w in cfg.warnings() {
                println!("warning: {w}");
            }
            println!("config OK: {path}");
            return Ok(());
        }
        Some("serve") => args.next().unwrap_or_else(default_config_path),
        Some(path) if !path.starts_with('-') => path.to_string(),
        Some(other) => anyhow::bail!("unknown argument '{other}'\n\n{USAGE}"),
        None => default_config_path(),
    };

    tracing_subscriber::fmt()
        .json()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let cfg = Config::load(&config_path)?;
    for w in cfg.warnings() {
        tracing::warn!("{w}");
    }
    let metrics = gw_metrics::install()?;
    let state = app::AppState::build(cfg.clone()).await?;

    {
        let m = metrics.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tick.tick().await;
                m.run_upkeep();
            }
        });
    }

    let listener = TcpListener::bind(&cfg.server.listen)
        .await
        .with_context(|| format!("binding {}", cfg.server.listen))?;
    let admin_listener = TcpListener::bind(&cfg.server.admin_listen)
        .await
        .with_context(|| format!("binding {}", cfg.server.admin_listen))?;

    let mut provider_names: Vec<&String> = cfg.providers.keys().collect();
    provider_names.sort();
    tracing::info!(
        listen = %cfg.server.listen,
        admin_listen = %cfg.server.admin_listen,
        providers = ?provider_names,
        routes = cfg.routes.len(),
        keys = cfg.keys.len(),
        store = ?cfg.store.backend,
        admin_api = cfg.admin.token_sha256.is_some(),
        guardrails = cfg.guardrails.rules.len(),
        "ai-gateway started"
    );

    let (stop_tx, stop_rx) = watch::channel(false);
    let api = tokio::spawn(serve(listener, app::router(state.clone()), stop_rx.clone()));
    let admin = tokio::spawn(serve(admin_listener, app::admin_router(state.clone(), metrics), stop_rx));

    shutdown_signal().await;
    state.ready.store(false, Ordering::SeqCst);
    tracing::info!(drain_secs = cfg.server.shutdown_drain_secs, "shutdown requested, draining");
    tokio::time::sleep(Duration::from_secs(cfg.server.shutdown_drain_secs)).await;
    let _ = stop_tx.send(true);

    let mut api = api;
    let finished = tokio::time::timeout(Duration::from_secs(cfg.server.shutdown_grace_secs), &mut api).await;
    if finished.is_err() {
        // Close remaining streams so their usage is recorded, then give the
        // connections a moment to finish.
        tracing::warn!("grace period ended with streams still open; closing them");
        let _ = state.stream_cancel.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(5), &mut api).await;
    }
    admin.abort();
    // Dropped stream bodies spawn their usage writes asynchronously.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Let token-usage writes for finished requests reach the store.
    let flush = tokio::time::timeout(Duration::from_secs(10), async {
        while state.pending_usage_writes.load(Ordering::SeqCst) > 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    if flush.is_err() {
        tracing::warn!(
            pending = state.pending_usage_writes.load(Ordering::SeqCst),
            "exiting with token-usage writes still pending"
        );
    }
    tracing::info!("shutdown complete");
    Ok(())
}

async fn serve(listener: TcpListener, router: axum::Router, mut stop: watch::Receiver<bool>) {
    let result = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = stop.wait_for(|v| *v).await;
        })
        .await;
    if let Err(e) = result {
        tracing::error!(error = %e, "server error");
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
