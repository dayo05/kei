use crate::config::ProjectConfig;
use crate::state::BuildStatus;

pub enum DiscordEvent {
    Queued,
    Started,
    Finished,
}

/// Notify the project's Discord targets about a build event. No-op when the
/// `discord` cargo feature is disabled.
pub async fn discord_notify(
    project: &ProjectConfig,
    public_url: Option<&str>,
    public_link_secret: Option<&str>,
    build: &BuildStatus,
    event: DiscordEvent,
) {
    #[cfg(feature = "discord")]
    discord::send(project, public_url, public_link_secret, build, event).await;

    #[cfg(not(feature = "discord"))]
    {
        let _ = (project, public_url, public_link_secret, build, event);
    }
}

#[cfg(feature = "discord")]
mod discord {
    use super::{BuildStatus, DiscordEvent, ProjectConfig};
    use crate::config::DiscordTarget;
    use crate::state::BuildState;
    use serde_json::{Value, json};
    use tracing::{info, warn};

    pub async fn send(
        project: &ProjectConfig,
        public_url: Option<&str>,
        public_link_secret: Option<&str>,
        build: &BuildStatus,
        event: DiscordEvent,
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
            let payload = build_payload(
                project,
                target,
                public_url,
                public_link_secret,
                build,
                &event,
            );
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
        event: &DiscordEvent,
    ) -> Value {
        let (status_word, color) = match build.state {
            BuildState::Success => ("succeeded", 0x3FB950),
            BuildState::Failed => ("failed", 0xF85149),
            BuildState::Canceled => ("canceled", 0xD29922),
            BuildState::Queued | BuildState::Running => ("finished", 0x6CB6FF),
        };
        let (event_word, event_color, default_title) = match event {
            DiscordEvent::Queued => ("pending", 0xD29922, "{project} #{number} pending"),
            DiscordEvent::Started => ("started", 0x6CB6FF, "{project} #{number} started"),
            DiscordEvent::Finished => (status_word, color, "{project} #{number} {status}"),
        };
        let title_tpl = target
            .title
            .clone()
            .unwrap_or_else(|| default_title.to_string());
        let title = title_tpl
            .replace("{project}", &build.project)
            .replace("{number}", &build.number.to_string())
            .replace("{status}", event_word);

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

        if let Some(url) = build_url(public_url, public_link_secret, build) {
            let label = match event {
                DiscordEvent::Queued | DiscordEvent::Started => "Build",
                DiscordEvent::Finished => "Result",
            };
            desc.push_str(&format!("**{label}:** [Open build]({url})\n"));
        }

        match (public_url, build.commit.as_deref()) {
            (Some(base), Some(commit)) => desc.push_str(&format!(
                "**Commit:** [`{short_commit}`]({})",
                path_url(
                    base,
                    public_link_secret,
                    &format!("/commits/{project}/{commit}", project = project.name),
                )
            )),
            _ => desc.push_str(&format!("**Commit:** `{short_commit}`")),
        }

        match event {
            DiscordEvent::Queued => {
                desc.push_str("\n**State:** waiting for another build to finish");
                return event_payload(
                    title,
                    desc,
                    event_color,
                    build.started_at,
                    public_url,
                    public_link_secret,
                    build,
                );
            }
            DiscordEvent::Started => {
                desc.push_str("\n**State:** build is running");
                return event_payload(
                    title,
                    desc,
                    event_color,
                    build.started_at,
                    public_url,
                    public_link_secret,
                    build,
                );
            }
            DiscordEvent::Finished => {}
        }

        if target.include_changes
            && let (Some(base), Some(from), Some(to)) = (
                public_url,
                build.previous_commit.as_deref(),
                build.commit.as_deref(),
            )
            && from != to
        {
            let from_short: String = from.chars().take(7).collect();
            desc.push_str(&format!(
                "\n**Changes:** [{from_short}…{short_commit}]({})",
                path_url(
                    base,
                    public_link_secret,
                    &format!("/compare/{project}/{from}...{to}", project = project.name),
                )
            ));
        }

        if target.include_docs_commit
            && let Some(docs) = build.docs_commit.as_deref()
        {
            let docs_short: String = docs.chars().take(7).collect();
            match public_url {
                Some(base) => desc.push_str(&format!(
                    "\n**Docs:** [`{docs_short}`]({})",
                    path_url(
                        base,
                        public_link_secret,
                        &format!("/commits/{project}/{docs}", project = project.name),
                    )
                )),
                None => desc.push_str(&format!("\n**Docs:** `{docs_short}`")),
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
                    (Some(base), secret, _) => desc.push_str(&format!(
                        "\n• [{}]({}) ({:.2} MB)",
                        basename(&a.path),
                        path_url(base, secret, &a.url),
                        size_mb
                    )),
                    (None, _, _) => {
                        desc.push_str(&format!("\n• {} ({:.2} MB)", basename(&a.path), size_mb))
                    }
                }
            }
        }

        event_payload(
            title,
            desc,
            color,
            build.finished_at.unwrap_or(build.started_at),
            public_url,
            public_link_secret,
            build,
        )
    }

    fn event_payload(
        title: String,
        description: String,
        color: u32,
        timestamp: chrono::DateTime<chrono::Utc>,
        public_url: Option<&str>,
        public_link_secret: Option<&str>,
        build: &BuildStatus,
    ) -> Value {
        let mut embed = json!({
            "title": title,
            "description": description,
            "color": color,
            "timestamp": timestamp.to_rfc3339(),
        });
        if let Some(url) = build_url(public_url, public_link_secret, build) {
            embed["url"] = Value::String(url);
        }
        json!({ "embeds": [embed] })
    }

    fn build_url(
        public_url: Option<&str>,
        public_link_secret: Option<&str>,
        build: &BuildStatus,
    ) -> Option<String> {
        let base = public_url?;
        Some(match public_link_secret {
            Some(secret) => crate::auth::public_build_url(base, secret, build.id),
            None => format!("{base}/builds/{}", build.id),
        })
    }

    fn path_url(base: &str, secret: Option<&str>, path: &str) -> String {
        match secret {
            Some(secret) => crate::auth::public_path_url(base, secret, path),
            None => format!("{base}{path}"),
        }
    }

    fn basename(path: &str) -> &str {
        path.rsplit_once('/').map(|(_, b)| b).unwrap_or(path)
    }
}
