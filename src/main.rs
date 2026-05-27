use anyhow::Result;
use axum::Router;
use axum::response::Redirect;
use axum::routing::{get, post};
use std::net::SocketAddr;
use std::path::PathBuf;
use tower_http::trace::TraceLayer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

mod artifacts;
mod build;
mod config;
mod error;
mod git;
mod notify;
mod routes;
mod runner;
mod state;
mod views;
mod webhook;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cfg_path = std::env::var("KEI_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("kei.toml"));
    tracing::info!(config = %cfg_path.display(), "loading config");
    let cfg = config::Config::load(&cfg_path)?;

    if cfg.github.webhook_secret.is_none() {
        tracing::warn!(
            "github.webhook_secret is unset — /webhook accepts unsigned \
             requests; anyone who can reach the endpoint can trigger builds"
        );
    }

    tokio::fs::create_dir_all(&cfg.storage.workspace_dir).await?;
    tokio::fs::create_dir_all(&cfg.storage.artifacts_dir).await?;

    // Expose the configured Maven repo dir to build subprocesses so projects
    // can do `./gradlew publishToMavenLocal -Dmaven.repo.local=$KEI_MAVEN_REPO`
    // without hard-coding the deployment path in their in-repo kei.toml.
    if let Some(dir) = cfg.maven.repo_dir.as_ref() {
        if let Err(e) = tokio::fs::create_dir_all(dir).await {
            tracing::warn!(error=%e, "couldn't create maven repo dir");
        }
        // Safety: called before any thread is spawned that reads env vars.
        unsafe {
            std::env::set_var("KEI_MAVEN_REPO", dir);
        }
        tracing::info!(dir=%dir.display(), "maven repo configured");
    }

    let app_state = state::AppState::new(cfg.clone());

    // Rehydrate the in-memory build map from disk so the builds list survives
    // restarts. Done before bootstrap so new auto-triggered builds get
    // monotonic numbers above the persisted ones.
    app_state.load_persisted_builds().await;

    // Catch up on anything that moved while kei was down: ls-remote each
    // configured project and trigger a build whenever the remote tip differs
    // from `.last_commit` (or `.last_commit` doesn't exist yet — new project).
    let bootstrap_state = app_state.clone();
    tokio::spawn(async move {
        build::bootstrap_initial_builds(bootstrap_state).await;
    });

    let app = Router::new()
        .route("/", get(views::index))
        .route("/health", get(routes::health))
        .route("/webhook", post(webhook::handle))
        .route("/api/projects", get(routes::list_projects))
        .route("/api/builds", get(routes::list_builds))
        .route("/api/builds/:id", get(routes::get_build))
        .route("/api/builds/:id/log", get(views::build_log_raw))
        .route("/api/builds/trigger", post(routes::trigger))
        .route("/api/artifacts", get(routes::list_artifacts))
        .route("/builds", get(views::list_builds))
        .route("/builds/:id", get(views::build_detail))
        // Kei-hosted commit / compare views — sourced from the workspace so
        // they work for private repos that a GitHub link couldn't reach
        // without auth.
        .route("/commits/:project/:sha", get(views::commit_view))
        .route("/compare/:project/:range", get(views::compare_view))
        // /artifacts: served by our own handler (directory listings + files).
        // ServeDir + nest_service had two interaction bugs in this version:
        // the directory-fallback status got pinned to 404, and the trailing-
        // slash redirect lost the `/artifacts` prefix.
        .route("/artifacts", get(|| async { Redirect::permanent("/artifacts/") }))
        .route("/artifacts/", get(views::artifacts_handler))
        .route("/artifacts/*path", get(views::artifacts_handler));

    #[cfg(feature = "maven")]
    let app = app
        .route("/maven", get(|| async { Redirect::permanent("/maven/") }))
        .route("/maven/", get(views::maven_handler))
        .route("/maven/*path", get(views::maven_handler));

    let app = app
        .layer(TraceLayer::new_for_http())
        .with_state(app_state);

    let addr = format!("{}:{}", cfg.server.host, cfg.server.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "kei listening");
    // ConnectInfo<SocketAddr> needed so the trigger route can gate by peer IP.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_env("KEI_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,kei=debug,tower_http=info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer())
        .init();
}
