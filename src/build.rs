use anyhow::Context;
use chrono::Utc;
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::{ProjectBuildConfig, ProjectConfig};
use crate::state::{AppState, BuildState};
use crate::{artifacts, git, notify, runner};

/// Register a new build for `project_name` and start it on a background task.
/// Returns the new build id immediately — the caller (webhook or trigger
/// endpoint) does not block on the build.
pub async fn run_build(state: AppState, project_name: String) -> anyhow::Result<Uuid> {
    let project = state
        .config
        .project(&project_name)
        .ok_or_else(|| anyhow::anyhow!("unknown project: {project_name}"))?
        .clone();
    let build_id = state.create_build(&project_name).await;

    let st = state.clone();
    tokio::spawn(async move {
        let outcome = run_build_inner(st.clone(), build_id, &project).await;
        let succeeded = if let Err(e) = &outcome {
            warn!(error=%e, build=%build_id, "build error");
            st.append_log(build_id, &format!("\n[error] {e:#}\n")).await;
            st.update_build(build_id, |b| {
                b.state = BuildState::Failed;
                b.finished_at = Some(Utc::now());
                b.error = Some(format!("{e:#}"));
                b.current_step = None;
            })
            .await;
            false
        } else {
            true
        };
        // Persist to disk before notifying so the link in the Discord embed
        // (and the build list page) survive a restart.
        st.persist_build(build_id).await;

        // Notifications are best-effort; never block or fail builds.
        if let Some(build) = st.get_build(build_id).await {
            notify::discord_notify(
                &project,
                st.config.server.public_url.as_deref(),
                &build,
                succeeded,
            )
            .await;
        }
    });

    Ok(build_id)
}

async fn run_build_inner(
    state: AppState,
    build_id: Uuid,
    project: &ProjectConfig,
) -> anyhow::Result<()> {
    // Serialize ALL builds: two projects targeting the same Minecraft
    // version race on the shared loom cache (~/.gradle/caches/fabric-loom/
    // <mc>/...mappings.tiny), so per-project locking isn't enough — we
    // need a global one. This also subsumes the previous per-project lock
    // (the workspace can't be touched by two builds of the same project at
    // once, but if no two builds run concurrently at all, neither can).
    let _guard = state.build_lock.lock().await;

    // Snapshot the previous-success commit (if any) so the post-build
    // notification can include a compare link showing what changed.
    let previous = state.last_built_commit(&project.name).await;
    state
        .update_build(build_id, |b| {
            b.state = BuildState::Running;
            b.started_at = Utc::now();
            b.previous_commit = previous;
        })
        .await;

    let workspace = state.config.storage.workspace_dir.join(&project.name);
    let artifacts_dir = state.config.storage.artifacts_dir.clone();
    let build_number = state
        .get_build(build_id)
        .await
        .map(|b| b.number)
        .unwrap_or(0);

    // 1. Force-sync the workspace to origin/<branch>
    state
        .update_build(build_id, |b| b.current_step = Some("git-sync".into()))
        .await;
    let (sync_log, sync_result) =
        git::sync(&project.repo_url, &project.branch, &workspace).await;
    state.append_log(build_id, &sync_log).await;
    let head = sync_result.context("git sync")?;
    state
        .append_log(build_id, &format!("\n[git] HEAD={head}\n"))
        .await;
    state
        .update_build(build_id, |b| b.commit = Some(head))
        .await;

    // 2. Run configured steps (gradlew, copy commands, ...). Prefer the
    //    project's in-tree kei.toml (loaded from workspace); fall back to
    //    the steps/artifacts on the global registration entry.
    let build_cfg = match ProjectBuildConfig::load_from_workspace(&workspace) {
        Ok(Some(c)) => {
            state
                .append_log(build_id, "\n[config] using workspace/kei.toml\n")
                .await;
            c
        }
        Ok(None) => ProjectBuildConfig {
            nix: project.nix.clone(),
            steps: project.steps.clone(),
            artifacts: project.artifacts.clone(),
        },
        Err(e) => {
            anyhow::bail!("loading workspace kei.toml: {e}");
        }
    };

    for step in &build_cfg.steps {
        state
            .update_build(build_id, |b| b.current_step = Some(step.name.clone()))
            .await;
        state
            .append_log(build_id, &format!("\n=== step: {} ===\n", step.name))
            .await;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let st_drain = state.clone();
        let drain = tokio::spawn(async move {
            while let Some(chunk) = rx.recv().await {
                st_drain.append_log(build_id, &chunk).await;
            }
        });

        let outcome =
            runner::run_step(step, &workspace, &state.config.nix, &build_cfg.nix, tx)
                .await
                .with_context(|| format!("run step {}", step.name))?;
        // Senders drop when run_step returns, so the drain task ends after
        // flushing any tail messages still in the channel.
        let _ = drain.await;

        if !outcome.success {
            anyhow::bail!(
                "step '{}' failed (exit {:?})",
                step.name,
                outcome.code
            );
        }
    }

    // If a step created and pushed a commit (e.g. update-docs), HEAD will
    // have drifted from the post-sync commit. Record that drift so the
    // notification can link to it. Errors here are non-fatal — the build
    // succeeded regardless.
    if let Ok(post_head) = git::current_head(&workspace).await {
        let sync_head = state.get_build(build_id).await.and_then(|b| b.commit);
        if sync_head.as_deref() != Some(post_head.as_str()) {
            state
                .update_build(build_id, |b| b.docs_commit = Some(post_head))
                .await;
        }
    }

    // 3. Collect artifacts via configured glob patterns.
    state
        .update_build(build_id, |b| b.current_step = Some("artifacts".into()))
        .await;
    let collected = artifacts::collect(
        &project.name,
        build_id,
        build_number,
        &workspace,
        &artifacts_dir,
        &build_cfg.artifacts,
    )
    .await
    .context("collect artifacts")?;

    state
        .append_log(
            build_id,
            &format!("\n[artifacts] collected {} files\n", collected.len()),
        )
        .await;
    for a in &collected {
        state
            .append_log(build_id, &format!("  - {} ({} bytes)\n", a.path, a.size))
            .await;
    }

    // 4. Static-file exposure: artifacts are served by the ServeDir mounted
    //    at `/artifacts`. Refresh the `latest` symlink so consumers can use
    //    a stable URL.
    if let Err(e) = update_latest_link(&artifacts_dir, &project.name, build_id).await {
        state
            .append_log(
                build_id,
                &format!("[warn] could not update latest link: {e}\n"),
            )
            .await;
    }

    state
        .update_build(build_id, |b| {
            b.artifacts = collected;
            b.state = BuildState::Success;
            b.finished_at = Some(Utc::now());
            b.current_step = None;
        })
        .await;

    // Remember what commit this success covered so we can detect remote drift
    // across restarts (see `bootstrap_initial_builds`). When a build step
    // pushed a new commit (e.g. update-docs), record THAT as the last built
    // commit — otherwise bootstrap on the next restart would see the bot's
    // [skip ci] commit as "remote moved" and rebuild it.
    let head_for_last = state.get_build(build_id).await.and_then(|b| {
        b.docs_commit.clone().or(b.commit.clone())
    });
    if let Some(head) = head_for_last {
        state.set_last_built_commit(&project.name, &head).await;
    }

    info!(build=%build_id, project=%project.name, "build succeeded");
    Ok(())
}

