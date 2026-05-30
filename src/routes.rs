use axum::Json;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::HeaderMap;
use serde::Deserialize;
use serde_json::{Value, json};
use std::net::{IpAddr, SocketAddr};
use tracing::warn;
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::{AppState, ArtifactRef, BuildStatus};
use crate::{auth, build};

pub async fn health() -> Json<Value> {
    Json(json!({ "ok": true }))
}

pub async fn list_projects(State(state): State<AppState>, headers: HeaderMap) -> Json<Value> {
    let principal = auth::authenticate(&headers, &state.config);
    let names: Vec<&str> = state
        .config
        .projects
        .iter()
        .filter(|p| auth::can_view_project(principal.as_ref(), p))
        .map(|p| p.name.as_str())
        .collect();
    Json(json!({ "projects": names }))
}

pub async fn list_builds(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Json<Vec<BuildStatus>> {
    let principal = auth::authenticate(&headers, &state.config);
    let builds = state
        .list_builds()
        .await
        .into_iter()
        .filter(|b| {
            state
                .config
                .project(&b.project)
                .is_some_and(|p| auth::can_view_project(principal.as_ref(), p))
        })
        .collect();
    Json(builds)
}

pub async fn get_build(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<BuildStatus>, ApiError> {
    let build = state
        .get_build(id)
        .await
        .ok_or_else(|| ApiError::not_found("build not found"))?;
    let project = state
        .config
        .project(&build.project)
        .ok_or_else(|| ApiError::not_found("project not found"))?;
    let principal = auth::authenticate(&headers, &state.config);
    auth::require_project_access(principal.as_ref(), project)?;
    Ok(Json(build))
}

pub async fn list_artifacts(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Json<Vec<ArtifactRef>> {
    let principal = auth::authenticate(&headers, &state.config);
    let mut out = Vec::new();
    for b in state.list_builds().await {
        if !state
            .config
            .project(&b.project)
            .is_some_and(|p| auth::can_view_project(principal.as_ref(), p))
        {
            continue;
        }
        out.extend(b.artifacts);
    }
    Json(out)
}

#[derive(Debug, Deserialize)]
pub struct TriggerBody {
    pub project: String,
}

pub async fn trigger(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<TriggerBody>,
) -> Result<Json<Value>, ApiError> {
    let client = effective_client_ip(&headers, peer);
    if !is_private_or_loopback(client) {
        warn!(%client, project=%body.project, "rejected trigger from non-private IP");
        return Err(ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            "trigger is restricted to private/loopback networks",
        ));
    }
    if state.config.project(&body.project).is_none() {
        return Err(ApiError::not_found(format!(
            "unknown project '{}'",
            body.project
        )));
    }
    let id = build::run_build(state, body.project.clone()).await?;
    Ok(Json(json!({ "build_id": id, "project": body.project })))
}

/// Resolve the *effective* client IP for trust decisions. When the request
/// arrives on the loopback peer (i.e. via the local reverse proxy), trust
/// the leftmost X-Forwarded-For value — nginx is configured to overwrite
/// (not append to) this header, so the value reflects the real client.
/// When the peer is non-loopback, kei is being hit directly and XFF is
/// attacker-controlled, so use the peer address.
fn effective_client_ip(headers: &HeaderMap, peer: SocketAddr) -> IpAddr {
    if peer.ip().is_loopback()
        && let Some(xff) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(',').next())
            .map(str::trim)
            .and_then(|s| s.parse::<IpAddr>().ok())
    {
        return xff;
    }
    peer.ip()
}

fn is_private_or_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private(),
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                return true;
            }
            // ULA: fc00::/7
            if v6.segments()[0] & 0xfe00 == 0xfc00 {
                return true;
            }
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4.is_loopback() || v4.is_private();
            }
            false
        }
    }
}
