use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;
use std::collections::HashMap;
use uuid::Uuid;

use crate::auth;
use crate::error::ApiError;
use crate::state::{AppState, BuildState};

pub async fn index() -> Response {
    let body = page(
        "Kei",
        r#"<h1>Kei</h1>
<ul class="nav">
  <li><a href="/builds">Builds</a></li>
  <li><a href="/artifacts/">Artifact files</a></li>
  <li><a href="/login">Login</a></li>
</ul>"#,
    );
    Html(body).into_response()
}

#[derive(Debug, Deserialize)]
pub struct LoginForm {
    account: String,
    token: String,
    #[serde(default)]
    next: String,
}

pub async fn login_form(Query(query): Query<HashMap<String, String>>) -> Response {
    let next = sanitize_next(query.get("next").map(String::as_str).unwrap_or("/"));
    Html(page("Login", &login_body(&next, None))).into_response()
}

pub async fn login_submit(State(state): State<AppState>, Form(form): Form<LoginForm>) -> Response {
    let next = sanitize_next(&form.next);
    if auth::authenticate_login(&form.account, &form.token, &state.config).is_none() {
        return Html(page(
            "Login",
            &login_body(&next, Some("Invalid account or token.")),
        ))
        .into_response();
    }
    let cookie = format!(
        "kei_token={}; Path=/; HttpOnly; SameSite=Lax; Max-Age=2592000",
        form.token
    );
    let mut response = Redirect::to(&next).into_response();
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

pub async fn logout() -> Response {
    let mut response = Redirect::to("/").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("kei_token=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"),
    );
    response
}

