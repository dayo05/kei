# Kei — Kickstart Environment Integrator

A small, self-hosted build server in Rust. GitHub pushes a webhook → Kei
force-syncs the repo, runs configured commands (gradle, copy, anything),
collects pattern-matched artifacts, and exposes them over HTTP as static
files. NixOS-friendly: each step can run inside a project's `nix develop`
dev shell.

## Pipeline

For every accepted webhook (or manual trigger):

1. **Sync.** `git fetch --prune --force`, `checkout -B <branch>
   origin/<branch>`, `reset --hard origin/<branch>`, `clean -fdx`. Force
   pushes upstream are honoured — the workspace always becomes a verbatim
   copy of the remote tip.
2. **Build.** Run each `[[projects.steps]]` in order. Steps are arbitrary
   commands (`./gradlew build`, `cp -r ...`, `make`, ...). When nix is
   enabled they're wrapped as `nix develop <flake>#<shell> -c <cmd> <args...>`.
3. **Collect.** Glob the workspace with each `[[projects.artifacts]]`
   pattern; copy matches to `<artifacts_dir>/<project>/<build_id>/...`.
4. **Serve.** Artifacts are immediately downloadable via the static
   `/artifacts` route. A `latest` symlink per project is updated on success.

## Quick start

```bash
cargo build --release
cp kei.toml.example kei.toml      # then edit
./target/release/kei              # picks up ./kei.toml (or $KEI_CONFIG)
```

Logs go to stderr (`KEI_LOG=info,kei=debug` for verbose). Workspaces and
artifacts default to `./data/{workspaces,artifacts}` — override under
`[storage]`.

## Configuration

Full schema with comments lives in [`kei.toml.example`](./kei.toml.example).
The minimum to get a project building:

```toml
[server]
port = 5050

[github]
webhook_secret = "..."          # shared secret from the GitHub webhook UI

[[projects]]
name = "demo"
repo_url = "https://github.com/owner/repo.git"
branch = "main"
github_full_name = "owner/repo" # webhook routing key

[[projects.steps]]
name = "build"
command = "./gradlew"
args = ["clean", "build"]

[[projects.artifacts]]
pattern = "build/libs/*.jar"
```

Nix integration (devShell selection, per-project/per-step overrides,
custom wrapper, NixOS systemd module) is documented in [`nix.md`](./nix.md).

## HTTP API

| Method | Path                          | Purpose                                              |
|--------|-------------------------------|------------------------------------------------------|
| GET    | `/health`                     | Liveness probe                                       |
| POST   | `/webhook`                    | GitHub webhook entry (HMAC-SHA256 verified)          |
| POST   | `/api/builds/trigger`         | Manual trigger: `{"project":"demo"}`                 |
| GET    | `/api/projects`               | Configured project names                             |
| GET    | `/api/builds`                 | All builds, newest first                             |
| GET    | `/api/builds/:id`             | One build (status, current step, log, artifacts)     |
| GET    | `/api/artifacts`              | Flat artifact list with download URLs                |
| GET    | `/artifacts/<project>/<id>/…` | Static download (also `…/<project>/latest/…`)        |
| GET    | `/public/artifacts/<project>/<id>/…?token=…` | Signed public artifact download |

Build state is one of `queued | running | success | failed`. The full
captured stdout/stderr of every step is exposed in `BuildStatus.log`.

## Access control

Projects are public by default. Set `visibility = "restricted"` and
`allowed_accounts = ["name"]` on a project to require
`Authorization: Bearer <token>` for that project's builds, logs, artifacts,
commit view, and compare view. Browser users can also visit `/login`, enter
their account name and token, and Kei will store the token in an HttpOnly
cookie. `visibility = "private"` restricts a project to admin accounts only.

Discord artifact links can still be public for restricted projects by setting
`public_artifact_links = true` on the Discord target and configuring
`[auth].public_link_secret`. Those links use signed `/public/artifacts/...`
URLs and only serve the exact artifact file.

Maven artifacts are owned by project config via `maven.artifacts = [...]` and
are public by default. Set `maven.public = false` on a project to require the
same bearer token for its Maven artifact IDs.

### GitHub webhook setup

In the repo's webhook settings:

- **Payload URL** — `https://your-host/webhook`
- **Content type** — `application/json`
- **Secret** — same value as `[github].webhook_secret`
- **Events** — at least *Pushes* (others are accepted but ignored).

Kei validates `X-Hub-Signature-256` in constant time and rejects mismatches
with `401`. `ping` events return `pong`.

## Concurrency

Builds for the same project are serialized through a per-project mutex
(workspace can't be touched concurrently). Different projects build in
parallel.

## Layout

```
src/
  main.rs        # axum server, routes
  config.rs      # TOML schema (server/storage/github/nix/projects)
  state.rs       # in-memory build registry + per-project locks
  webhook.rs     # GitHub signature verify + dispatch
  routes.rs      # /api/* JSON endpoints
  build.rs       # orchestrates sync → steps → artifacts
  git.rs         # force-sync logic
  runner.rs      # step execution; nix wrapper resolution
  artifacts.rs   # glob matching + copy
  error.rs       # ApiError → JSON response
kei.toml.example
nix.md
```

## Environment

| Variable      | Purpose                                          |
|---------------|--------------------------------------------------|
| `KEI_CONFIG`  | Path to config (default: `./kei.toml`)           |
| `KEI_LOG`     | `tracing` filter (default: `info,kei=debug`)     |

## Notes

- Build state is in-memory; restarting Kei drops history but keeps artifacts
  on disk (the `/artifacts` route still serves them).
- Kei runs whatever the configured steps say. Run it as a dedicated user on
  a host you're willing to treat as a CI runner.
- `git` must be on `$PATH`. SSH clone URLs additionally need `ssh` and a
  pre-seeded `known_hosts` for the user Kei runs as.
