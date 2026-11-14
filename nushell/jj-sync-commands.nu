# Run transport/setup commands normally. Structured subcommands below shadow
# the executable; ^jj-sync always selects the terminal interface.
export def --wrapped jj-sync [...args: string] {
  ^jj-sync ...$args
}

# Inspect cached transport status as Nushell records.
export def "jj-sync status" [--config: path --json] {
  let options = if $config == null { [] } else { [--config $config] }
  ^jj-sync ...$options status --json | from json
}

# Refresh staged updates and return one record per peer.
export def "jj-sync pending" [
  --path: path = "."
  --cached                 # Inspect downloaded snapshots without refreshing
  --count                  # Cached numeric prompt interface
  --json                   # Records are already structured
  --config: path
] {
  let options = if $config == null { [] } else { [--config $config] }
  let cache = if $cached { [--cached] } else { [] }
  if $count {
    ^jj-sync ...$options pending --path $path --count | str trim | into int
  } else {
    let result = (^jj-sync ...$options pending --path $path ...$cache --json | from json)
    $result.peers | transpose peer update | each {|row|
      $row.update | insert peer $row.peer | insert repository $result.repository
    }
  }
}

# Reconcile staged updates, or inspect the proposed result with --dry-run.
# The result contains changes, revisions, workspace effects and evolution edges.
export def "jj-sync apply" [
  --dry-run                # Preview in an isolated repository
  --preview                # Alias for --dry-run
  --json                   # Records are already structured
  --cached                 # Use downloaded snapshots
  --path: path = "."
  --config: path
] {
  let options = if $config == null { [] } else { [--config $config] }
  let preview_args = if $dry_run or $preview { [--dry-run] } else { [] }
  let cache = if $cached { [--cached] } else { [] }
  ^jj-sync ...$options apply --path $path ...$preview_args ...$cache --json | from json
}