pub async fn list_builds(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let principal = auth::authenticate(&headers, &state.config);
    let builds: Vec<_> = state
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
    let mut rows = String::new();
    if builds.is_empty() {
        rows.push_str(r#"<tr><td colspan="5" class="muted">No builds yet.</td></tr>"#);
    } else {
        for b in &builds {
            let dur = match b.finished_at {
                Some(end) => format!("{}s", (end - b.started_at).num_seconds()),
                None => format!("{}s+", (chrono::Utc::now() - b.started_at).num_seconds()),
            };
            rows.push_str(&format!(
                r#"<tr>
  <td><a href="/builds/{id}">#{number}</a> <span class="muted"><code>{short}</code></span></td>
  <td>{project}</td>
  <td><span class="state {state_cls}">{state}</span></td>
  <td>{step}</td>
  <td class="muted">{started} · {dur}</td>
</tr>"#,
                id = b.id,
                number = b.number,
                short = short_id(&b.id),
                project = html_escape(&b.project),
                state_cls = state_class(&b.state),
                state = state_label(&b.state),
                step = b
                    .current_step
                    .as_deref()
                    .map(html_escape)
                    .unwrap_or_default(),
                started = b.started_at.format("%Y-%m-%d %H:%M:%S UTC"),
                dur = dur,
            ));
        }
    }
    let body = format!(
        r#"<h1><a href="/">Kei</a> — Builds</h1>
<table>
  <thead><tr><th>ID</th><th>Project</th><th>State</th><th>Step</th><th>Started · Duration</th></tr></thead>
  <tbody>{rows}</tbody>
</table>"#
    );
    Html(page("Builds", &body)).into_response()
}

pub async fn build_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Path(id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let b = state
        .get_build(id)
        .await
        .ok_or_else(|| ApiError::not_found("build not found"))?;
    let project = state
        .config
        .project(&b.project)
        .ok_or_else(|| ApiError::not_found("project not found"))?;
    let principal = auth::authenticate(&headers, &state.config);
    let query_token = auth::query_param(uri.query(), "token");
    auth::require_project_or_public_build_access(
        principal.as_ref(),
        project,
        state.config.auth.public_link_secret.as_deref(),
        id,
        query_token,
    )?;

    let live = matches!(b.state, BuildState::Queued | BuildState::Running);
    let can_stop = live
        && principal
            .as_ref()
            .is_some_and(|p| auth::can_view_project(Some(p), project));

    let dur = match b.finished_at {
        Some(end) => format!("{}s", (end - b.started_at).num_seconds()),
        None => format!(
            "{}s (running)",
            (chrono::Utc::now() - b.started_at).num_seconds()
        ),
    };

    let mut artifacts_items = String::new();
    for a in &b.artifacts {
        artifacts_items.push_str(&format!(
            r#"<li><a href="{url}">{path}</a> <span class="muted">({size})</span></li>"#,
            url = html_escape(&a.url),
            path = html_escape(&a.path),
            size = human_bytes(a.size),
        ));
    }
    let artifacts_section_style = if b.artifacts.is_empty() {
        "display:none"
    } else {
        ""
    };

    let (error_attr_style, error_text) = match &b.error {
        Some(e) => ("", html_escape(e)),
        None => ("display:none", String::new()),
    };

    let log_html = if b.log.is_empty() {
        r#"<p class="muted" id="log-empty">No output yet.</p><pre class="log" id="log" style="display:none"></pre>"#.to_string()
    } else {
        format!(
            r#"<p class="muted" id="log-empty" style="display:none">No output yet.</p><pre class="log" id="log">{}</pre>"#,
            html_escape(&b.log)
        )
    };

    // Polling script: re-fetches /api/builds/:id every 2s while the build is
    // live, patches in-place (no flash, no scroll reset), and only auto-scrolls
    // the log to the bottom if the user was already pinned there.
    let live_script = if live {
        format!(
            r#"<script>
(function() {{
  const buildId = {id:?};
  const token = new URLSearchParams(location.search).get('token');
  const NEAR_BOTTOM_PX = 24;
  const els = {{
    state: document.getElementById('state'),
    step: document.getElementById('step'),
    dur: document.getElementById('dur'),
    log: document.getElementById('log'),
    logEmpty: document.getElementById('log-empty'),
    artifactsSection: document.getElementById('artifacts-section'),
    artifacts: document.getElementById('artifacts'),
    errorBox: document.getElementById('error-box'),
    errorText: document.getElementById('error-text'),
    stop: document.getElementById('stop-build'),
  }};
  if (els.stop) {{
    els.stop.addEventListener('click', async () => {{
      els.stop.disabled = true;
      els.stop.textContent = 'Stopping...';
      let resp;
      try {{
        resp = await fetch('/api/builds/' + buildId + '/stop', {{ method: 'POST' }});
      }} catch (e) {{
        els.stop.disabled = false;
        els.stop.textContent = 'Stop build';
        return;
      }}
      if (resp.status === 401) {{
        location.href = '/login?next=' + encodeURIComponent(location.pathname + location.search);
        return;
      }}
      if (!resp.ok) {{
        els.stop.disabled = false;
        els.stop.textContent = 'Stop build';
        return;
      }}
      tick();
    }});
  }}
  async function tick() {{
    let resp;
    try {{
      const suffix = token ? '?token=' + encodeURIComponent(token) : '';
      resp = await fetch('/api/builds/' + buildId + suffix, {{ cache: 'no-store' }});
      if (!resp.ok) throw new Error('http ' + resp.status);
    }} catch (e) {{
      setTimeout(tick, 2000);
      return;
    }}
    const b = await resp.json();
    els.state.textContent = b.state;
    els.state.className = 'state ' + b.state;
    els.step.textContent = b.current_step || '—';
    const started = new Date(b.started_at);
    const end = b.finished_at ? new Date(b.finished_at) : new Date();
    const secs = Math.max(0, Math.round((end - started) / 1000));
    els.dur.textContent = secs + (b.finished_at ? 's' : 's (running)');
    const log = b.log || '';
    if (log.length) {{
      els.logEmpty.style.display = 'none';
      els.log.style.display = '';
      // Only auto-scroll if we were already near the bottom — preserves
      // user scroll position when they scroll up to read.
      const wasAtBottom =
        els.log.scrollHeight - els.log.scrollTop - els.log.clientHeight < NEAR_BOTTOM_PX;
      if (els.log.textContent !== log) {{
        els.log.textContent = log;
        if (wasAtBottom) els.log.scrollTop = els.log.scrollHeight;
      }}
    }}
    if (b.artifacts && b.artifacts.length) {{
      els.artifactsSection.style.display = '';
      els.artifacts.innerHTML = b.artifacts.map(a =>
        '<li><a href="' + a.url + '">' + a.path + '</a> <span class="muted">(' + a.size + ' B)</span></li>'
      ).join('');
    }}
    if (b.error) {{
      els.errorBox.style.display = '';
      els.errorText.textContent = b.error;
    }}
    if (b.state === 'queued' || b.state === 'running') {{
      setTimeout(tick, 2000);
    }} else if (els.stop) {{
      els.stop.remove();
    }}
  }}
  setTimeout(tick, 2000);
}})();
</script>"#,
            id = b.id.to_string()
        )
    } else {
        String::new()
    };

    let body = format!(
        r#"<h1><a href="/builds">Builds</a> / #{number} <span class="muted"><code>{short}</code></span></h1>
<dl class="meta">
  <dt>Project</dt><dd>{project}</dd>
  <dt>State</dt><dd><span class="state {state_cls}" id="state">{state}</span></dd>
  <dt>Current step</dt><dd id="step">{step}</dd>
  <dt>Commit</dt><dd><code>{commit}</code></dd>
  <dt>Started</dt><dd>{started}</dd>
  <dt>Duration</dt><dd id="dur">{dur}</dd>
</dl>
{stop_controls}
<div class="error" id="error-box" style="{error_style}"><strong>Error:</strong> <span id="error-text">{error_text}</span></div>
<section id="artifacts-section" style="{artifacts_style}"><h2>Artifacts</h2><ul class="artifacts" id="artifacts">{artifacts_items}</ul></section>
<h2>Log <a class="raw" href="/api/builds/{id}/log{raw_token}">raw</a></h2>
{log_html}
{live_script}"#,
        number = b.number,
        short = short_id(&b.id),
        project = html_escape(&b.project),
        state_cls = state_class(&b.state),
        state = state_label(&b.state),
        step = b
            .current_step
            .as_deref()
            .map(html_escape)
            .unwrap_or_else(|| "—".into()),
        commit = b.commit.as_deref().map(html_escape).unwrap_or_default(),
        started = b.started_at.format("%Y-%m-%d %H:%M:%S UTC"),
        dur = dur,
        stop_controls = if can_stop {
            r#"<div class="actions"><button class="danger" id="stop-build" type="button">Stop build</button></div>"#
        } else {
            ""
        },
        id = b.id,
        error_style = error_attr_style,
        artifacts_style = artifacts_section_style,
        artifacts_items = artifacts_items,
        raw_token = query_token
            .map(|token| format!("?token={token}"))
            .unwrap_or_default(),
    );
    let title = format!("Build #{}", b.number);
    Ok(Html(page(&title, &body)).into_response())
}

/// Serves anything under `/artifacts/...`. Directories render an HTML index;
/// files stream from disk with a mime type guessed from the extension.
/// Replaces tower-http ServeDir + nest_service, which had two issues for our
/// use case: (1) directories without index.html returned 404, and the
/// not_found_service fallback's status got mangled to 404; (2) the trailing-
/// slash redirect lost the `/artifacts` prefix because nest_service strips it.
pub async fn artifacts_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let principal = auth::authenticate(&headers, &state.config);
    let rel = tree_rel_path("/artifacts", uri.path());
    let Some(project_name) = rel.split('/').next().filter(|s| !s.is_empty()) else {
        return render_artifacts_root(&state, principal.as_ref()).await;
    };
    let Some(project) = state.config.project(project_name) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    if let Err(e) = auth::require_project_or_public_path_access(
        principal.as_ref(),
        project,
        state.config.auth.public_link_secret.as_deref(),
        &uri,
    ) {
        return e.into_response();
    }
    serve_tree(
        state.config.storage.artifacts_dir.clone(),
        "/artifacts",
        uri,
        true,
    )
    .await
}

