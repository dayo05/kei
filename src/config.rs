use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub github: GithubConfig,
    #[serde(default)]
    pub nix: NixConfig,
    /// Optional Maven repository serving. Only read when the `maven` cargo
    /// feature is enabled; harmless to configure in slim builds.
    #[cfg_attr(not(feature = "maven"), allow(dead_code))]
    #[serde(default)]
    pub maven: MavenConfig,
    #[serde(default)]
    pub projects: Vec<ProjectConfig>,
}

#[cfg_attr(not(feature = "maven"), allow(dead_code))]
#[derive(Debug, Deserialize, Clone, Default)]
pub struct MavenConfig {
    /// Directory served at `/maven/...` as a Maven repository. Projects
    /// publish here via `./gradlew publishToMavenLocal` with
    /// `-Dmaven.repo.local=$KEI_MAVEN_REPO` (kei exports that env var to
    /// each build step). When unset, the /maven route returns 404.
    #[serde(default)]
    pub repo_dir: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Externally-reachable base URL (no trailing slash), used when kei needs
    /// to construct absolute links (e.g. Discord notifications). Optional —
    /// if unset, notifications include relative URLs and the build page is
    /// referenced by id only.
    #[serde(default)]
    pub public_url: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            public_url: None,
        }
    }
}

fn default_host() -> String { "0.0.0.0".into() }
fn default_port() -> u16 { 5050 }

#[derive(Debug, Deserialize, Clone)]
pub struct StorageConfig {
    #[serde(default = "default_workspace")]
    pub workspace_dir: PathBuf,
    #[serde(default = "default_artifacts")]
    pub artifacts_dir: PathBuf,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            workspace_dir: default_workspace(),
            artifacts_dir: default_artifacts(),
        }
    }
}

fn default_workspace() -> PathBuf { PathBuf::from("./data/workspaces") }
fn default_artifacts() -> PathBuf { PathBuf::from("./data/artifacts") }

#[derive(Debug, Deserialize, Clone, Default)]
pub struct GithubConfig {
    #[serde(default)]
    pub webhook_secret: Option<String>,
}

/// First-class nix integration. Steps run as `nix develop ${flake}#${shell} -c
/// <step.command> <step.args...>` when enabled. Use `command` to fully override
/// the wrapper (e.g. `["nix-shell", "--run"]` for non-flake projects).
#[derive(Debug, Deserialize, Clone)]
pub struct NixConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_flake")]
    pub flake: String,
    #[serde(default = "default_shell")]
    pub shell: String,
    #[serde(default)]
    pub extra_args: Vec<String>,
    #[serde(default)]
    pub command: Vec<String>,
}

impl Default for NixConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            flake: default_flake(),
            shell: default_shell(),
            extra_args: vec![],
            command: vec![],
        }
    }
}

fn default_flake() -> String { ".".into() }
fn default_shell() -> String { "default".into() }

/// Per-project overrides for nix settings. Each `Some(_)` field replaces the
/// global `[nix]` value for steps in this project.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct ProjectNixOverride {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub flake: Option<String>,
    #[serde(default)]
    pub shell: Option<String>,
    #[serde(default)]
    pub extra_args: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ProjectConfig {
    pub name: String,
    pub repo_url: String,
    #[serde(default = "default_branch")]
    pub branch: String,
    #[serde(default)]
    pub github_full_name: Option<String>,
    #[serde(default)]
    pub nix: ProjectNixOverride,
    #[serde(default)]
    pub steps: Vec<StepConfig>,
    #[serde(default)]
    pub artifacts: Vec<ArtifactConfig>,
    /// Only read when the `discord` cargo feature is enabled.
    #[cfg_attr(not(feature = "discord"), allow(dead_code))]
    #[serde(default)]
    pub notify: NotifyConfig,
}

/// Per-project notification targets. Currently Discord webhooks only.
#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone, Default)]
pub struct NotifyConfig {
    #[serde(default)]
    pub discord: Vec<DiscordTarget>,
}

