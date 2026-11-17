use crate::Sync;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Revision {
    pub commit_id: String,
    pub change_id: String,
    pub parents: Vec<String>,
    pub description: String,
}
#[derive(Debug, Serialize)]
pub struct Change {
    pub change_id: String,
    pub description: String,
    pub before: Vec<String>,
    pub after: Vec<String>,
    pub kind: String,
}
#[derive(Debug, Deserialize, Serialize)]
pub struct Evolution {
    pub successor: Revision,
    pub predecessors: Vec<Revision>,
}
#[derive(Debug, Serialize)]
pub struct Workspace {
    pub before: String,
    pub after: String,
    pub files: Vec<FileChange>,
    pub leaving_ancestry: Vec<Revision>,
    pub entering_ancestry: Vec<Revision>,
    pub available_outside_ancestry: Vec<Revision>,
}
#[derive(Debug, Serialize)]
pub struct FileChange {
    pub path: String,
    pub status: String,
}
#[derive(Debug, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub repository: String,
    pub dry_run: bool,
    pub peers: Vec<String>,
    pub changes: Vec<Change>,
    pub evolution: Vec<Evolution>,
    pub revisions: Vec<Revision>,
    pub workspace: Workspace,
    pub conflicts: Vec<String>,
    pub divergent: Vec<String>,
    pub bookmarks: Vec<RefChange>,
    pub tags: Vec<RefChange>,
    pub operation_before: String,
    pub operation_after: String,
}
#[derive(Debug, Serialize)]
pub struct RefChange {
    pub name: String,
    pub remote: Option<String>,
    pub before: Option<serde_json::Value>,
    pub after: Option<serde_json::Value>,
}
fn ref_changes(
    before: &BTreeMap<String, serde_json::Value>,
    after: &BTreeMap<String, serde_json::Value>,
) -> Vec<RefChange> {
    before
        .keys()
        .chain(after.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|key| before.get(*key) != after.get(*key))
        .map(|key| {
            let value = after.get(key).or(before.get(key)).unwrap();
            RefChange {
                name: value["name"].as_str().unwrap_or("").into(),
                remote: value["remote"].as_str().map(str::to_owned),
                before: before.get(key).cloned(),
                after: after.get(key).cloned(),
            }
        })
        .collect()
}
impl Sync {
    pub(crate) fn refs(
        &self,
        repo: &Path,
        command: &str,
    ) -> Result<BTreeMap<String, serde_json::Value>> {
        let text = self.jj(
            repo,
            &[
                command,
                "list",
                "--all-remotes",
                "-T",
                "json(self) ++ \"\\n\"",
            ],
            false,
            false,
            false,
        )?;
        text.lines()
            .map(|line| {
                let value: serde_json::Value = serde_json::from_str(line)?;
                let key = serde_json::to_string(&(&value["name"], &value["remote"]))?;
                Ok((key, value))
            })
            .collect()
    }
    pub(crate) fn revisions(
        &self,
        repo: &Path,
        revset: &str,
    ) -> Result<BTreeMap<String, Revision>> {
        let text = self.jj(
            repo,
            &[
                "log",
                "-r",
                revset,
                "--no-graph",
                "-T",
                "json(self) ++ \"\\n\"",
            ],
            false,
            false,
            false,
        )?;
        text.lines()
            .map(|line| {
                let r: Revision = serde_json::from_str(line)?;
                Ok((r.commit_id.clone(), r))
            })
            .collect()
    }
    pub(crate) fn evolution(&self, repo: &Path, revset: &str) -> Result<Vec<Evolution>> {
        let text = self.jj(repo, &["evolog", "-r", revset, "--no-graph", "-T",
            "\"{\\\"successor\\\":\" ++ json(commit) ++ \",\\\"predecessors\\\":\" ++ json(predecessors) ++ \"}\\n\""], false, false, false)?;
        text.lines()
            .map(|line| Ok(serde_json::from_str(line)?))
            .collect()
    }
    pub(crate) fn report(
        &self,
        repo: &Path,
        before: &BTreeMap<String, Revision>,
        before_tip: &str,
        baseline: &str,
        before_refs: &(
            BTreeMap<String, serde_json::Value>,
            BTreeMap<String, serde_json::Value>,
        ),
        repository: &str,
        peers: Vec<String>,
        dry_run: bool,
    ) -> Result<Report> {
        let after = self.revisions(repo, "all()")?;
        let affected: BTreeSet<_> = before
            .values()
            .filter(|r| !after.contains_key(&r.commit_id))
            .chain(
                after
                    .values()
                    .filter(|r| !before.contains_key(&r.commit_id)),
            )
            .map(|r| r.change_id.clone())
            .collect();
        let evolution = if affected.is_empty() {
            Vec::new()
        } else {
            self.evolution(repo, "all()")?
        };
        let changes: Vec<Change> = affected
            .iter()
            .map(|change| {
                let old: Vec<_> = before.values().filter(|r| &r.change_id == change).collect();
                let new: Vec<_> = after.values().filter(|r| &r.change_id == change).collect();
                let rewritten = !old.is_empty()
                    && !new.is_empty()
                    && old.iter().all(|old| {
                        // Follow JJ's evolution edges, including intermediate rewrites.
                        let mut known: BTreeSet<_> =
                            new.iter().map(|r| r.commit_id.clone()).collect();
                        loop {
                            let size = known.len();
                            for edge in &evolution {
                                if known.contains(&edge.successor.commit_id) {
                                    known.extend(
                                        edge.predecessors.iter().map(|r| r.commit_id.clone()),
                                    );
                                }
                            }
                            if known.len() == size {
                                break;
                            }
                        }
                        known.contains(&old.commit_id)
                    });
                Change {
                    change_id: change.clone(),
                    description: new
                        .first()
                        .or(old.first())
                        .unwrap()
                        .description
                        .lines()
                        .next()
                        .unwrap_or("")
                        .into(),
                    before: old.iter().map(|r| r.commit_id.clone()).collect(),
                    after: new.iter().map(|r| r.commit_id.clone()).collect(),
                    kind: if new.len() > 1 {
                        "divergent"
                    } else if old.is_empty() {
                        "added"
                    } else if new.is_empty() {
                        "retired"
                    } else if rewritten {
                        "rewritten"
                    } else {
                        "revised"
                    }
                    .into(),
                }
            })
            .collect();
        let after_tip = self.jj(
            repo,
            &["log", "-r", "@", "--no-graph", "-T", "commit_id"],
            false,
            false,
            false,
        )?;
        let files = self.jj(
            repo,
            &[
                "diff",
                "--from",
                before_tip,
                "--to",
                &after_tip,
                "-T",
                "\"[\" ++ json(path) ++ \",\" ++ json(status) ++ \"]\\n\"",
            ],
            false,
            false,
            false,
        )?;
        let files = files
            .lines()
            .map(|line| {
                let (path, status): (String, String) = serde_json::from_str(line)?;
                Ok(FileChange { path, status })
            })
            .collect::<Result<Vec<_>>>()?;
        let leaving = self.revisions(repo, &format!("::{before_tip} ~ ::{after_tip}"))?;
        let entering = self.revisions(repo, &format!("::{after_tip} ~ ::{before_tip}"))?;
        let ancestry = self.revisions(repo, &format!("::{after_tip}"))?;
        let leaving_changes: BTreeSet<_> = leaving.values().map(|r| &r.change_id).collect();
        let outside: Vec<_> = after
            .values()
            .filter(|r| {
                leaving_changes.contains(&r.change_id)
                    && !ancestry.values().any(|a| a.change_id == r.change_id)
            })
            .cloned()
            .collect();
        let conflict_ids = |revset| -> Result<Vec<String>> {
            Ok(self.revisions(repo, revset)?.into_keys().collect())
        };
        let conflicts = conflict_ids("conflicts()")?;
        let divergent = conflict_ids("divergent() ~ immutable()")?;
        let bookmarks = ref_changes(&before_refs.0, &self.refs(repo, "bookmark")?);
        let tags = ref_changes(&before_refs.1, &self.refs(repo, "tag")?);
        let evolution: Vec<_> = evolution
            .into_iter()
            .filter(|e| {
                affected.contains(&e.successor.change_id)
                    || e.predecessors
                        .iter()
                        .any(|p| affected.contains(&p.change_id))
            })
            .collect();
        let mut revisions = before.clone();
        revisions.extend(after);
        // Detail records describe the comparison, rather than serializing the
        // entire repository. Include changed versions, checkout effects, issues,
        // changed ref targets and their immediate parents for graph context.
        let mut relevant: BTreeSet<String> = changes
            .iter()
            .flat_map(|c| c.before.iter().chain(&c.after))
            .cloned()
            .chain([before_tip.to_owned(), after_tip.clone()])
            .chain(leaving.keys().cloned())
            .chain(entering.keys().cloned())
            .chain(outside.iter().map(|r| r.commit_id.clone()))
            .chain(conflicts.iter().cloned())
            .chain(divergent.iter().cloned())
            .chain(
                evolution
                    .iter()
                    .flat_map(|e| std::iter::once(&e.successor).chain(&e.predecessors))
                    .map(|r| r.commit_id.clone()),
            )
            .collect();
        for change in bookmarks.iter().chain(&tags) {
            for value in change.before.iter().chain(&change.after) {
                for field in ["target", "tracking_target"] {
                    if let Some(targets) = value[field].as_array() {
                        relevant.extend(
                            targets
                                .iter()
                                .filter_map(|id| id.as_str().map(str::to_owned)),
                        );
                    }
                }
            }
        }
        let parents: Vec<_> = relevant
            .iter()
            .filter_map(|id| revisions.get(id))
            .flat_map(|r| r.parents.iter().cloned())
            .collect();
        relevant.extend(parents);
        revisions.retain(|id, _| relevant.contains(id));
        Ok(Report {
            schema_version: 1,
            repository: repository.into(),
            dry_run,
            peers,
            changes,
            evolution,
            revisions: revisions.into_values().collect(),
            workspace: Workspace {
                before: before_tip.into(),
                after: after_tip,
                files,
                available_outside_ancestry: outside,
                leaving_ancestry: leaving.into_values().collect(),
                entering_ancestry: entering.into_values().collect(),
            },
            conflicts,
            divergent,
            bookmarks,
            tags,
            operation_before: baseline.into(),
            operation_after: self.jj(
                repo,
                &["op", "log", "--limit", "1", "--no-graph", "-T", "id"],
                false,
                false,
                false,
            )?,
        })
    }
    pub(crate) fn render_report(
        &self,
        repo: &Path,
        report: &Report,
        verbose: bool,
        color: &str,
    ) -> Result<()> {
        println!(
            "{} staged updates from {}.",
            if report.dry_run {
                "Previewing"
            } else {
                "Applied"
            },
            report.peers.join(", ")
        );
        println!(
            "Working copy: {} → {}",
            &report.workspace.before[..12],
            &report.workspace.after[..12]
        );
        if report.workspace.before != report.workspace.after {
            println!(
                "Incoming descendants remain available; apply follows the working-copy change."
            );
        }
        if !report.workspace.available_outside_ancestry.is_empty() {
            println!(
                "Changes available elsewhere in history, outside the proposed checkout ancestry:"
            );
            for r in &report.workspace.available_outside_ancestry {
                println!(
                    "  {} {}",
                    &r.change_id[..8],
                    r.description.lines().next().unwrap_or("")
                );
            }
        }
        if !report.changes.is_empty() {
            println!("\nChange ID     Before                    After                     Outcome / description");
            for change in &report.changes {
                let short = |ids: &[String]| {
                    if ids.is_empty() {
                        "—".into()
                    } else {
                        ids.iter()
                            .map(|id| id[..8].to_owned())
                            .collect::<Vec<_>>()
                            .join(",")
                    }
                };
                println!(
                    "{:<12}  {:<24}  {:<24}  {}: {}",
                    &change.change_id[..8],
                    short(&change.before),
                    short(&change.after),
                    change.kind,
                    change.description
                );
            }
        }
        // JJ renders the proposed visible graph. Old revisions are compared in the
        // table/op diff, so a display-only union cannot masquerade as divergence.
        let seeds: Vec<_> = report
            .changes
            .iter()
            .flat_map(|c| c.after.iter())
            .cloned()
            .chain(std::iter::once(report.workspace.after.clone()))
            .collect();
        let revset = format!("ancestors({}, 2)", seeds.join(" | "));
        println!(
            "\nResulting history (@ is the {} checkout):",
            if report.dry_run {
                "proposed"
            } else {
                "current"
            }
        );
        println!("{}", self.jj_display(repo, &["log", "-r", &revset], color)?);
        if verbose {
            println!(
                "{}",
                self.jj_display(
                    repo,
                    &[
                        "op",
                        "diff",
                        "--from",
                        &report.operation_before,
                        "--to",
                        &report.operation_after
                    ],
                    color
                )?
            );
        }
        println!(
            "\nWorkspace content changes:\n{}",
            self.jj_display(
                repo,
                &[
                    "diff",
                    "--from",
                    &report.workspace.before,
                    "--to",
                    &report.workspace.after,
                    "--stat"
                ],
                color
            )?
        );
        if !report.conflicts.is_empty() || !report.divergent.is_empty() {
            println!(
                "Review required: {} conflicted revisions, {} divergent revisions.",
                report.conflicts.len(),
                report.divergent.len()
            );
        }
        if report.dry_run {
            println!("Run `jj-sync apply` to accept this staged state.");
        }
        Ok(())
    }
}
