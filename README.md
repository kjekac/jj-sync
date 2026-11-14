# jj-sync

A Rust CLI that publishes unfinished Jujutsu work and fetches it between
allowlisted repositories on paired machines. Requires JJ 0.47.1 or newer.

`cycle` snapshots local working copies, publishes `hosts/<host>/work/<workspace>`
bookmarks, and downloads peer snapshots into isolated staging repositories through read-only SSH.
Explicit `apply` uses native JJ reconciliation, including rewrites, rebases,
abandonment, conflicts and divergence.
Bookmark parked work to keep it published, and use one editing machine per active
change.

## Use

Build with `nix build`, or run `nix run . -- --help`. Configure the tool and its
SSH endpoint as described in [CONFIG.md](CONFIG.md), then schedule
`jj-sync cycle` on each machine.

| Command | Purpose |
| --- | --- |
| `setup` | Create the dedicated sync identity. |
| `public-key` | Print its public key. |
| `pair <peer>` | Exchange sync keys over normal authenticated SSH. |
| `revoke <peer>` | Remove a peer's sync key. |
| `cycle` | Publish local work and stage peer snapshots. |
| `apply --dry-run` | Preview native JJ reconciliation in a disposable copy. |
| `apply` | Reconcile staged updates into the current repository. |
| `pending` | Refresh and summarize staged updates for this repository. |
| `pending --count --path DIR` | Print a cached pending-peer count for a prompt. |
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

Allow **JJ Sync** in Privacy & Security > Local Network from the machine's
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

At a coordinated development handoff, inspect and apply pending updates:

```sh
jj-sync pending
jj-sync apply --dry-run
jj-sync apply
jj status
jj log
```

`pending` and `apply --dry-run` refresh staging automatically. `apply` uses the
snapshots from the dry-run; use `--cached` to work offline. `--verbose` adds the
full operation diff. Background cycles keep history ready for application.
Coordinate the checkpoint with the editing host. Check conflicts, divergence,
and dependent work after applying. The Nushell prompt supplied by nix-config
shows `↓sync:N` when peers have pending snapshots for the current repository.


Nushell integration is installed at `share/nushell/jj-sync-commands.nu`. Load it
with `use /path/to/package/share/nushell/jj-sync-commands.nu *`; nix-config loads
it automatically. The custom commands return records:

```nu
jj-sync pending                              # One row per peer, including incoming revisions
jj-sync status                               # Cached transport status
let preview = (jj-sync apply --dry-run)
$preview.changes | select change_id before after kind description
$preview.workspace                           # Checkout transition and file changes
$preview.evolution                           # JJ predecessor relationships, including splits
jj-sync apply
^jj-sync apply --dry-run                      # Colored terminal graph and comparison
^jj-sync apply --dry-run --verbose            # Also show the native operation diff
```

The terminal comparison pairs versions by change ID and renders the proposed
visible history with JJ. `@` marks the resulting checkout. Apply follows the
current change's rewrite; incoming descendants remain available without moving
the checkout to their tip. Workspace file changes are shown separately from
history changes. Preview uses repository-relative paths.

`pending --json` and `apply --dry-run --json` provide versioned JSON on stdout;
diagnostics go to stderr. Reports contain full revision/change IDs, parent IDs,
paired before/after revision arrays, JJ evolution edges, changed bookmarks and
tags, checkout/file changes, conflicts and divergence. Evolution edges expose relationships JJ has retained, including different change
IDs when available. Git transport can omit cross-change evolution metadata; split
parts still appear with their actual change IDs and parents. A shared change ID
alone does not establish a rewrite. `--color=auto|always|never` controls
native terminal rendering; JSON contains no ANSI escapes. `pending` omits
already-known and proven superseded revisions and reports new tags and removed
bookmarks separately.


`jj-sync watch` is a persistent background process on Linux and macOS. It watches
allowlisted live repositories' operation heads with native filesystem events,
coalesces operations for 200 ms (at most one second under continuous activity),
exports workspace markers/bookmarks/tags, and queues availability notifications.
Ordinary file edits become publishable when JJ snapshots them; periodic cycles
also snapshot working files. `jj-sync publish` forces publication without a fetch.

The `jj-sync-notify <repository> <token>` restricted SSH request only queues an
allowlisted repository ID. The receiving watcher immediately schedules a fetch
from its configured peers into staging, using the same software-key transport as
normal cycles. It cannot apply history or choose a command or destination. Queue
writes are the endpoint's only new write capability; the Linux SSH service can
write only the notification directory inside the otherwise read-only home.

Publication and staging use separate locks and workers, so slow fetching does
not delay local export or notification delivery. Notifications coalesce per
repository and survive restarts; delivery retries every ten seconds. Incoming
hints remain queued while staging is busy or fetching fails. Repeated tokens are
deduplicated. Bookkeeping refs are excluded from publication fingerprints, and
staging repositories are excluded from operation watches. Periodic cycles remain
the fallback for missed filesystem events, missed hints and offline peers.

Run `jj-sync watch --interval 60 --debounce-ms 200` under a supervisor. The interval
controls recovery audits, not notification latency. Nix-config configures
`jj-sync-watch.service` on NixOS and the `jj-sync-watch` LaunchAgent on macOS,
alongside its existing periodic cycle. Activate both hosts to enable the new
notification request. On macOS, Local Network access must cover the native
watcher as well as interactive invocations.
