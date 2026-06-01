use crate::config::ProjectConfig;
use crate::state::{BuildState, BuildStatus};

/// Notify the project's Discord targets about a finished build. No-op when
/// the `discord` cargo feature is disabled — the call site doesn't need to
/// know whether the feature is compiled in.
pub async fn discord_notify(
    project: &ProjectConfig,
    public_url: Option<&str>,
    public_link_secret: Option<&str>,
    build: &BuildStatus,
) {
    if matches!(build.state, BuildState::Queued | BuildState::Running) {
        return;
    }

    #[cfg(feature = "discord")]
    discord::send(project, public_url, public_link_secret, build).await;

    #[cfg(not(feature = "discord"))]
    {
        let _ = (project, public_url, public_link_secret, build);
    }
}

#[cfg(feature = "discord")]
mod discord {
    use super::{BuildStatus, ProjectConfig};
    use crate::config::DiscordTarget;
    use crate::state::BuildState;
    use serde_json::{Value, json};
    use tracing::{info, warn};

    pub async fn send(
        project: &ProjectConfig,
        public_url: Option<&str>,
        public_link_secret: Option<&str>,
        build: &BuildStatus,
    ) {
        let targets = &project.notify.discord;
        if targets.is_empty() {
            return;
        }
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                warn!(error=%e, "discord: failed to construct http client");
                return;
            }
        };
        for target in targets {
            let payload = build_payload(project, target, public_url, public_link_secret, build);
            let url = match &target.thread_id {
                Some(tid) => format!("{}?thread_id={}&wait=true", target.url, tid),
                None => format!("{}?wait=true", target.url),
            };
            match client.post(&url).json(&payload).send().await {
                Ok(resp) if resp.status().is_success() => {
                    info!(project=%build.project, "discord: notified");
                }
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    warn!(
                        project=%build.project,
                        status=%status,
                        body=%body,
                        "discord: webhook rejected"
                    );
                }
                Err(e) => warn!(project=%build.project, error=%e, "discord: post failed"),
            }
        }
    }

    fn build_payload(
        project: &ProjectConfig,
        target: &DiscordTarget,
        public_url: Option<&str>,
        public_link_secret: Option<&str>,
        build: &BuildStatus,
    ) -> Value {
        let (status_word, color) = match build.state {
            BuildState::Success => ("succeeded", 0x3FB950),
            BuildState::Failed => ("failed", 0xF85149),
            BuildState::Canceled => ("canceled", 0xD29922),
            BuildState::Queued | BuildState::Running => ("finished", 0x6CB6FF),
        };
        let title_tpl = target
            .title
            .clone()
            .unwrap_or_else(|| "{project} #{number} {status}".to_string());
        let title = title_tpl
            .replace("{project}", &build.project)
            .replace("{number}", &build.number.to_string())
            .replace("{status}", status_word);

        let short_commit = build
            .commit
            .as_deref()
            .map(|c| c.chars().take(7).collect::<String>())
            .unwrap_or_else(|| "?".into());

        let mut desc = String::new();
        if let Some(prefix) = &target.custom_message {
            desc.push_str(prefix);
            if !desc.ends_with('\n') {
                desc.push('\n');
            }
            desc.push('\n');
        }

        // Built commit, linked to kei's own commit view. Kei serves these
        // from the local workspace, so they work for private repos that a
        // GitHub link couldn't reach without auth.
        match (public_url, build.commit.as_deref()) {
            (Some(base), Some(commit)) => desc.push_str(&format!(
                "**Commit:** [`{short_commit}`]({base}/commits/{project}/{commit})",
                project = project.name,
            )),
            _ => desc.push_str(&format!("**Commit:** `{short_commit}`")),
        }

        // Compare link to the previous successful build's commit.
        if target.include_changes {
            if let (Some(base), Some(from), Some(to)) = (
                public_url,
                build.previous_commit.as_deref(),
                build.commit.as_deref(),
            ) {
                if from != to {
                    let from_short: String = from.chars().take(7).collect();
                    desc.push_str(&format!(
                        "\n**Changes:** [{from_short}…{short_commit}]({base}/compare/{project}/{from}...{to})",
                        project = project.name,
                    ));
                }
            }
        }

        // Docs commit pushed by a build step (e.g. update-docs).
        if target.include_docs_commit {
            if let Some(docs) = build.docs_commit.as_deref() {
                let docs_short: String = docs.chars().take(7).collect();
                match public_url {
                    Some(base) => desc.push_str(&format!(
                        "\n**Docs:** [`{docs_short}`]({base}/commits/{project}/{docs})",
                        project = project.name,
                    )),
                    None => desc.push_str(&format!("\n**Docs:** `{docs_short}`")),
                }
            }
        }

        if let Some(err) = &build.error {
            desc.push_str(&format!("\n\n**Error:** {err}"));
        }

        if target.include_artifacts && !build.artifacts.is_empty() {
            desc.push_str("\n\n**Artifacts**");
            for a in &build.artifacts {
                let size_mb = a.size as f64 / 1_048_576.0;
                match (public_url, public_link_secret, target.public_artifact_links) {
                    (Some(base), Some(secret), true) => {
                        let url = crate::auth::public_artifact_url(base, secret, &a.path);
                        desc.push_str(&format!(
                            "\n• [{}]({url}) ({:.2} MB)",
                            basename(&a.path),
                            size_mb
                        ));
                    }
                    (Some(base), _, _) => desc.push_str(&format!(
                        "\n• [{}]({}{}) ({:.2} MB)",
                        basename(&a.path),
                        base,
                        a.url,
                        size_mb
                    )),
                    (None, _, _) => {
                        desc.push_str(&format!("\n• {} ({:.2} MB)", basename(&a.path), size_mb))
                    }
                }
            }
        }

        let mut embed = json!({
            "title": title,
            "description": desc,
            "color": color,
            "timestamp": build.finished_at.unwrap_or(build.started_at).to_rfc3339(),
        });
        if let Some(base) = public_url {
            embed["url"] = Value::String(format!("{base}/builds/{}", build.id));
        }

        json!({ "embeds": [embed] })
    }

    fn basename(path: &str) -> &str {
        path.rsplit_once('/').map(|(_, b)| b).unwrap_or(path)
    }
}
