# Run transport/setup commands normally. ^jj-sync always selects the terminal UI.
export def --wrapped jj-sync [...args: string] {
  ^jj-sync ...$args
}

def short-id [id: string] { $id | str substring 0..7 }
def short-ids [ids: list<string>] {
  if ($ids | is-empty) { return '—' }
  let visible = ($ids | take 2 | each {|id| short-id $id } | str join ',')
  if ($ids | length) > 2 { $"($visible),+((($ids | length) - 2))" } else { $visible }
}
def short-revision [revision: record] {
  {
    change_id: (short-id $revision.change_id)
    revision: (short-id $revision.commit_id)
    description: ($revision.description | lines | first | default '')
  }
}

# Inspect cached transport status as Nushell records.
export def "jj-sync status" [--config: path --json] {
  let options = if $config == null { [] } else { [--config $config] }
  ^jj-sync ...$options status --json | from json
}

# Show incoming revisions in a flat table. --details includes full peer records.
export def "jj-sync pending" [
  --path: path = "."
  --cached                 # Inspect downloaded snapshots without refreshing
  --count                  # Cached numeric prompt interface
  --details                # Full peer records, including IDs and fetch metadata
  --json                   # Alias for --details; ^jj-sync produces JSON text
  --config: path
] {
  let options = if $config == null { [] } else { [--config $config] }
  let cache = if $cached { [--cached] } else { [] }
  if $count {
    ^jj-sync ...$options pending --path $path --count | str trim | into int
  } else {
    let result = (^jj-sync ...$options pending --path $path ...$cache --json | from json)
    let peers = ($result.peers | transpose peer update)
    if $details or $json {
      $peers | each {|row|
        $row.update | insert peer $row.peer | insert repository $result.repository
      }
    } else {
      $peers | each {|row|
        let peer = $row.peer
        let update = $row.update
        let changes = ($update.revisions | each {|revision|
          {peer: $peer kind: incoming} | merge (short-revision $revision)
        })
        let tags = ($update.tags | each {|tag|
          let parts = ($tag | split row ' ')
          {peer: $peer kind: tag change_id: '' revision: (short-id ($parts | last)) description: ($parts | first)}
        })
        let deleted = ($update.deleted_bookmarks | each {|name|
          {peer: $peer kind: removed change_id: '' revision: '' description: $name}
        })
        mut rows = ($changes | append $tags | append $deleted)
        let total = ($rows | length)
        $rows = ($rows | take 10)
        if $total > 10 {
          $rows = ($rows | append {peer: $peer kind: more change_id: '' revision: '' description: $"($total - 10) more updates; use --details"})
        }
        if $update.error != null {
          $rows = ($rows | append {peer: $peer kind: error change_id: '' revision: '' description: $update.error})
        }
        if ($rows | is-empty) {
          let state = if $update.pending { 'pending' } else if $update.lastSuccess == null { 'unfetched' } else { 'up to date' }
          let description = if $update.pending {
            'Staged refs changed; inspect apply --dry-run'
          } else if $update.lastSuccess == null { 'No successful fetch yet' } else { $"Last fetched ($update.lastSuccess)" }
          [{peer: $peer kind: $state change_id: '' revision: '' description: $description}]
        } else { $rows }
      } | flatten
    }
  }
}

# Reconcile staged updates, or return a compact preview with --dry-run.
# --details exposes full IDs, affected history, evolution and ref changes.
export def "jj-sync apply" [
  --dry-run                # Preview in an isolated repository
  --preview                # Alias for --dry-run
  --details                # Full reconciliation report
  --json                   # Alias for --details; ^jj-sync produces JSON text
  --cached                 # Use downloaded snapshots
  --path: path = "."
  --config: path
] {
  let options = if $config == null { [] } else { [--config $config] }
  let preview_args = if $dry_run or $preview { [--dry-run] } else { [] }
  let cache = if $cached { [--cached] } else { [] }
  let report = (^jj-sync ...$options apply --path $path ...$preview_args ...$cache --json | from json)
  if $details or $json { return $report }
  let changes = ($report.changes | take 10 | each {|change|
    {
      change_id: (short-id $change.change_id)
      before: (short-ids $change.before)
      after: (short-ids $change.after)
      kind: $change.kind
      description: $change.description
    }
  })
  let before = (short-id $report.workspace.before)
  let after = (short-id $report.workspace.after)
  let checkout = if $report.workspace.before == $report.workspace.after { $"unchanged ($after)" } else { $"($before) → ($after)" }
  mut summary = {
    mode: (if $report.dry_run { 'preview' } else { 'applied' })
    peers: ($report.peers | str join ', ')
    changes: $changes
    workspace: {checkout: $checkout files: ($report.workspace.files | length)}
    conflicts: ($report.conflicts | length)
    divergent: ($report.divergent | length)
  }
  if ($report.changes | length) > 10 {
    $summary = ($summary | insert more_changes (($report.changes | length) - 10))
  }
  if not ($report.workspace.files | is-empty) {
    $summary = ($summary | insert file_changes ($report.workspace.files | take 10))
    if ($report.workspace.files | length) > 10 {
      $summary = ($summary | insert more_files (($report.workspace.files | length) - 10))
    }
  }
  if not ($report.workspace.available_outside_ancestry | is-empty) {
    $summary = ($summary | insert outside_checkout ($report.workspace.available_outside_ancestry | take 10 | each {|revision| short-revision $revision }))
    if ($report.workspace.available_outside_ancestry | length) > 10 {
      $summary = ($summary | insert more_outside_checkout (($report.workspace.available_outside_ancestry | length) - 10))
    }
  }
  for kind in [conflicts divergent] {
    if not ($report | get $kind | is-empty) {
      $summary = ($summary | insert $"($kind)_revisions" ($report | get $kind | take 10 | each {|id| short-id $id }))
    }
  }
  if not ($report.bookmarks | is-empty) {
    $summary = ($summary | insert bookmarks_changed ($report.bookmarks | length))
  }
  if not ($report.tags | is-empty) {
    $summary = ($summary | insert tags_changed ($report.tags | length))
  }
  $summary
}
