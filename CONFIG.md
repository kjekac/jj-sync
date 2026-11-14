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

`status --json` includes per-repository `pending` flags. `pending --path DIR`
prints the pending peer count using cached status, suitable for shell prompts.
A snapshot containing only locally visible commits or proven predecessors of
visible local changes is baselined without importing its refs. This prevents a
peer's unchanged workspace marker from reintroducing your own superseded version.
Matching change IDs alone do not establish that relationship. Independent edits,
new deployment tags and removal of previously accepted bookmarks remain pending.
At a coordinated handoff, run `apply --preview` in the receiving repository.
It snapshots local files and prepares native JJ reconciliation in a disposable
copy of its metadata and Git store. The preview shows fetch results, proposed
history and workspace content changes. `apply` fetches staged refs locally,
so it works offline. A saved preview is checked against local operation identity
and staged refs; preview again if either changes. Preview is optional.

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

An optional `postFetchCommand` is an absolute executable path. It runs directly,
with the repository path as its only argument, after each successful peer or
GitHub fetch. Hook failure is reported as a cycle error. Use it for local policy
such as retaining deployment tags.

`github` selects an existing GitHub remote, preferring `origin`. It fetches through
an untracked `sync-github` remote, publishes host workspace and feature bookmarks,
and pushes `hosts/<host>/*` bookmarks and local tags with JJ's guarded push.
