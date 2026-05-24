use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use tracing::info;

use crate::build;
use crate::error::ApiError;
use crate::state::AppState;

type HmacSha256 = Hmac<Sha256>;

/// GitHub webhook entry point. Verifies `X-Hub-Signature-256` if a secret
/// is configured, then dispatches `push` events to a matching project.
pub async fn handle(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    if let Some(secret) = state.config.github.webhook_secret.as_deref() {
        let sig = headers
            .get("x-hub-signature-256")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "missing signature"))?;
        let sig = sig.strip_prefix("sha256=").unwrap_or(sig);
        let expected = hex::decode(sig)
            .map_err(|_| ApiError::bad_request("invalid signature encoding"))?;
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
            .map_err(|_| ApiError::internal("hmac key error"))?;
        mac.update(&body);
        mac.verify_slice(&expected)
            .map_err(|_| ApiError::new(StatusCode::UNAUTHORIZED, "bad signature"))?;
    }

    let event = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if event == "ping" {
        return Ok(Json(json!({ "ok": true, "msg": "pong" })));
    }
    if event != "push" {
        info!(event, "ignoring github event");
        return Ok(Json(json!({ "ok": true, "ignored": event })));
    }

    let payload: PushPayload = serde_json::from_slice(&body)
        .map_err(|e| ApiError::bad_request(format!("invalid push payload: {e}")))?;

    let branch = payload
        .ref_field
        .strip_prefix("refs/heads/")
        .unwrap_or(&payload.ref_field);

    let project = state
        .config
        .project_for_full_name(&payload.repository.full_name)
        .filter(|p| p.branch == branch);

    let Some(project) = project else {
        info!(
            repo = %payload.repository.full_name,
            branch,
            "no matching project"
        );
        return Ok(Json(json!({ "ok": true, "ignored": "no matching project" })));
    };

    // Honour the `[skip ci]` marker. A single push can contain multiple
    // commits; only skip when EVERY commit in the push carries the marker
    // (otherwise a mixed push with one tagged commit + one real change
    // would silently skip the real change). Falls back to head_commit when
    // the `commits` array is empty.
    let messages: Vec<&str> = if !payload.commits.is_empty() {
        payload.commits.iter().map(|c| c.message.as_str()).collect()
    } else if let Some(hc) = payload.head_commit.as_ref() {
        vec![hc.message.as_str()]
    } else {
        vec![]
    };
    let is_skip = |m: &str| {
        let lc = m.trim().to_ascii_lowercase();
        lc.ends_with("[skip ci]") || lc.ends_with("[ci skip]")
    };
    if !messages.is_empty() && messages.iter().all(|m| is_skip(m)) {
        info!(
            repo = %payload.repository.full_name,
            branch,
            commits = messages.len(),
            "all commits in push tagged [skip ci]; not triggering build"
        );
        return Ok(Json(json!({ "ok": true, "ignored": "skip ci" })));
    }

    let build_id = build::run_build(state.clone(), project.name.clone()).await?;

    Ok(Json(json!({
        "ok": true,
        "build_id": build_id,
        "project": project.name,
    })))
}

#[derive(Debug, Deserialize)]
struct PushPayload {
    #[serde(rename = "ref")]
    ref_field: String,
    repository: PushRepo,
    #[serde(default)]
    head_commit: Option<HeadCommit>,
    #[serde(default)]
    commits: Vec<HeadCommit>,
}

#[derive(Debug, Deserialize)]
struct PushRepo {
    full_name: String,
}

#[derive(Debug, Deserialize)]
struct HeadCommit {
    message: String,
}
