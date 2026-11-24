# Configuration

Read configuration from `/etc/jj-sync.json`, or pass `--config <path>`:

```json
{
  "host": "desktop",
  "home": "/home/alice",
  "state": "/home/alice/.local/state/jj-sync",
  "repositoryRoots": {"dev": "/home/alice/dev"},
  "repositories": ["dev/project"],
  "peers": {"laptop": {"address": "192.0.2.2", "sshAlias": "laptop"}},
  "sshConfig": "/etc/jj-sync-ssh.conf",
  "remoteCommand": "/run/current-system/sw/bin/jj-sync"
}
```

Host, peer, and root names use `[a-z][a-z0-9-]*`. Root names and relative repository
paths match between machines; absolute root paths can differ. `repositories` is
an exact allowlist for discovery, serving, incoming catalogs, and GitHub backup.
An absent or empty list permits no repositories. `.jj-sync-ignore` excludes a
subtree. Discovery skips hidden directories, symlinks, `node_modules`, `target`,
and `vendor`.

`sshConfig` selects the dedicated sync credentials and host-key policy.
`peers.<name>.sshAlias` selects the normal SSH destination used for pairing.
`remoteCommand` is the executable invoked on that destination during pairing.
`peers.<name>.address` records the peer's network address; outgoing SSH connections
use peer names from `sshConfig`.

## JJ settings

Configure the scheduled user and interactive shell to use the same JJ version
and these settings:

```toml
[git]
abandon-unreachable-commits = true
record-synthetic-predecessors = true
write-change-id-header = true

[revset-aliases]
"immutable_heads()" = "tags() | remote_tags(remote=glob:sync-*) | present(published@origin)"
```

Peer bookmarks remain untracked. The helper sets
`remotes.sync-<peer>.auto-track-bookmarks = "glob:* ~ glob:*"` and refuses to fetch
when that remote has tracked bookmarks. Workspace markers advertise recorded
commits; JJ handles stale workspaces.

## Explicit application

`cycle` publishes local workspace tips and named bookmarks, and fetches peer
snapshots into `state/staging/<peer>/<repository>`. Incoming updates affect
those isolated repositories. Publication snapshots current working files.

`pending` refreshes staging and lists peers, fetch errors and incoming revisions for
this repository. `pending --cached` inspects downloaded snapshots offline.
`pending --json` provides structured output. `pending --count --path DIR` reads
cached status without refreshing or waiting for the cycle lock, for shell prompts.
`status --json` always reads the saved transport report for all repositories.

Snapshots containing only locally visible commits or proven predecessors of
visible local changes are baselined without importing stale refs. Matching
change IDs alone do not establish that relationship. Independent edits, new
deployment tags and removal of previously accepted bookmarks remain pending.

`apply --dry-run` refreshes staging and prepares native JJ reconciliation in a
disposable copy of local metadata and the Git store. It pairs revisions by change ID, renders the proposed visible graph with JJ,
and reports the checkout transition, file changes, conflicts and divergence.
Incoming descendants remain available; apply follows the current change.
`--json` returns a versioned report with full IDs, parent and evolution
relationships, ref changes and workspace effects. The packaged Nushell module
returns a compact structured summary by default. Nu `--details` (or `--json`)
returns the full report; `^jj-sync` selects the terminal interface. Detailed
revision records include the affected history and immediate context. Nu `pending`
uses flat rows for revisions, tag updates, removed bookmarks and fetch errors.
Summary lists are bounded with explicit overflow counts; full data remains
available through `--details`. `--verbose`
includes the full native operation diff. `--preview` remains an alias for
`--dry-run`. Local working files are snapshotted before preparing the copy.

`apply` consumes the snapshots inspected by the last dry-run. It checks local
operation identity and staged refs, asking for another dry-run if either changed.
Without a saved dry-run, apply refreshes staging first. Use `--cached` on dry-run
or apply to operate offline. Refresh failure stops apply; pending reports the
failure alongside cached results. Dry-run is optional.

Cycles and apply serialize through a helper lock. Native JJ owns conflicts,
abandonment, divergence and handling of concurrent ordinary JJ operations.
Multiple peers apply sequentially, with each success recorded for retry.
Inspect dependent work after splits and use `jj op log` for recovery.

## SSH endpoint

Provision a dedicated SSH daemon that forces `jj-sync serve`, uses
`<state>/authorized_keys`, and prohibits shells, forwarding, PTYs, and SSH startup
scripts. The handler accepts `jj-sync-list` and `git-upload-pack` requests
for allowlisted repositories. Listing reads repository metadata without taking
snapshots; schedule publication on each host.

The dedicated client configuration must pin host keys and use `<state>/identity`
with batch authentication, `IdentityAgent none`, and `IdentitiesOnly yes`.
Regular SSH and GitHub authentication use the user's normal configuration.
Pair with `jj-sync pair <peer>` over normal SSH. The remote pairing command
`pair-receive <peer>` reads a public key from stdin and returns its own public key.

## State and hooks

The state directory contains the sync `identity`, paired keys in `peers/`, the
restricted `authorized_keys`, staging repositories in `staging/<peer>/<repository>`, accepted fingerprints
in `accepted/`, preview records in `previews/`,
and `status.json`. A lock serializes local commands; the read-only server runs
independently.

Status records the latest pass and retains peer and repository success timestamps
across offline failures. `cycle` writes status before returning failure. `status`
prints the saved results regardless of the last cycle's outcome.

Optional `preFetchCommand` and `postFetchCommand` are absolute executable paths.
They run directly with the repository path as their only argument, before and
after a live apply or GitHub fetch. Use them for repository policy such as
retaining deployment tags before remote refs change and validating incoming
targets afterward. Failures stop the operation and are reported; a post-fetch
failure leaves the fetch visible for inspection and does not accept the snapshot.

Dry-run executes both hooks against its isolated repository. Hooks must confine
their effects to the supplied repository. Staging fetches run neither hook and
export exactly the peer's refs; they never manufacture local retention tags.

`github` selects an existing GitHub remote, preferring `origin`. It fetches through
an untracked `sync-github` remote, publishes host workspace and feature bookmarks,
and pushes `hosts/<host>/*` bookmarks and local tags with JJ's guarded push.


## Event-driven publication and refresh

Run `jj-sync watch` persistently alongside periodic cycles. Native filesystem
watching covers live JJ operation heads and durable notification queues, including
repositories with an external/shared JJ repository directory. `--debounce-ms`
defaults to 200; `--interval` defaults to 60 seconds for recovery audits. Source
operations publish without waiting for the staging lock. The daemon sends hints
through the configured paired SSH endpoint and immediately fetches staging on
receipt. It never applies those snapshots.

The restricted server accepts `jj-sync-notify <repository> <token>` for configured
repository IDs only. Tokens are opaque bounded identifiers. The authenticated
request wakes a local worker that fetches configured peers; it contains no command
or network address. Multiple requests coalesce. Failed delivery retries after ten
seconds; failed/busy staging leaves incoming requests queued. Periodic cycles also
recover missed hints. A Linux sandbox must grant write access to
`state/notifications` while retaining read-only access to the repository roots.

`notifications/published/` stores successful publication fingerprints;
`outgoing/`, `incoming/` and `received/` hold bounded per-peer/repository queue and
acknowledgement records. Source bookkeeping and received staging imports do not
trigger another publication. Queue acknowledgement checks for a newer replacement
before removing a record. Watcher configuration changes require a restart;
nix-config embeds the configuration store path in each service's arguments so
activation replaces the watcher when its policy changes.