pub async fn public_artifact_handler(State(state): State<AppState>, uri: Uri) -> Response {
    let rel = tree_rel_path("/public/artifacts", uri.path());
    if rel.is_empty() || rel.ends_with('/') {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let Some(token) = auth::query_param(uri.query(), "token") else {
        return (StatusCode::UNAUTHORIZED, "missing token").into_response();
    };
    if !auth::verify_public_artifact_token(
        state.config.auth.public_link_secret.as_deref(),
        rel,
        token,
    ) {
        return (StatusCode::FORBIDDEN, "bad token").into_response();
    }
    serve_tree(
        state.config.storage.artifacts_dir.clone(),
        "/public/artifacts",
        uri,
        false,
    )
    .await
}

/// Serves the configured Maven repository at `/maven/...`. Same file/dir
/// semantics as the artifacts handler; gated by the `maven` cargo feature.
#[cfg(feature = "maven")]
pub async fn maven_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let Some(root) = state.config.maven.repo_dir.clone() else {
        return (StatusCode::NOT_FOUND, "maven repo not configured").into_response();
    };
    let rel = tree_rel_path("/maven", uri.path());
    if let Some(project) = maven_project_for_path(&state, rel) {
        let principal = auth::authenticate(&headers, &state.config);
        if let Err(e) = auth::require_project_maven_access(principal.as_ref(), project) {
            return e.into_response();
        }
    }
    serve_tree(root, "/maven", uri, false).await
}