// Fields are only read when the `discord` cargo feature is enabled; suppress
// dead-code warnings on slim builds.
#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct DiscordTarget {
    /// Full webhook URL from the Discord channel/integration settings.
    pub url: String,
    /// Optional forum/thread id — appended as `?thread_id=...` so the
    /// notification lands in a specific thread instead of the channel root.
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Whether to also notify on failed builds. Default false — only
    /// successes ping the channel.
    #[serde(default)]
    pub on_failure: bool,
    /// Include a "Changes" link comparing the previous successful commit to
    /// the one just built (requires `github_full_name` on the project).
    #[serde(default = "default_true")]
    pub include_changes: bool,
    /// Include a "Docs" link to the commit a build step pushed back to the
    /// repo (e.g. the update-docs flow).
    #[serde(default = "default_true")]
    pub include_docs_commit: bool,
    /// Include the artifact list with download links.
    #[serde(default = "default_true")]
    pub include_artifacts: bool,
    /// Optional title template. Variables: `{project}`, `{number}`,
    /// `{status}` (succeeded|failed). Default:
    /// `"{project} #{number} {status}"`.
    #[serde(default)]
    pub title: Option<String>,
    /// Optional raw markdown to prepend to the embed description (e.g. role
    /// mentions, custom blurb). Substitutions are NOT applied.
    #[serde(default)]
    pub custom_message: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_branch() -> String { "main".into() }

#[derive(Debug, Deserialize, Clone)]
pub struct StepConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub use_nix: Option<bool>,
    #[serde(default)]
    pub nix_shell: Option<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Extra env vars applied just for this step on top of the inherited
    /// service environment. Useful for isolating per-step caches (e.g. a
    /// separate `GRADLE_USER_HOME` so a pre-build step's daemons don't
    /// conflict with the main build's).
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ArtifactConfig {
    pub pattern: String,
    /// Optional suffix inserted before the file extension when copying.
    /// When set, matched files are also flattened to the artifact root
    /// (so `fabric/build/libs/aris-1.1.0.jar` with `suffix = "fabric"`
    /// becomes `<artifacts>/<project>/<build_id>/aris-1.1.0-fabric.jar`).
    /// When unset, the original relative path under the workspace is kept.
    #[serde(default)]
    pub suffix: Option<String>,
}

/// Per-project build configuration loaded from `<workspace>/kei.toml` after
/// sync. When present, its steps/artifacts/nix override the registration
/// entry in the global kei.toml — so projects can keep their build recipe
/// in-tree alongside the source it builds.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct ProjectBuildConfig {
    #[serde(default)]
    pub nix: ProjectNixOverride,
    #[serde(default)]
    pub steps: Vec<StepConfig>,
    #[serde(default)]
    pub artifacts: Vec<ArtifactConfig>,
}

impl ProjectBuildConfig {
    /// Attempt to load `<workspace>/kei.toml`. Returns Ok(None) if no file
    /// exists; Err only when parsing fails (so build.rs can fall back to
    /// the global config but surface real config errors).
    pub fn load_from_workspace(workspace: &Path) -> anyhow::Result<Option<Self>> {
        let path = workspace.join("kei.toml");
        let s = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(anyhow::anyhow!("reading {}: {e}", path.display())),
        };
        let cfg: ProjectBuildConfig = toml::from_str(&s)
            .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;
        Ok(Some(cfg))
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        let cfg: Config = toml::from_str(&s)?;
        Ok(cfg)
    }

    pub fn project(&self, name: &str) -> Option<&ProjectConfig> {
        self.projects.iter().find(|p| p.name == name)
    }

    pub fn project_for_full_name(&self, full_name: &str) -> Option<&ProjectConfig> {
        self.projects
            .iter()
            .find(|p| p.github_full_name.as_deref() == Some(full_name))
    }
}
