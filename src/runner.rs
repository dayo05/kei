use anyhow::{Context, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::UnboundedSender;

use crate::config::{NixConfig, ProjectNixOverride, StepConfig};

pub struct StepOutcome {
    pub success: bool,
    pub code: Option<i32>,
}

/// Run a configured step inside the workspace. When nix is enabled, the
/// command is wrapped as `nix develop <flake>#<shell> [extra_args...] -c
/// <command> <args...>` so steps execute inside the project's flake-defined
/// dev shell. Per-step (`use_nix`, `nix_shell`) and per-project
/// (`[projects.nix]`) overrides take precedence over the global `[nix]`
/// settings. If `nix.command` is non-empty it replaces the wrapper entirely.
///
/// Output is streamed line-by-line through `log_sink` as it is produced —
/// the build state's log updates live instead of only on step exit.
pub async fn run_step(
    step: &StepConfig,
    workspace: &Path,
    nix: &NixConfig,
    project_nix: &ProjectNixOverride,
    log_sink: UnboundedSender<String>,
) -> Result<StepOutcome> {
    let cwd = match &step.cwd {
        Some(p) if p.is_absolute() => p.clone(),
        Some(p) => workspace.join(p),
        None => workspace.to_path_buf(),
    };

    let enabled = step
        .use_nix
        .or(project_nix.enabled)
        .unwrap_or(nix.enabled);

    let (program, args) = if enabled {
        let wrapper = build_nix_wrapper(nix, project_nix, step);
        let mut wrapper = wrapper;
        let program = wrapper.remove(0);
        let mut args = wrapper;
        args.push(step.command.clone());
        args.extend(step.args.iter().cloned());
        (program, args)
    } else {
        (step.command.clone(), step.args.clone())
    };

    let env_preview: String = step
        .env
        .iter()
        .map(|(k, v)| format!("{k}={v} "))
        .collect();
    let _ = log_sink.send(format!(
        "$ {env_preview}{program} {}\n",
        args.join(" ")
    ));

    let mut cmd = Command::new(&program);
    cmd.args(&args)
        .current_dir(&cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in &step.env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn {program}"))?;

    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");

    let tx_out = log_sink.clone();
    let tx_err = log_sink;

    let h_out = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = tx_out.send(format!("{line}\n"));
        }
    });
    let h_err = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = tx_err.send(format!("{line}\n"));
        }
    });

    let status = child.wait().await?;
    let _ = h_out.await;
    let _ = h_err.await;

    Ok(StepOutcome {
        success: status.success(),
        code: status.code(),
    })
}

fn build_nix_wrapper(
    nix: &NixConfig,
    project_nix: &ProjectNixOverride,
    step: &StepConfig,
) -> Vec<String> {
    if !nix.command.is_empty() {
        return nix.command.clone();
    }
    let flake = project_nix.flake.as_deref().unwrap_or(&nix.flake);
    let shell = step
        .nix_shell
        .as_deref()
        .or(project_nix.shell.as_deref())
        .unwrap_or(&nix.shell);
    let extra: &[String] = project_nix
        .extra_args
        .as_deref()
        .unwrap_or(nix.extra_args.as_slice());
    let mut argv: Vec<String> = vec![
        "nix".into(),
        "develop".into(),
        format!("{flake}#{shell}"),
    ];
    argv.extend(extra.iter().cloned());
    argv.push("-c".into());
    argv
}