#[cfg(feature = "maven")]
fn maven_project_for_path<'a>(
    state: &'a AppState,
    rel: &str,
) -> Option<&'a crate::config::ProjectConfig> {
    let segments: Vec<&str> = rel
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    state.config.projects.iter().find(|project| {
        project
            .maven
            .artifacts
            .iter()
            .any(|artifact| segments.iter().any(|segment| segment == artifact))
    })
}

async fn serve_tree(
    root: std::path::PathBuf,
    prefix: &str,
    uri: Uri,
    allow_directory_listing: bool,
) -> Response {
    let raw_path = uri.path();
    let rel = tree_rel_path(prefix, raw_path);

    let mut fs_path = root.clone();
    if !rel.is_empty() {
        // Reject path traversal — only accept clean, non-empty segments.
        for seg in rel.trim_end_matches('/').split('/') {
            if seg.is_empty() || seg == "." || seg == ".." {
                return (StatusCode::NOT_FOUND, "not found").into_response();
            }
            fs_path.push(seg);
        }
    }

    // Resolve symlinks in both root and target, then verify target stays
    // under root. Without this, a symlink inside an artifact directory
    // (e.g. planted by a malicious build step) could escape to /etc.
    let canonical_root = match tokio::fs::canonicalize(&root).await {
        Ok(c) => c,
        Err(_) => return (StatusCode::NOT_FOUND, "not found").into_response(),
    };
    let canonical = match tokio::fs::canonicalize(&fs_path).await {
        Ok(c) => c,
        Err(_) => return (StatusCode::NOT_FOUND, "not found").into_response(),
    };
    if !canonical.starts_with(&canonical_root) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    let meta = match tokio::fs::metadata(&canonical).await {
        Ok(m) => m,
        Err(_) => return (StatusCode::NOT_FOUND, "not found").into_response(),
    };

    if meta.is_file() {
        return serve_file(&canonical, meta.len()).await;
    }
    if !meta.is_dir() {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    if !allow_directory_listing {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    if !raw_path.ends_with('/') {
        return Redirect::permanent(&format!("{raw_path}/")).into_response();
    }

    render_directory(&canonical, rel, &canonical_root).await
}

fn tree_rel_path<'a>(prefix: &str, raw_path: &'a str) -> &'a str {
    raw_path
        .strip_prefix(prefix)
        .unwrap_or("")
        .trim_start_matches('/')
}

async fn render_artifacts_root(state: &AppState, principal: Option<&auth::Principal>) -> Response {
    let mut rows = String::new();
    for project in auth::visible_projects(principal, &state.config) {
        let dir = state.config.storage.artifacts_dir.join(&project.name);
        if tokio::fs::metadata(&dir)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false)
        {
            let name = html_escape(&project.name);
            rows.push_str(&format!(
                r#"<tr><td><a href="{name}/">{name}/</a></td><td class="muted"></td><td class="muted"></td></tr>"#
            ));
        }
    }
    if rows.is_empty() {
        rows.push_str(r#"<tr><td colspan="3" class="muted">No artifacts yet.</td></tr>"#);
    }
    let body = format!(
        r#"<h1><a href="/">Kei</a> — Artifacts</h1>
<table>
  <thead><tr><th>Name</th><th>Size</th><th>Modified</th></tr></thead>
  <tbody>{rows}</tbody>
</table>"#
    );
    Html(page("Artifacts", &body)).into_response()
}

