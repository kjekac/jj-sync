# jj-repo-sync

A Rust CLI that publishes unfinished Jujutsu work and fetches it between
allowlisted repositories on paired machines. Requires JJ 0.47.1 or newer.

`cycle` snapshots local working copies, publishes `hosts/<host>/work/<workspace>`
bookmarks, and fetches peers through read-only SSH. JJ handles rewrites, rebases,
abandonment, and divergence. Fetching can change ancestry and working files.
Bookmark parked work to keep it published, and use one editing machine per active
change.

## Use

Build with `nix build`, or run `nix run . -- --help`. Configure the tool and its
SSH endpoint as described in [CONFIG.md](CONFIG.md), then schedule
`jj-repo-sync cycle` on each machine.

| Command | Purpose |
| --- | --- |
| `setup` | Create the dedicated sync identity. |
| `public-key` | Print its public key. |
| `pair <peer>` | Exchange sync keys over normal authenticated SSH. |
| `revoke <peer>` | Remove a peer's sync key. |
| `cycle` | Publish local work and fetch peers. |
| `status` | Print the latest saved cycle results. |
| `github` | Back up host bookmarks and tags to an existing GitHub remote. |

The dedicated endpoint permits repository listing and fetching only. Regular SSH
and GitHub use normal user authentication. GitHub pushes are guarded; fetch and
reconcile unexpected remote changes before retrying.

An empty repository allowlist permits no repositories. Missing local repositories
are fetched into the state cache. Offline peers retain their last fetched objects
and success timestamps. `cycle` saves status and exits nonzero if any operation
fails.

## macOS

Allow **JJ Repo Sync** in Privacy & Security > Local Network from the machine's
graphical session. The executable embeds its permission identity and usage
description. Failed cycles stay alive for at least 30 seconds so the permission
alert can appear. Nix ad-hoc signs the executable; permission may need renewed
approval after rebuilding.

Run `nix run .#native-cycle-check` from a native macOS terminal or SSH session to
check scheduled network access. It uses the installed SSH configuration,
temporary state, and an empty repository allowlist, then waits for three successful
scheduled passes. It lists peers without snapshotting or fetching repositories
and removes its temporary agent afterward. Keep the GUI available for approval.

## Development

`nix develop` provides Rust, Cargo, JJ, Git, SSH, and test tools. Nix embeds tool
paths in the executable. Python is used only by tests.

```sh
nix flake check
nix develop -c cargo test
nix develop -c cargo clippy --all-targets -- -D warnings
nix develop -c cargo fmt --check
```

Tests use disposable repositories and isolated configuration, state, and transport.
