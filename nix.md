# Kei × Nix

Kei has first-class Nix integration on two sides:

1. **Target side** — projects Kei builds can declare their toolchain in a
   `flake.nix`. When nix is enabled in `kei.toml`, every step is wrapped in
   `nix develop` and runs inside that flake's dev shell.
2. **Host side** — Kei itself can be packaged as a flake and installed as a
   NixOS systemd service.

## How the wrapper works

When `[nix].enabled = true`, each step is executed as:

```
nix develop <flake>#<shell> <extra_args...> -c <step.command> <step.args...>
```

The wrapper runs with `cwd = <workspace>`, so `flake = "."` (the default)
resolves to the freshly-synced project repo. Resolution order for
`enabled` / `flake` / `shell` / `extra_args`:

| Field        | Override order (highest precedence first)                     |
|--------------|---------------------------------------------------------------|
| `enabled`    | step `use_nix` → project `nix.enabled` → global `nix.enabled` |
| `flake`      | project `nix.flake` → global `nix.flake`                      |
| `shell`      | step `nix_shell` → project `nix.shell` → global `nix.shell`   |
| `extra_args` | project `nix.extra_args` → global `nix.extra_args`            |

For non-flake setups (or fully custom wrappers), set `nix.command` and the
structured fields are ignored — everything in `command` is used verbatim:

```toml
[nix]
enabled = true
command = ["nix-shell", "shell.nix", "--run"]
```

## kei.toml — full nix surface

```toml
[nix]
enabled = true              # default off
flake = "."                 # flake reference; "." = workspace root
shell = "default"           # devShell attribute name
extra_args = []             # e.g. ["--impure", "--accept-flake-config"]
# command = []              # full wrapper override; if non-empty, takes over

[[projects]]
name = "demo"
repo_url = "https://github.com/owner/repo.git"
branch = "main"
github_full_name = "owner/repo"

# Per-project overrides — each field falls back to [nix] if omitted.
nix.shell = "ci"
# nix.flake = "github:owner/shared-toolchain"
# nix.enabled = false
# nix.extra_args = ["--impure"]

[[projects.steps]]
name = "build"
command = "./gradlew"
args = ["build"]
# Step-level knobs:
# use_nix = false           # this step bypasses nix entirely
# nix_shell = "lint"        # use a different devShell for just this step
```

## What the target repo must provide

A flake that exposes the dev shell Kei will enter. A minimal Java example:

```nix
# flake.nix in the project Kei builds
{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs = { self, nixpkgs }:
    let pkgs = nixpkgs.legacyPackages.x86_64-linux; in {
      devShells.x86_64-linux.default = pkgs.mkShell {
        packages = [ pkgs.jdk21 pkgs.gradle pkgs.git ];
      };
    };
}
```

`./gradlew build` then runs as `nix develop .#default -c ./gradlew build` —
gradle and JDK come from the flake, no host pollution.

## Packaging Kei itself

Drop this `flake.nix` at the repo root. It exposes `packages.default`
(the Kei binary) and `nixosModules.default` (a systemd service).

