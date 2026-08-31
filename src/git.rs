use anyhow::Result;
use std::path::Path;
use tokio::process::Command;

/// Sync a repo at `dir` to the tip of `branch` on `origin`, force-overwriting
/// any local divergence (handles upstream force-pushes). `ssh_key`, when set,
/// is the private key to authenticate with (see [`apply_ssh`]). Returns the
/// full command log alongside the result so callers can persist it even on
/// failure.
pub async fn sync(
    repo_url: &str,
    branch: &str,
    dir: &Path,
    ssh_key: Option<&Path>,
) -> (String, Result<String>) {
    let mut log = String::new();
    let r = sync_inner(repo_url, branch, dir, ssh_key, &mut log).await;
    (log, r)
}

/// Point git at a specific private key for this invocation. `IdentitiesOnly`
/// stops ssh from offering the agent's keys (or `~/.ssh/id_*`) first — without
/// it, a host that accepts several of our keys can authenticate as the wrong
/// identity and fail authorization on the repo.
fn apply_ssh(cmd: &mut Command, ssh_key: Option<&Path>) {
    if let Some(key) = ssh_key {
        // git shell-parses GIT_SSH_COMMAND, so a path with spaces needs
        // quoting; single quotes plus the standard '\'' escape cover any path.
        let quoted = key.to_string_lossy().replace('\'', r"'\''");
        cmd.env(
            "GIT_SSH_COMMAND",
            format!("ssh -i '{quoted}' -o IdentitiesOnly=yes"),
        );
    }
}

async fn sync_inner(
    repo_url: &str,
    branch: &str,
    dir: &Path,
    ssh_key: Option<&Path>,
    log: &mut String,
) -> Result<String> {
    let exists = dir.join(".git").is_dir();
    let dir_str = dir
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-utf8 path: {}", dir.display()))?;

    if !exists {
        if let Some(parent) = dir.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        run(
            log,
            &[
                "clone",
                "--branch",
                branch,
                "--recurse-submodules",
                repo_url,
                dir_str,
            ],
            None,
            ssh_key,
        )
        .await?;
    } else {
        // Sync remote URL in case it changed in config; ignore failure.
        let _ = run(
            log,
            &["remote", "set-url", "origin", repo_url],
            Some(dir),
            ssh_key,
        )
        .await;
        run(
            log,
            &["fetch", "--prune", "--force", "origin"],
            Some(dir),
            ssh_key,
        )
        .await?;
        let target = format!("origin/{branch}");
        run(
            log,
            &["checkout", "-B", branch, &target],
            Some(dir),
            ssh_key,
        )
        .await?;
        run(log, &["reset", "--hard", &target], Some(dir), ssh_key).await?;
        run(log, &["clean", "-fdx"], Some(dir), ssh_key).await?;
        // Pick up any submodule URL changes (e.g. upstream renamed a remote).
        run(
            log,
            &["submodule", "sync", "--recursive"],
            Some(dir),
            ssh_key,
        )
        .await?;
        // Initialize new, update existing, force-overwrite divergent ones.
        // No-op if the repo has no submodules.
        run(
            log,
            &["submodule", "update", "--init", "--recursive", "--force"],
            Some(dir),
            ssh_key,
        )
        .await?;
    }

    let head = run(log, &["rev-parse", "HEAD"], Some(dir), ssh_key).await?;
    Ok(head.trim().to_string())
}

/// Single commit's metadata + diff stat, sourced from the project workspace.
/// Kei exposes this via `/commits/:project/:sha` so notifications can link
/// to commit info without depending on the remote (which may be private).
#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub sha: String,
    pub author: String,
    pub date: String,
    pub subject: String,
    pub body: String,
    pub stat: String,
    /// Full unified diff (`git show --format= <sha>`). Rendered by the
    /// commit view with line-level +/- coloring.
    pub diff: String,
}

#[derive(Debug, Clone)]
pub struct CommitSummary {
    pub sha: String,
    pub author: String,
    pub date: String,
    pub subject: String,
}