/// On startup, walk every configured project and trigger a build whenever the
/// remote tip doesn't match the persisted `.last_commit`. New projects (no
/// `.last_commit` yet) always get an initial build. Errors are logged and
/// skipped — they shouldn't crash startup.
pub async fn bootstrap_initial_builds(state: AppState) {
    for project in state.config.projects.clone() {
        let last = state.last_built_commit(&project.name).await;
        let remote = match git::remote_head(&project.repo_url, &project.branch).await {
            Ok(h) => h,
            Err(e) => {
                warn!(error=%e, project=%project.name, "remote-head probe failed; skipping auto-trigger");
                continue;
            }
        };
        if last.as_deref() == Some(remote.as_str()) {
            info!(project=%project.name, commit=%remote, "remote unchanged; skipping auto-trigger");
            continue;
        }
        info!(
            project=%project.name,
            from=?last,
            to=%remote,
            "remote moved (or new project); triggering build"
        );
        if let Err(e) = run_build(state.clone(), project.name.clone()).await {
            warn!(error=%e, project=%project.name, "auto-trigger failed");
        }
    }
}

#[cfg(unix)]
async fn update_latest_link(
    artifacts_dir: &std::path::Path,
    project: &str,
    build_id: Uuid,
) -> anyhow::Result<()> {
    let link = artifacts_dir.join(project).join("latest");
    let target = build_id.to_string();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        if std::fs::symlink_metadata(&link).is_ok() {
            std::fs::remove_file(&link)
                .or_else(|_| std::fs::remove_dir_all(&link))?;
        }
        std::os::unix::fs::symlink(target, &link)
    })
    .await??;
    Ok(())
}

#[cfg(not(unix))]
async fn update_latest_link(
    _artifacts_dir: &std::path::Path,
    _project: &str,
    _build_id: Uuid,
) -> anyhow::Result<()> {
    Ok(())
}
