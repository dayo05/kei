use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, watch};
use tracing::warn;
use uuid::Uuid;

use crate::config::Config;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BuildState {
    Queued,
    Running,
    Success,
    Failed,
    Canceled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildStatus {
    pub id: Uuid,
    pub project: String,
    /// Monotonic per-project build counter. Persisted on disk so it survives
    /// restarts and is available as `{build_number}` in artifact suffixes.
    pub number: u64,
    pub commit: Option<String>,
    /// Commit of the previous successful build for this project, if any.
    /// Captured from `.last_commit` at build start so notifications can
    /// link a compare-url showing what changed.
    pub previous_commit: Option<String>,
    /// Set when a build step creates and pushes a commit (e.g. the
    /// update-docs flow). Detected by comparing post-step HEAD against
    /// the post-sync HEAD.
    pub docs_commit: Option<String>,
    pub state: BuildState,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub current_step: Option<String>,
    pub log: String,
    pub artifacts: Vec<ArtifactRef>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub project: String,
    pub build_id: Uuid,
    pub path: String,
    pub size: u64,
    pub url: String,
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub builds: Arc<RwLock<HashMap<Uuid, BuildStatus>>>,
    pub cancellations: Arc<RwLock<HashMap<Uuid, watch::Sender<bool>>>>,
    /// Serialises every build across the whole server. Different projects
    /// can race on the shared ~/.gradle/caches/fabric-loom/<mc-version>/...
    /// state when they target the same Minecraft version; a global lock is
    /// the cheapest way to make that go away without giving each project
    /// an isolated GRADLE_USER_HOME.
    pub build_lock: Arc<Mutex<()>>,
    /// Serialises read-modify-write of the per-project build_number files.
    counter_lock: Arc<Mutex<()>>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        Self {
            config: Arc::new(config),
            builds: Arc::new(RwLock::new(HashMap::new())),
            cancellations: Arc::new(RwLock::new(HashMap::new())),
            build_lock: Arc::new(Mutex::new(())),
            counter_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Allocate the next build number for `project`, persisting it to
    /// `<artifacts_dir>/<project>/.build_number` so it survives restarts.
    pub async fn next_build_number(&self, project: &str) -> u64 {
        let _guard = self.counter_lock.lock().await;
        let dir = self.config.storage.artifacts_dir.join(project);
        let path = dir.join(".build_number");
        let current: u64 = tokio::fs::read_to_string(&path)
            .await
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let next = current + 1;
        let _ = tokio::fs::create_dir_all(&dir).await;
        let _ = tokio::fs::write(&path, next.to_string()).await;
        next
    }

    pub async fn create_build(&self, project: &str) -> (Uuid, watch::Receiver<bool>) {
        let id = Uuid::new_v4();
        let number = self.next_build_number(project).await;
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let status = BuildStatus {
            id,
            project: project.to_string(),
            number,
            commit: None,
            previous_commit: None,
            docs_commit: None,
            state: BuildState::Queued,
            started_at: Utc::now(),
            finished_at: None,
            current_step: None,
            log: String::new(),
            artifacts: vec![],
            error: None,
        };
        self.builds.write().await.insert(id, status);
        self.cancellations.write().await.insert(id, cancel_tx);
        (id, cancel_rx)
    }

    pub async fn cancel_superseded_builds(&self, project: &str) -> Vec<Uuid> {
        let ids: Vec<Uuid> = self
            .builds
            .read()
            .await
            .values()
            .filter(|b| {
                b.project == project && matches!(b.state, BuildState::Queued | BuildState::Running)
            })
            .map(|b| b.id)
            .collect();

        let cancellations = self.cancellations.read().await;
        for id in &ids {
            if let Some(tx) = cancellations.get(id) {
                let _ = tx.send(true);
            }
        }
        ids
    }

    pub async fn request_cancel_build(&self, id: Uuid, reason: &str) -> bool {
        let cancel_sent = self
            .cancellations
            .read()
            .await
            .get(&id)
            .is_some_and(|tx| tx.send(true).is_ok());

        if cancel_sent {
            let mut builds = self.builds.write().await;
            if let Some(b) = builds.get_mut(&id)
                && matches!(b.state, BuildState::Queued | BuildState::Running)
            {
                b.log.push_str(reason);
                if matches!(b.state, BuildState::Queued) {
                    b.state = BuildState::Canceled;
                    b.finished_at = Some(Utc::now());
                    b.current_step = None;
                }
            }
        }

        cancel_sent
    }

    pub async fn remove_cancellation(&self, id: Uuid) {
        self.cancellations.write().await.remove(&id);
    }

    pub async fn update_build<F: FnOnce(&mut BuildStatus)>(&self, id: Uuid, f: F) {
        if let Some(b) = self.builds.write().await.get_mut(&id) {
            f(b);
        }
    }

    pub async fn append_log(&self, id: Uuid, s: &str) {
        if let Some(b) = self.builds.write().await.get_mut(&id) {
            b.log.push_str(s);
        }
    }

    pub async fn get_build(&self, id: Uuid) -> Option<BuildStatus> {
        self.builds.read().await.get(&id).cloned()
    }

    pub async fn list_builds(&self) -> Vec<BuildStatus> {
        let mut v: Vec<_> = self.builds.read().await.values().cloned().collect();
        v.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        v
    }

    /// Read the last successfully-built commit for `project` from disk. Used
    /// at startup to decide whether the remote has moved while kei was down.
    pub async fn last_built_commit(&self, project: &str) -> Option<String> {
        let path = self
            .config
            .storage
            .artifacts_dir
            .join(project)
            .join(".last_commit");
        tokio::fs::read_to_string(&path)
            .await
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    pub async fn set_last_built_commit(&self, project: &str, commit: &str) {
        let dir = self.config.storage.artifacts_dir.join(project);
        let path = dir.join(".last_commit");
        let _ = tokio::fs::create_dir_all(&dir).await;
        let _ = tokio::fs::write(&path, commit).await;
    }

    /// Write a completed build to `<artifacts>/<project>/<id>/build.json`.
    /// Called from build.rs at the terminal state (success or failure) so the
    /// build is visible after a kei restart.
    pub async fn persist_build(&self, id: Uuid) {
        let Some(b) = self.get_build(id).await else {
            return;
        };
        let dir = self
            .config
            .storage
            .artifacts_dir
            .join(&b.project)
            .join(id.to_string());
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            warn!(error=%e, "persist_build: mkdir failed");
            return;
        }
        let path = dir.join("build.json");
        match serde_json::to_string_pretty(&b) {
            Ok(s) => {
                if let Err(e) = tokio::fs::write(&path, s).await {
                    warn!(error=%e, "persist_build: write failed");
                }
            }
            Err(e) => warn!(error=%e, "persist_build: serialize failed"),
        }
    }

    /// Scan `<artifacts>/*/*/build.json` and load each into the in-memory
    /// build map. Called once at startup so previous-run history is visible
    /// in the builds list/detail pages.
    pub async fn load_persisted_builds(&self) {
        let root = &self.config.storage.artifacts_dir;
        let mut projects = match tokio::fs::read_dir(root).await {
            Ok(r) => r,
            Err(_) => return,
        };
        let mut count = 0usize;
        let mut store = self.builds.write().await;
        while let Ok(Some(proj)) = projects.next_entry().await {
            let proj_meta = match proj.metadata().await {
                Ok(m) => m,
                Err(_) => continue,
            };
            if !proj_meta.is_dir() {
                continue;
            }
            let mut builds = match tokio::fs::read_dir(proj.path()).await {
                Ok(r) => r,
                Err(_) => continue,
            };
            while let Ok(Some(b)) = builds.next_entry().await {
                let name = b.file_name();
                let name_str = name.to_string_lossy();
                // Skip the `latest` symlink and any non-uuid sibling.
                if name_str == "latest" || name_str.starts_with('.') {
                    continue;
                }
                let json = b.path().join("build.json");
                let s = match tokio::fs::read_to_string(&json).await {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                match serde_json::from_str::<BuildStatus>(&s) {
                    Ok(bs) => {
                        store.insert(bs.id, bs);
                        count += 1;
                    }
                    Err(e) => warn!(
                        error=%e,
                        path=%json.display(),
                        "load_persisted_builds: skipping malformed build.json"
                    ),
                }
            }
        }
        if count > 0 {
            tracing::info!(loaded = count, "loaded persisted builds");
        }
    }
}