async fn render_directory(
    fs_path: &std::path::Path,
    rel: &str,
    canonical_root: &std::path::Path,
) -> Response {
    let mut rd = match tokio::fs::read_dir(fs_path).await {
        Ok(r) => r,
        Err(_) => return (StatusCode::NOT_FOUND, "not found").into_response(),
    };
    let mut items: Vec<(String, bool, u64, Option<chrono::DateTime<chrono::Utc>>)> = Vec::new();
    while let Ok(Some(e)) = rd.next_entry().await {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        // Hide symlinks whose target escapes the artifact root — a malicious
        // build step could plant `evil -> /etc/passwd` and a listing would
        // otherwise reveal its size/mtime. serve_tree blocks the actual
        // fetch, but we don't want the link visible at all.
        let link_meta = match tokio::fs::symlink_metadata(e.path()).await {
            Ok(m) => m,
            Err(_) => continue,
        };
        if link_meta.file_type().is_symlink() {
            match tokio::fs::canonicalize(e.path()).await {
                Ok(c) if c.starts_with(canonical_root) => {}
                _ => continue,
            }
        }
        // Follow symlinks (the per-project `latest` link points at a build
        // directory) so size and is_dir reflect the target, not the link.
        let m = match tokio::fs::metadata(e.path()).await {
            Ok(m) => m,
            Err(_) => continue,
        };
        let modified = m.modified().ok().map(chrono::DateTime::<chrono::Utc>::from);
        items.push((name, m.is_dir(), m.len(), modified));
    }
    // `latest` (the per-project symlink to the most recent build) always
    // pins to the top. Below that: most-recently-modified first, with
    // directories before files within each modtime group, then alphabetical.
    items.sort_by(|a, b| {
        let a_latest = a.0 == "latest";
        let b_latest = b.0 == "latest";
        b_latest
            .cmp(&a_latest)
            .then_with(|| b.3.cmp(&a.3))
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| a.0.cmp(&b.0))
    });

    let mut rows = String::new();
    if !rel.is_empty() {
        rows.push_str(
            r#"<tr><td><a href="../">../</a></td><td class="muted"></td><td class="muted"></td></tr>"#,
        );
    }
    if items.is_empty() && rel.is_empty() {
        rows.push_str(r#"<tr><td colspan="3" class="muted">No artifacts yet.</td></tr>"#);
    }
    for (name, is_dir, size, modified) in &items {
        let escaped = html_escape(name);
        let (href, label, size_cell) = if *is_dir {
            (format!("{escaped}/"), format!("{escaped}/"), String::new())
        } else {
            (escaped.clone(), escaped, human_bytes(*size))
        };
        let mtime_cell = modified
            .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_default();
        rows.push_str(&format!(
            r#"<tr><td><a href="{href}">{label}</a></td><td class="muted">{size_cell}</td><td class="muted">{mtime_cell}</td></tr>"#
        ));
    }

    let crumb = if rel.is_empty() {
        "Artifacts".to_string()
    } else {
        format!("Artifacts / {}", html_escape(rel.trim_end_matches('/')))
    };
    let body = format!(
        r#"<h1><a href="/">Kei</a> — {crumb}</h1>
<table>
  <thead><tr><th>Name</th><th>Size</th><th>Modified</th></tr></thead>
  <tbody>{rows}</tbody>
</table>"#
    );
    Html(page(&crumb, &body)).into_response()
}

async fn serve_file(fs_path: &std::path::Path, size: u64) -> Response {
    // Stream the file instead of buffering it. tokio::fs::read previously
    // pulled the whole jar into RAM, so a multi-GB artifact (or many
    // concurrent requests for one) translated directly into RSS pressure.
    let file = match tokio::fs::File::open(fs_path).await {
        Ok(f) => f,
        Err(_) => return (StatusCode::NOT_FOUND, "not found").into_response(),
    };
    let stream = tokio_util::io::ReaderStream::new(file);
    let body = axum::body::Body::from_stream(stream);
    let mut resp = body.into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(mime_for(fs_path)),
    );
    if let Ok(v) = HeaderValue::from_str(&size.to_string()) {
        resp.headers_mut().insert(header::CONTENT_LENGTH, v);
    }
    resp
}

