use axum::Json;
use axum::extract::{Path, State};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::build;
use crate::error::ApiError;
use crate::state::{AppState, ArtifactRef, BuildStatus};

pub async fn health() -> Json<Value> {
    Json(json!({ "ok": true }))
}

pub async fn list_projects(State(state): State<AppState>) -> Json<Value> {
    let names: Vec<&str> = state
        .config
        .projects
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    Json(json!({ "projects": names }))
}

pub async fn list_builds(State(state): State<AppState>) -> Json<Vec<BuildStatus>> {
    Json(state.list_builds().await)
}

pub async fn get_build(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<BuildStatus>, ApiError> {
    state
        .get_build(id)
        .await
        .map(Json)
        .ok_or_else(|| ApiError::not_found("build not found"))
}

pub async fn list_artifacts(State(state): State<AppState>) -> Json<Vec<ArtifactRef>> {
    let mut out = Vec::new();
    for b in state.list_builds().await {
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
    Json(body): Json<TriggerBody>,
) -> Result<Json<Value>, ApiError> {
    if state.config.project(&body.project).is_none() {
        return Err(ApiError::not_found(format!(
            "unknown project '{}'",
            body.project
        )));
    }
    let id = build::run_build(state, body.project.clone()).await?;
    Ok(Json(json!({ "build_id": id, "project": body.project })))
}