pub async fn show_commit(dir: &Path, sha: &str) -> Result<CommitInfo> {
    let dir_arg = dir
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-utf8 path: {}", dir.display()))?;
    // ASCII-only field separator — chosen so commit subjects/bodies (which
    // can contain newlines, tabs, anything) don't collide with the split.
    let sep = "\x1e";
    let format = format!("%H{sep}%an <%ae>{sep}%aI{sep}%s{sep}%B");
    let out = Command::new("git")
        .args([
            "-C",
            dir_arg,
            "show",
            "-s",
            &format!("--format={format}"),
            sha,
        ])
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "git show -s {sha} failed (exit {:?}): {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let parts: Vec<&str> = raw.splitn(5, sep).collect();
    if parts.len() < 5 {
        anyhow::bail!("git show output malformed for {sha}");
    }

    // Diff stat is separate — `git show -s` skips the diff entirely.
    let stat_out = Command::new("git")
        .args(["-C", dir_arg, "show", "--stat", "--format=", sha])
        .output()
        .await?;
    let stat = if stat_out.status.success() {
        String::from_utf8_lossy(&stat_out.stdout).trim().to_string()
    } else {
        String::new()
    };

    // Full unified diff for the commit view's GitHub-style rendering.
    let diff_out = Command::new("git")
        .args(["-C", dir_arg, "show", "--format=", sha])
        .output()
        .await?;
    let diff = if diff_out.status.success() {
        String::from_utf8_lossy(&diff_out.stdout).into_owned()
    } else {
        String::new()
    };

    Ok(CommitInfo {
        sha: parts[0].trim().to_string(),
        author: parts[1].trim().to_string(),
        date: parts[2].trim().to_string(),
        subject: parts[3].trim().to_string(),
        body: parts[4].trim_end().to_string(),
        stat,
        diff,
    })
}

pub async fn log_range(dir: &Path, from: &str, to: &str) -> Result<Vec<CommitSummary>> {
    let dir_arg = dir
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-utf8 path: {}", dir.display()))?;
    let range = format!("{from}..{to}");
    // Tab-separated; subjects are reasonably safe (no tabs in commit subjects).
    let out = Command::new("git")
        .args([
            "-C",
            dir_arg,
            "log",
            "--format=%H%x09%an <%ae>%x09%aI%x09%s",
            &range,
        ])
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "git log {range} failed (exit {:?}): {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let mut out_vec = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let parts: Vec<&str> = line.splitn(4, '\t').collect();
        if parts.len() == 4 {
            out_vec.push(CommitSummary {
                sha: parts[0].to_string(),
                author: parts[1].to_string(),
                date: parts[2].to_string(),
                subject: parts[3].to_string(),
            });
        }
    }
    Ok(out_vec)
}

/// Read the current HEAD commit in `dir`. Used after build steps to detect
/// commits created by the build itself (e.g. an update-docs step that pushes
/// back) without parsing step logs.
pub async fn current_head(dir: &Path) -> Result<String> {
    let dir_arg = dir
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-utf8 path: {}", dir.display()))?;
    let out = Command::new("git")
        .args(["-C", dir_arg, "rev-parse", "HEAD"])
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "git rev-parse HEAD failed (exit {:?}): {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Cheap remote-state probe: `git ls-remote <repo> refs/heads/<branch>` returns
/// the commit hash currently at the tip of <branch> on origin without touching
/// any local workspace. Used by the startup auto-trigger to compare against
/// `.last_commit`.
pub async fn remote_head(repo_url: &str, branch: &str, ssh_key: Option<&Path>) -> Result<String> {
    let refspec = format!("refs/heads/{branch}");
    let mut cmd = Command::new("git");
    cmd.args(["ls-remote", repo_url, &refspec]);
    apply_ssh(&mut cmd, ssh_key);
    let out = cmd.output().await?;
    if !out.status.success() {
        anyhow::bail!(
            "git ls-remote {repo_url} failed (exit {:?}): {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let hash = stdout
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("ls-remote returned no refs for {branch} on {repo_url}"))?;
    Ok(hash.to_string())
}

async fn run(
    log: &mut String,
    args: &[&str],
    dir: Option<&Path>,
    ssh_key: Option<&Path>,
) -> Result<String> {
    let mut cmd = Command::new("git");
    if let Some(d) = dir {
        cmd.arg("-C").arg(d);
    }
    cmd.args(args);
    apply_ssh(&mut cmd, ssh_key);
    log.push_str(&format!("$ git {}\n", args.join(" ")));
    let out = cmd.output().await?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    log.push_str(&stdout);
    log.push_str(&String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        anyhow::bail!(
            "git {} failed (exit {:?})",
            args.join(" "),
            out.status.code()
        );
    }
    Ok(stdout)
}