fn mime_for(path: &std::path::Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase());
    match ext.as_deref() {
        Some("jar") => "application/java-archive",
        Some("zip") => "application/zip",
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("json") => "application/json",
        Some("log") | Some("txt") => "text/plain; charset=utf-8",
        Some("xml") => "application/xml",
        Some("css") => "text/css",
        Some("js") => "application/javascript",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

/// Hex-only sha guard — accept `[0-9a-fA-F]+` and nothing else, so a malicious
/// `:sha` like `..` can't be used to escape the workspace.
fn valid_sha(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Locate `project`'s workspace dir without falling through to arbitrary
/// filesystem paths supplied by URL.
fn workspace_for(state: &AppState, project: &str) -> Option<std::path::PathBuf> {
    let p = state.config.project(project)?;
    Some(state.config.storage.workspace_dir.join(&p.name))
}

pub async fn commit_view(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Path((project, sha)): Path<(String, String)>,
) -> Response {
    if !valid_sha(&sha) {
        return (StatusCode::BAD_REQUEST, "invalid sha").into_response();
    }
    let Some(workspace) = workspace_for(&state, &project) else {
        return (StatusCode::NOT_FOUND, "no such project").into_response();
    };
    let Some(project_cfg) = state.config.project(&project) else {
        return (StatusCode::NOT_FOUND, "no such project").into_response();
    };
    let principal = auth::authenticate(&headers, &state.config);
    if let Err(e) = auth::require_project_or_public_path_access(
        principal.as_ref(),
        project_cfg,
        state.config.auth.public_link_secret.as_deref(),
        &uri,
    ) {
        return e.into_response();
    }
    let info = match crate::git::show_commit(&workspace, &sha).await {
        Ok(i) => i,
        Err(_) => return (StatusCode::NOT_FOUND, "commit not found").into_response(),
    };

    let body = format!(
        r#"<h1><a href="/">Kei</a> — {project_esc} · <code>{short}</code></h1>
<dl class="meta">
  <dt>Project</dt><dd>{project_esc}</dd>
  <dt>SHA</dt><dd><code>{sha_esc}</code></dd>
  <dt>Author</dt><dd>{author}</dd>
  <dt>Date</dt><dd>{date}</dd>
  <dt>Subject</dt><dd>{subject}</dd>
</dl>
<h2>Message</h2>
<pre class="log">{body_text}</pre>
<h2>Files changed</h2>
<pre class="log">{stat}</pre>
<h2>Diff</h2>
{diff_html}"#,
        project_esc = html_escape(&project),
        short = html_escape(&info.sha.chars().take(7).collect::<String>()),
        sha_esc = html_escape(&info.sha),
        author = html_escape(&info.author),
        date = html_escape(&info.date),
        subject = html_escape(&info.subject),
        body_text = html_escape(&info.body),
        stat = html_escape(&info.stat),
        diff_html = render_diff(&info.diff),
    );
    Html(page(
        &format!("Commit {} · {}", short_str(&info.sha), project),
        &body,
    ))
    .into_response()
}

/// Renders a unified diff as line-classified `<div>`s — GitHub-style
/// red/green coloring for removals/additions, blue for hunk headers,
/// muted gray for the file/index metadata.
fn render_diff(text: &str) -> String {
    if text.trim().is_empty() {
        return r#"<p class="muted">No diff (empty commit?).</p>"#.to_string();
    }
    let mut out = String::with_capacity(text.len() * 2);
    out.push_str(r#"<div class="diff">"#);
    for line in text.lines() {
        let class = if line.starts_with("diff --git")
            || line.starts_with("index ")
            || line.starts_with("new file mode")
            || line.starts_with("deleted file mode")
            || line.starts_with("rename ")
            || line.starts_with("similarity ")
            || line.starts_with("Binary files")
        {
            "diff-meta"
        } else if line.starts_with("---") || line.starts_with("+++") {
            "diff-fileheader"
        } else if line.starts_with("@@") {
            "diff-hunk"
        } else if line.starts_with('+') {
            "diff-add"
        } else if line.starts_with('-') {
            "diff-del"
        } else {
            "diff-ctx"
        };
        out.push_str(&format!(
            r#"<div class="{class}">{}</div>"#,
            // Render empty lines with a non-breaking space so the row still
            // has height and shows its background color.
            if line.is_empty() {
                "&nbsp;".to_string()
            } else {
                html_escape(line)
            }
        ));
    }
    out.push_str("</div>");
    out
}

pub async fn compare_view(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Path((project, range)): Path<(String, String)>,
) -> Response {
    let (from, to) = match range.split_once("...") {
        Some((a, b)) if valid_sha(a) && valid_sha(b) => (a.to_string(), b.to_string()),
        _ => return (StatusCode::BAD_REQUEST, "invalid range").into_response(),
    };
    let Some(workspace) = workspace_for(&state, &project) else {
        return (StatusCode::NOT_FOUND, "no such project").into_response();
    };
    let Some(project_cfg) = state.config.project(&project) else {
        return (StatusCode::NOT_FOUND, "no such project").into_response();
    };
    let principal = auth::authenticate(&headers, &state.config);
    if let Err(e) = auth::require_project_or_public_path_access(
        principal.as_ref(),
        project_cfg,
        state.config.auth.public_link_secret.as_deref(),
        &uri,
    ) {
        return e.into_response();
    }
    let commits = match crate::git::log_range(&workspace, &from, &to).await {
        Ok(c) => c,
        Err(_) => return (StatusCode::NOT_FOUND, "compare not available").into_response(),
    };

    let mut rows = String::new();
    if commits.is_empty() {
        rows.push_str(r#"<tr><td colspan="3" class="muted">No commits in range.</td></tr>"#);
    } else {
        for c in &commits {
            rows.push_str(&format!(
                r#"<tr><td><a href="/commits/{project}/{sha}"><code>{short}</code></a></td><td>{subject}</td><td class="muted">{author} · {date}</td></tr>"#,
                project = html_escape(&project),
                sha = html_escape(&c.sha),
                short = html_escape(&c.sha.chars().take(7).collect::<String>()),
                subject = html_escape(&c.subject),
                author = html_escape(&c.author),
                date = html_escape(&c.date),
            ));
        }
    }

    let body = format!(
        r#"<h1><a href="/">Kei</a> — {project_esc} · <code>{from_short}</code>…<code>{to_short}</code></h1>
<p class="muted">{count} commits</p>
<table>
  <thead><tr><th>SHA</th><th>Subject</th><th>Author · Date</th></tr></thead>
  <tbody>{rows}</tbody>
</table>"#,
        project_esc = html_escape(&project),
        from_short = html_escape(&short_str(&from)),
        to_short = html_escape(&short_str(&to)),
        count = commits.len(),
    );
    Html(page(
        &format!(
            "{} compare {}…{}",
            project,
            short_str(&from),
            short_str(&to)
        ),
        &body,
    ))
    .into_response()
}

fn short_str(sha: &str) -> String {
    sha.chars().take(7).collect()
}

fn sanitize_next(next: &str) -> String {
    if next.starts_with('/') && !next.starts_with("//") && !next.starts_with("/login") {
        next.to_string()
    } else {
        "/".to_string()
    }
}

fn login_body(next: &str, error: Option<&str>) -> String {
    let error_html = error
        .map(|msg| format!(r#"<div class="error">{}</div>"#, html_escape(msg)))
        .unwrap_or_default();
    format!(
        r#"<h1><a href="/">Kei</a> — Login</h1>
{error_html}
<form class="login" method="post" action="/login">
  <input type="hidden" name="next" value="{next}">
  <label>Account <input name="account" autocomplete="username" required autofocus></label>
  <label>Token <input name="token" type="password" autocomplete="current-password" required></label>
  <button type="submit">Login</button>
</form>"#,
        next = html_escape(next),
    )
}

pub async fn build_log_raw(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Path(id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let b = state
        .get_build(id)
        .await
        .ok_or_else(|| ApiError::not_found("build not found"))?;
    let project = state
        .config
        .project(&b.project)
        .ok_or_else(|| ApiError::not_found("project not found"))?;
    let principal = auth::authenticate(&headers, &state.config);
    auth::require_project_or_public_build_access(
        principal.as_ref(),
        project,
        state.config.auth.public_link_secret.as_deref(),
        id,
        query.get("token").map(String::as_str),
    )?;
    let mut resp = (StatusCode::OK, b.log).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    Ok(resp)
}

fn page(title: &str, body: &str) -> String {
    let title = html_escape(title);
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>{title} · Kei</title>
<style>
  :root {{
    --bg: #0f1115; --fg: #d6dae0; --muted: #7c8492; --accent: #6cb6ff;
    --ok: #3fb950; --warn: #d29922; --err: #f85149; --run: #6cb6ff;
    --panel: #161922; --border: #232936;
  }}
  * {{ box-sizing: border-box; }}
  html, body {{ background: var(--bg); color: var(--fg); }}
  body {{ font: 14px/1.5 ui-sans-serif, system-ui, sans-serif;
         margin: 0; padding: 24px; max-width: 1100px; margin-inline: auto; }}
  h1, h2 {{ margin: 0.6em 0 0.4em; font-weight: 600; }}
  h1 {{ font-size: 1.4em; }}
  h2 {{ font-size: 1.1em; border-bottom: 1px solid var(--border); padding-bottom: 4px; }}
  a {{ color: var(--accent); text-decoration: none; }}
  a:hover {{ text-decoration: underline; }}
  code {{ font-family: ui-monospace, Menlo, Consolas, monospace; }}
  .muted {{ color: var(--muted); }}
  .nav {{ list-style: none; padding: 0; display: flex; gap: 16px; }}
  table {{ width: 100%; border-collapse: collapse; }}
  th, td {{ text-align: left; padding: 8px 10px; border-bottom: 1px solid var(--border); }}
  th {{ color: var(--muted); font-weight: 500; font-size: 0.85em; text-transform: uppercase; letter-spacing: 0.05em; }}
  .state {{ display: inline-block; padding: 2px 8px; border-radius: 999px; font-size: 0.8em; font-weight: 600; }}
  .state.queued {{ background: #21262d; color: var(--muted); }}
  .state.running {{ background: rgba(108,182,255,0.15); color: var(--run); }}
  .state.success {{ background: rgba(63,185,80,0.15); color: var(--ok); }}
  .state.failed {{ background: rgba(248,81,73,0.15); color: var(--err); }}
  .state.canceled {{ background: rgba(210,153,34,0.15); color: var(--warn); }}
  dl.meta {{ display: grid; grid-template-columns: max-content 1fr; gap: 4px 18px; margin: 12px 0; }}
  dl.meta dt {{ color: var(--muted); }}
  dl.meta dd {{ margin: 0; }}
  pre.log {{ background: var(--panel); border: 1px solid var(--border);
             padding: 14px; border-radius: 8px; overflow-x: auto;
             font: 12.5px/1.55 ui-monospace, Menlo, Consolas, monospace;
             white-space: pre-wrap; word-break: break-word;
             max-height: 70vh; }}
  .error {{ background: rgba(248,81,73,0.1); border: 1px solid var(--err);
            padding: 10px 14px; border-radius: 6px; margin: 12px 0; }}
  form.login {{ max-width: 360px; display: grid; gap: 12px; }}
  form.login label {{ display: grid; gap: 4px; color: var(--muted); }}
  form.login input {{ width: 100%; background: var(--panel); color: var(--fg);
                      border: 1px solid var(--border); border-radius: 6px;
                      padding: 8px 10px; font: inherit; }}
  form.login button {{ justify-self: start; background: var(--accent); color: #07111f;
                       border: 0; border-radius: 6px; padding: 8px 14px;
                       font: inherit; font-weight: 600; cursor: pointer; }}
  .actions {{ margin: 14px 0; }}
  button.danger {{ background: var(--err); color: #fff; border: 0; border-radius: 6px;
                   padding: 8px 14px; font: inherit; font-weight: 600; cursor: pointer; }}
  button.danger:disabled {{ opacity: 0.65; cursor: wait; }}
  ul.artifacts {{ padding-left: 18px; }}
  a.raw {{ font-size: 0.75em; color: var(--muted); margin-left: 8px; }}
  .diff {{ background: var(--panel); border: 1px solid var(--border);
           border-radius: 8px; overflow-x: auto;
           font: 12.5px/1.55 ui-monospace, Menlo, Consolas, monospace; }}
  .diff > div {{ padding: 0 14px; white-space: pre-wrap; word-break: break-word; }}
  .diff-add {{ background: rgba(63,185,80,0.18); color: #b4f1bd; }}
  .diff-del {{ background: rgba(248,81,73,0.18); color: #fbb9b3; }}
  .diff-hunk {{ background: rgba(108,182,255,0.10); color: var(--accent); }}
  .diff-meta {{ color: var(--muted); }}
  .diff-fileheader {{ color: var(--muted); font-weight: 600; }}
  .diff-ctx {{ color: var(--fg); }}
</style>
</head>
<body>
{body}
</body>
</html>
"#
    )
}

fn state_class(s: &BuildState) -> &'static str {
    match s {
        BuildState::Queued => "queued",
        BuildState::Running => "running",
        BuildState::Success => "success",
        BuildState::Failed => "failed",
        BuildState::Canceled => "canceled",
    }
}

fn state_label(s: &BuildState) -> &'static str {
    match s {
        BuildState::Queued => "queued",
        BuildState::Running => "running",
        BuildState::Success => "success",
        BuildState::Failed => "failed",
        BuildState::Canceled => "canceled",
    }
}

fn short_id(id: &Uuid) -> String {
    id.to_string().chars().take(8).collect()
}

fn human_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    const TB: u64 = GB * 1024;
    if n >= TB {
        format!("{:.2} TB", n as f64 / TB as f64)
    } else if n >= GB {
        format!("{:.2} GB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.2} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else {
        format!("{n} B")
    }
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}