```nix
{
  description = "Kei — Kickstart Environment Integrator";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let pkgs = import nixpkgs { inherit system; }; in {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "kei";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          meta.mainProgram = "kei";
        };

        devShells.default = pkgs.mkShell {
          packages = with pkgs; [ cargo rustc rust-analyzer clippy git ];
        };
      })
    // {
      nixosModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.services.kei;
          tomlFormat = pkgs.formats.toml { };
          configFile = tomlFormat.generate "kei.toml" cfg.settings;
        in {
          options.services.kei = {
            enable = lib.mkEnableOption "Kei build server";

            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.system}.default;
            };

            stateDir = lib.mkOption {
              type = lib.types.str;
              default = "/var/lib/kei";
              description = "Holds workspaces/ and artifacts/.";
            };

            openFirewall = lib.mkOption {
              type = lib.types.bool;
              default = false;
            };

            settings = lib.mkOption {
              type = tomlFormat.type;
              default = { };
              description = "Contents of kei.toml. See kei.toml.example.";
            };
          };

          config = lib.mkIf cfg.enable {
            users.users.kei = {
              isSystemUser = true;
              group = "kei";
              home = cfg.stateDir;
            };
            users.groups.kei = { };

            networking.firewall.allowedTCPPorts =
              lib.mkIf cfg.openFirewall
                [ (cfg.settings.server.port or 5050) ];

            # Non-root user needs to talk to the nix daemon for `nix develop`.
            nix.settings.allowed-users = [ "kei" ];

            systemd.services.kei = {
              description = "Kei — Kickstart Environment Integrator";
              after = [ "network-online.target" ];
              wants = [ "network-online.target" ];
              wantedBy = [ "multi-user.target" ];

              # git: always; openssh: SSH remotes & deploy keys; nix: when enabled.
              path = [ pkgs.git pkgs.openssh config.nix.package ];

              environment = {
                KEI_CONFIG = "${configFile}";
                KEI_LOG = "info,kei=info";
                HOME = cfg.stateDir;
              };

              serviceConfig = {
                User = "kei";
                Group = "kei";
                ExecStart = lib.getExe cfg.package;
                Restart = "on-failure";
                RestartSec = 5;
                StateDirectory = "kei";
                WorkingDirectory = cfg.stateDir;

                NoNewPrivileges = true;
                PrivateTmp = true;
                ProtectHome = true;
                ProtectSystem = "full";
                ReadWritePaths = [ cfg.stateDir ];
              };
            };
          };
        };
    };
}
```

## Consumer flake — adding Kei to a NixOS host

```nix
{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  inputs.kei.url = "github:you/kei";   # or path:/srv/src/kei

  outputs = { self, nixpkgs, kei }: {
    nixosConfigurations.builder = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        kei.nixosModules.default
        ({ ... }: {
          nix.settings.experimental-features = [ "nix-command" "flakes" ];

          services.kei = {
            enable = true;
            openFirewall = true;
            settings = {
              server.port = 5050;
              storage.workspace_dir = "/var/lib/kei/workspaces";
              storage.artifacts_dir = "/var/lib/kei/artifacts";
              github.webhook_secret = "REPLACE_VIA_SOPS";   # see Gotchas
              nix = {
                enabled = true;
                flake = ".";
                shell = "default";
              };
              projects = [{
                name = "demo";
                repo_url = "https://github.com/owner/repo.git";
                branch = "main";
                github_full_name = "owner/repo";
                nix.shell = "ci";
                steps = [
                  { name = "build"; command = "./gradlew"; args = [ "clean" "build" ]; }
                ];
                artifacts = [ { pattern = "build/libs/*.jar"; } ];
              }];
            };
          };
        })
      ];
    };
  };
}
```

Then:

```
nixos-rebuild switch --flake .#builder
systemctl status kei
journalctl -u kei -f
curl http://builder:5050/health
```

## Gotchas

- **Secrets in the store.** `services.kei.settings.github.webhook_secret`
  ends up world-readable in `/nix/store/...-kei.toml`. For real deployments,
  swap the module to read the secret from a path (sops-nix / agenix) and
  splice it in via an `ExecStartPre` that pre-renders the TOML to
  `${RUNTIME_DIRECTORY}/kei.toml`.
- **Flakes must be enabled host-wide.** Set
  `nix.settings.experimental-features = [ "nix-command" "flakes" ]` on the
  host; otherwise `nix develop` fails for the `kei` user.
- **Untrusted user, but with daemon access.** `nix.settings.allowed-users =
  [ "kei" ]` lets the service talk to the daemon for evaluation/builds.
  It does *not* make `kei` a trusted user — Kei still can't override
  substituters or `experimental-features`.
- **Per-build trust.** Kei runs whatever each project's flake declares. Treat
  it like any CI runner — dedicated host or VM.
- **Initial SSH clones.** If a `repo_url` uses SSH, pre-seed
  `${stateDir}/.ssh/known_hosts` (e.g. via `system.activationScripts`) or
  switch to HTTPS clone URLs.
