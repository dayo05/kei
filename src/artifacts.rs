use anyhow::{Context, Result};
use glob::glob;
use std::path::{Path, PathBuf};
use uuid::Uuid;

use crate::config::ArtifactConfig;
use crate::state::ArtifactRef;

/// Glob the workspace for artifact patterns and copy matches into
/// `<artifacts_dir>/<project>/<build_id>/<relative path>`. Returns
/// references suitable for serving via the static file route.
///
/// Suffixes may contain `{build_number}`, which is substituted with the
/// monotonic per-project counter (`number`).
pub async fn collect(
    project: &str,
    build_id: Uuid,
    build_number: u64,
    workspace: &Path,
    artifacts_dir: &Path,
    patterns: &[ArtifactConfig],
) -> Result<Vec<ArtifactRef>> {
    let workspace_buf = workspace.to_path_buf();
    let workspace_canon = tokio::task::spawn_blocking(move || {
        std::fs::canonicalize(&workspace_buf)
    })
    .await?
    .context("canonicalize workspace")?;

    let dest_root = artifacts_dir.join(project).join(build_id.to_string());
    tokio::fs::create_dir_all(&dest_root)
        .await
        .context("create artifact directory")?;

    let mut found = Vec::new();
    for ac in patterns {
        let pattern = ac.pattern.clone();
        let workspace = workspace_canon.clone();
        let matches = tokio::task::spawn_blocking(move || -> Result<Vec<PathBuf>> {
            let abs_pattern: String = if Path::new(&pattern).is_absolute() {
                pattern
            } else {
                workspace.join(&pattern).to_string_lossy().into_owned()
            };
            let mut v = Vec::new();
            for entry in glob(&abs_pattern).context("invalid glob pattern")? {
                let path = entry?;
                if path.is_file() {
                    v.push(path);
                }
            }
            Ok(v)
        })
        .await??;

        for src in matches {
            let rel = if let Some(suffix_tpl) = ac.suffix.as_deref() {
                // Suffix mode: flatten to dest root. An empty `suffix = ""`
                // flattens without renaming (useful when the source filename
                // already encodes everything you need). Otherwise the suffix
                // is spliced before the extension, e.g.
                //   aris-1.1.0.jar + "fabric" -> aris-1.1.0-fabric.jar
                let suffix = suffix_tpl.replace("{build_number}", &build_number.to_string());
                let basename = src
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("artifact");
                let new_name = if suffix.is_empty() {
                    basename.to_string()
                } else {
                    let (stem, ext) = match basename.rsplit_once('.') {
                        Some((s, e)) if !s.is_empty() => (s, Some(e)),
                        _ => (basename, None),
                    };
                    match ext {
                        Some(e) => format!("{stem}-{suffix}.{e}"),
                        None => format!("{stem}-{suffix}"),
                    }
                };
                PathBuf::from(new_name)
            } else {
                match src.strip_prefix(&workspace_canon) {
                    Ok(r) => r.to_path_buf(),
                    Err(_) => src
                        .file_name()
                        .map(PathBuf::from)
                        .unwrap_or_else(|| PathBuf::from("artifact")),
                }
            };
            let dest = dest_root.join(&rel);
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::copy(&src, &dest).await.with_context(|| {
                format!("copy {} -> {}", src.display(), dest.display())
            })?;
            let size = tokio::fs::metadata(&dest).await?.len();
            let path_str = format!("{}/{}/{}", project, build_id, rel.to_string_lossy());
            let url = format!("/artifacts/{}", path_str);
            found.push(ArtifactRef {
                project: project.to_string(),
                build_id,
                path: path_str,
                size,
                url,
            });
        }
    }
    Ok(found)
}
