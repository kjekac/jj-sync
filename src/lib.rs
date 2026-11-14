mod process;
mod report;
pub use process::{install_signal_handlers, received_signal};

use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::{SecondsFormat, Utc};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::Write,
    os::{
        fd::AsRawFd,
        unix::{
            fs::{DirBuilderExt, OpenOptionsExt},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RepositoryId(String);

impl TryFrom<String> for RepositoryId {
    type Error = anyhow::Error;
    fn try_from(value: String) -> Result<Self> {
        ensure!(
            value.chars().count() < 1024
                && !value.chars().any(|c| c < ' ' || c == '\u{7f}')
                && value
                    .split('/')
                    .all(|part| !matches!(part, "" | "." | "..")),
            "Invalid repository ID: {value:?}"
        );
        Ok(Self(value))
    }
}
impl From<RepositoryId> for String {
    fn from(id: RepositoryId) -> Self {
        id.0
    }
}
impl RepositoryId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Peer {
    pub address: String,
    pub ssh_alias: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub host: String,
    pub home: PathBuf,
    pub state: PathBuf,
    #[serde(default)]
    pub repository_roots: BTreeMap<String, PathBuf>,
    #[serde(default)]
    pub repositories: BTreeSet<RepositoryId>,
    pub peers: BTreeMap<String, Peer>,
    pub ssh_config: PathBuf,
    #[serde(default)]
    pub remote_command: Option<String>,
    #[serde(default)]
    pub post_fetch_command: Option<PathBuf>,
}

impl Config {
    pub fn from_json(text: &str) -> Result<Self> {
        let value: serde_json::Value = serde_json::from_str(text)?;
        ensure!(value.get("roots").is_none(), "Legacy unrestricted roots config: use repositoryRoots and an explicit repositories list");
        let config: Self = serde_json::from_value(value)?;
        for name in std::iter::once(&config.host)
            .chain(config.peers.keys())
            .chain(config.repository_roots.keys())
        {
            ensure!(
                identifier(name),
                "Invalid host, peer or root name: {name:?}"
            );
        }
        if let Some(path) = &config.post_fetch_command {
            ensure!(path.is_absolute(), "postFetchCommand must be absolute");
        }
        Ok(config)
    }
}
fn identifier(name: &str) -> bool {
    name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

#[derive(Clone, Debug)]
pub struct Tools {
    pub jj: PathBuf,
    pub git: PathBuf,
    pub ssh: PathBuf,
    pub keygen: PathBuf,
}
impl Default for Tools {
    fn default() -> Self {
        fn locate(compiled: Option<&str>, name: &str) -> PathBuf {
            if let Some(path) = compiled {
                return path.into();
            }
            std::env::var_os("PATH")
                .and_then(|paths| {
                    std::env::split_paths(&paths)
                        .map(|path| path.join(name))
                        .find(|path| path.is_file())
                        .and_then(|path| path.canonicalize().ok())
                })
                .unwrap_or_else(|| name.into())
        }
        Self {
            jj: locate(option_env!("JJ_SYNC_JJ"), "jj"),
            git: locate(option_env!("JJ_SYNC_GIT"), "git"),
            ssh: locate(option_env!("JJ_SYNC_SSH"), "ssh"),
            keygen: locate(option_env!("JJ_SYNC_KEYGEN"), "ssh-keygen"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Repository {
    pub path: PathBuf,
    pub jj: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub jj: bool,
}
pub type Catalog = BTreeMap<RepositoryId, CatalogEntry>;
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_success: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<bool>,
}
pub type Status = BTreeMap<String, StatusEntry>;
fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, false)
}
fn success() -> StatusEntry {
    StatusEntry {
        last_success: Some(now()),
        ..Default::default()
    }
}

pub fn private_dir(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    Ok(())
}
pub fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let parent = path.parent().context("File has no parent")?;
    private_dir(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(text.as_bytes())?;
    file.persist(path)?;
    Ok(())
}

pub struct Sync {
    pub config: Config,
    pub tools: Tools,
}
impl Sync {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            tools: Tools::default(),
        }
    }
    fn command(&self, executable: &Path) -> Command {
        let mut command = Command::new(executable);
        let mut paths = vec![
            self.tools
                .git
                .parent()
                .unwrap_or(Path::new("."))
                .to_path_buf(),
            self.tools
                .ssh
                .parent()
                .unwrap_or(Path::new("."))
                .to_path_buf(),
        ];
        if let Some(path) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&path));
        }
        command.env("PATH", std::env::join_paths(paths).unwrap());
        command
    }
    pub fn jj(
        &self,
        repo: &Path,
        args: &[&str],
        snapshot: bool,
        interactive: bool,
        peer: bool,
    ) -> Result<String> {
        let mut command = self.command(&self.tools.jj);
        command
            .arg("-R")
            .arg(repo)
            .args(["--no-pager", "--color=never"]);
        if !snapshot {
            command.arg("--ignore-working-copy");
        }
        command.args(args);
        if peer {
            command
                .env(
                    "GIT_SSH_COMMAND",
                    shell_words::join([
                        self.tools.ssh.to_str().context("Non-UTF-8 SSH path")?,
                        "-F",
                        self.config
                            .ssh_config
                            .to_str()
                            .context("Non-UTF-8 SSH config path")?,
                    ]),
                )
                .env("GIT_TERMINAL_PROMPT", "0");
        }
        process::output(command, None, interactive)
    }
    fn jj_display(&self, repo: &Path, args: &[&str], color: &str) -> Result<String> {
        use std::io::IsTerminal;
        let color = match color {
            "auto" => {
                if std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none() {
                    "always"
                } else {
                    "never"
                }
            }
            other => other,
        };
        let mut command = self.command(&self.tools.jj);
        command
            .current_dir(repo)
            .args(["--no-pager", "--ignore-working-copy", "--color", color])
            .args(args);
        process::output(command, None, false)
    }
    pub fn lock(&self) -> Result<File> {
        private_dir(&self.config.state)?;
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(self.config.state.join("lock"))?;
        loop {
            ensure!(
                received_signal() == 0,
                "Interrupted by signal {}",
                received_signal()
            );
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(file);
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error.into());
            }
        }
    }
    pub fn setup(&self) -> Result<()> {
        private_dir(&self.config.state)?;
        let key = self.config.state.join("identity");
        if !key.exists() {
            let mut command = self.command(&self.tools.keygen);
            command
                .args(["-q", "-t", "ed25519", "-N", "", "-C"])
                .arg(format!("jj-sync@{}", self.config.host))
                .arg("-f")
                .arg(key);
            process::output(command, None, false)?;
        }
        Ok(())
    }
    pub fn public_key(&self) -> Result<String> {
        self.setup()?;
        let mut command = self.command(&self.tools.keygen);
        command
            .args(["-y", "-f"])
            .arg(self.config.state.join("identity"));
        Ok(process::output(command, None, false)?
            .split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .join(" "))
    }
    pub fn accept_key(&self, peer: &str, value: &str) -> Result<()> {
        ensure!(self.config.peers.contains_key(peer), "Unknown peer: {peer}");
        let parts: Vec<_> = value.split_whitespace().collect();
        let valid = parts.len() == 2
            && parts[0] == "ssh-ed25519"
            && STANDARD.decode(parts[1]).is_ok_and(|bytes| {
                bytes.len() == 51 && bytes.starts_with(b"\0\0\0\x0bssh-ed25519\0\0\0\x20")
            });
        ensure!(valid, "Expected one Ed25519 public key");
        let target = self.config.state.join("peers").join(format!("{peer}.pub"));
        if target.exists() {
            ensure!(
                fs::read_to_string(&target)?.trim() == value.trim(),
                "Key changed for {peer}; revoke the old pairing first"
            );
        }
        write_atomic(&target, &format!("{}\n", value.trim()))?;
        self.authorized_keys()
    }
    pub fn authorized_keys(&self) -> Result<()> {
        let mut keys = String::new();
        for peer in self.config.peers.keys() {
            let path = self.config.state.join("peers").join(format!("{peer}.pub"));
            if path.exists() {
                keys.push_str(&format!(
                    "restrict {} {peer}\n",
                    fs::read_to_string(path)?.trim()
                ));
            }
        }
        write_atomic(&self.config.state.join("authorized_keys"), &keys)
    }
    pub fn revoke(&self, peer: &str) -> Result<()> {
        ensure!(self.config.peers.contains_key(peer), "Unknown peer");
        let path = self.config.state.join("peers").join(format!("{peer}.pub"));
        match fs::remove_file(path) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        self.authorized_keys()
    }
    pub fn pair(&self, peer: &str) -> Result<()> {
        let settings = self.config.peers.get(peer).context("Unknown peer")?;
        let remote = self
            .config
            .remote_command
            .as_deref()
            .context("remoteCommand is required for pairing")?;
        let remote_command = shell_words::join([remote, "pair-receive", &self.config.host]);
        let mut command = self.command(&self.tools.ssh);
        command.args([
            "-oControlMaster=no",
            "-oControlPath=none",
            "-oForwardAgent=no",
            "-oControlPersist=no",
            &settings.ssh_alias,
            &remote_command,
        ]);
        let value = process::output(command, Some(&format!("{}\n", self.public_key()?)), true)?;
        self.accept_key(peer, &value)
    }
    pub fn discover(&self) -> Result<BTreeMap<RepositoryId, Repository>> {
        let mut repos = BTreeMap::new();
        for (name, root) in &self.config.repository_roots {
            if root.is_dir() {
                self.walk(root, name, &mut repos)?;
            }
        }
        Ok(repos)
    }
    fn walk(
        &self,
        current: &Path,
        id: &str,
        repos: &mut BTreeMap<RepositoryId, Repository>,
    ) -> Result<()> {
        if !self
            .config
            .repositories
            .iter()
            .any(|value| value.0 == id || value.0.starts_with(&format!("{id}/")))
            || current.join(".jj-sync-ignore").is_file()
        {
            return Ok(());
        }
        let is_jj = current.join(".jj").is_dir();
        let is_git = current.join(".git").exists();
        let bare = current.join("objects").is_dir()
            && current.join("refs").is_dir()
            && current.join("HEAD").is_file();
        if is_jj || is_git || bare {
            let id = RepositoryId::try_from(id.to_owned())?;
            if self.config.repositories.contains(&id) {
                repos.insert(
                    id,
                    Repository {
                        path: current.to_owned(),
                        jj: is_jj,
                    },
                );
            }
            return Ok(());
        }
        for entry in fs::read_dir(current)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.starts_with('.')
                && !matches!(name, "node_modules" | "target" | "vendor")
                && entry.file_type()?.is_dir()
            {
                self.walk(&entry.path(), &format!("{id}/{name}"), repos)?;
            }
        }
        Ok(())
    }
    pub fn git_dir(&self, repo: &Repository) -> Result<PathBuf> {
        let result = if repo.jj {
            PathBuf::from(self.jj(
                &repo.path,
                &["--at-op=@", "git", "root"],
                false,
                false,
                false,
            )?)
        } else if repo.path.join(".git").is_file() {
            let text = fs::read_to_string(repo.path.join(".git"))?;
            repo.path.join(
                text.strip_prefix("gitdir: ")
                    .context("Invalid Git directory pointer")?
                    .trim(),
            )
        } else if repo.path.join(".git").is_dir() {
            repo.path.join(".git")
        } else {
            repo.path.clone()
        };
        let result = result.canonicalize()?;
        ensure!(
            self.config.repository_roots.values().any(|root| root
                .canonicalize()
                .is_ok_and(|root| result.starts_with(root))),
            "Git directory is outside configured repository roots"
        );
        Ok(result)
    }
    pub fn catalog(&self) -> Result<Catalog> {
        Ok(self
            .discover()?
            .into_iter()
            .map(|(id, repo)| (id, CatalogEntry { jj: repo.jj }))
            .collect())
    }
    pub fn serve(&self, original: &str) -> Result<()> {
        if original == "jj-sync-list" {
            println!("{}", serde_json::to_string(&self.catalog()?)?);
            return Ok(());
        }
        let args = request_words(original)?;
        ensure!(
            args.len() == 2 && args[0] == "git-upload-pack",
            "Only repository listing and fetching are permitted"
        );
        let id = RepositoryId::try_from(args[1].strip_prefix('/').unwrap_or(&args[1]).to_owned())?;
        let repos = self.discover()?;
        let repo = repos.get(&id).context("Unknown repository")?;
        let directory = self.git_dir(repo)?;
        let mut command = Command::new(&self.tools.git);
        command
            .env_clear()
            .env("PATH", self.tools.git.parent().unwrap_or(Path::new(".")))
            .env("HOME", &self.config.home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .args([
                "-c",
                "uploadpack.allowAnySHA1InWant=false",
                "-c",
                "uploadpack.allowReachableSHA1InWant=false",
                "-c",
                "uploadpack.allowTipSHA1InWant=false",
                "upload-pack",
                "--strict",
                "--timeout=60",
            ])
            .arg(directory);
        Err(command.exec().into())
    }
    pub fn snapshot(&self, repo: &Path, backup: bool) -> Result<()> {
        self.jj(repo, &["status"], true, false, false)?;
        let prefix = format!("hosts/{}/", self.config.host);
        let template = "if(!remote && present, name ++ \"\\t\" ++ if(conflict, \"CONFLICT\", normal_target.commit_id()) ++ \"\\n\")";
        let output = self.jj(
            repo,
            &["bookmark", "list", "-T", template],
            false,
            false,
            false,
        )?;
        let refs = parse_pairs(&output, '\t')?;
        let output = self.jj(
            repo,
            &[
                "workspace",
                "list",
                "-T",
                "name ++ \"\\t\" ++ target.commit_id() ++ \"\\n\"",
            ],
            false,
            false,
            false,
        )?;
        let mut desired: BTreeMap<String, String> = parse_pairs(&output, '\t')?
            .into_iter()
            .map(|(name, commit)| (format!("{prefix}work/{name}"), commit))
            .collect();
        if backup {
            for (name, commit) in &refs {
                if !name.starts_with("hosts/") && commit != "CONFLICT" {
                    desired.insert(format!("{prefix}bookmarks/{name}"), commit.clone());
                }
            }
        }
        for (name, commit) in &desired {
            if refs.get(name) != Some(commit) {
                self.jj(
                    repo,
                    &["bookmark", "set", name, "-r", commit, "--allow-backwards"],
                    false,
                    false,
                    false,
                )?;
            }
        }
        for name in refs.keys() {
            if name.starts_with(&prefix) && !desired.contains_key(name) {
                self.jj(repo, &["bookmark", "delete", name], false, false, false)?;
            }
        }
        self.jj(repo, &["git", "export"], false, false, false)?;
        Ok(())
    }
    pub fn ensure_remote(&self, repo: &Path, name: &str, url: &str) -> Result<()> {
        let remotes = parse_pairs(
            &self.jj(repo, &["git", "remote", "list"], false, false, false)?,
            ' ',
        )?;
        if let Some(old) = remotes.get(name) {
            ensure!(
                old == url,
                "Remote {name} already has a different URL in {}",
                repo.display()
            );
        } else {
            self.jj(
                repo,
                &["git", "remote", "add", name, url],
                false,
                false,
                false,
            )?;
        }
        self.jj(
            repo,
            &[
                "config",
                "set",
                "--repo",
                &format!("remotes.{name}.auto-track-bookmarks"),
                "glob:* ~ glob:*",
            ],
            false,
            false,
            false,
        )?;
        ensure!(
            self.jj(
                repo,
                &[
                    "bookmark",
                    "list",
                    "--remote",
                    name,
                    "--tracked",
                    "-T",
                    "name ++ \"\\n\""
                ],
                false,
                false,
                false
            )?
            .is_empty(),
            "Untrack bookmarks on {name} before unattended fetching: {}",
            repo.display()
        );
        Ok(())
    }
    pub fn fetch(
        &self,
        repo: &Path,
        name: &str,
        url: &str,
        peer: bool,
        interactive: bool,
    ) -> Result<()> {
        self.ensure_remote(repo, name, url)?;
        self.jj(
            repo,
            &[
                "git", "fetch", "--remote", name, "--branch", "glob:*", "--tag", "glob:*",
            ],
            true,
            interactive,
            peer,
        )?;
        if let Some(hook) = &self.config.post_fetch_command {
            let mut command = self.command(hook);
            command.arg(repo);
            process::output(command, None, false)?;
        }
        Ok(())
    }
    fn stage_refs(&self, stage: &Path, peer: &str) -> Result<String> {
        self.jj(
            stage,
            &[
                "bookmark",
                "list",
                "--remote",
                &format!("sync-{peer}"),
                "-T",
                "if(!conflict, name ++ \"\\t\" ++ normal_target.commit_id() ++ \"\\n\")",
            ],
            false,
            false,
            false,
        )
    }
    fn export_stage(&self, stage: &Path, peer: &str) -> Result<()> {
        let refs = parse_pairs(&self.stage_refs(stage, peer)?, '\t')?;
        let locals = self.jj(
            stage,
            &["bookmark", "list", "-T", "if(!remote, name ++ \"\\n\")"],
            false,
            false,
            false,
        )?;
        for name in locals.lines().filter(|name| !refs.contains_key(*name)) {
            self.jj(stage, &["bookmark", "delete", name], false, false, false)?;
        }
        for (name, revision) in refs {
            self.jj(
                stage,
                &[
                    "bookmark",
                    "set",
                    &name,
                    "-r",
                    &revision,
                    "--allow-backwards",
                ],
                false,
                false,
                false,
            )?;
        }
        self.jj(stage, &["git", "export"], false, false, false)?;
        Ok(())
    }
    fn stage_fingerprint(&self, stage: &Path, peer: &str) -> Result<String> {
        let tags = self.jj(
            stage,
            &[
                "tag",
                "list",
                "--all-remotes",
                "-T",
                "name ++ \" \" ++ if(conflict, \"CONFLICT\", normal_target.commit_id()) ++ \"\\n\"",
            ],
            false,
            false,
            false,
        )?;
        Ok(format!("{}\n{}", self.stage_refs(stage, peer)?, tags))
    }
    fn accepted_path(&self, peer: &str, id: &RepositoryId) -> PathBuf {
        self.config
            .state
            .join("accepted")
            .join(peer)
            .join(format!("{}.json", id.0))
    }
    fn pending_stage(&self, stage: &Path, peer: &str, id: &RepositoryId) -> Result<bool> {
        let fingerprint = self.stage_fingerprint(stage, peer)?;
        let accepted = self.accepted_path(peer, id);
        let previous = match fs::read_to_string(&accepted) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error.into()),
        };
        if previous == fingerprint {
            return Ok(false);
        }
        let repos = self.discover()?;
        let Some(local) = repos.get(id).filter(|repo| repo.jj) else {
            return Ok(true);
        };
        let incoming = parse_pairs(&self.stage_refs(stage, peer)?, '\t')?;
        // A removed previously accepted bookmark is a real publication change,
        // even when its old commit remains available locally.
        if previous
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .any(|(name, _)| !incoming.contains_key(name))
        {
            return Ok(true);
        }
        let accounted = self.accounted_revisions(&local.path)?;
        if incoming
            .values()
            .any(|revision| !accounted.contains(revision))
        {
            return Ok(true);
        }
        let template =
            "name ++ \" \" ++ if(conflict, \"CONFLICT\", normal_target.commit_id()) ++ \"\\n\"";
        let local_tags = self.jj(
            &local.path,
            &["--at-op=@", "tag", "list", "--all-remotes", "-T", template],
            false,
            false,
            false,
        )?;
        let peer_tags = self.jj(
            stage,
            &["tag", "list", "--all-remotes", "-T", template],
            false,
            false,
            false,
        )?;
        let local_tags: BTreeSet<_> = local_tags.lines().collect();
        if peer_tags.lines().any(|tag| !local_tags.contains(tag)) {
            return Ok(true);
        }
        // Baseline a fully accounted-for snapshot without importing stale refs.
        // This also records known bookmark names so later deletions are noticed.
        write_atomic(&accepted, &fingerprint)?;
        Ok(false)
    }
    fn accounted_revisions(&self, repo: &Path) -> Result<BTreeSet<String>> {
        let visible = self.jj(
            repo,
            &[
                "--at-op=@",
                "log",
                "-r",
                "all()",
                "--no-graph",
                "-T",
                "commit_id ++ \"\\n\"",
            ],
            false,
            false,
            false,
        )?;
        let mut accounted: BTreeSet<String> = visible.lines().map(str::to_owned).collect();
        let evolution = self.jj(repo, &["--at-op=@", "evolog", "-r", "all()", "--no-graph", "-T",
            "commit.commit_id() ++ \" \" ++ commit.change_id() ++ \" \" ++ predecessors.map(|p| p.change_id() ++ \":\" ++ p.commit_id()).join(\",\") ++ \"\\n\""], false, false, false)?;
        let mut edges = Vec::new();
        for line in evolution.lines() {
            let mut fields = line.splitn(3, ' ');
            let successor = fields.next().unwrap_or_default();
            let change = fields.next().unwrap_or_default();
            for edge in fields.next().unwrap_or_default().split(',') {
                if let Some((old_change, old)) = edge.split_once(':') {
                    if old_change == change {
                        edges.push((successor.to_owned(), old.to_owned()));
                    }
                }
            }
        }
        // Only actual local evolution establishes a superseded version.
        // A matching change ID on an independent peer edit is insufficient.
        loop {
            let before = accounted.len();
            for (successor, old) in &edges {
                if accounted.contains(successor) {
                    accounted.insert(old.clone());
                }
            }
            if accounted.len() == before {
                break;
            }
        }
        Ok(accounted)
    }
    pub fn pending(&self, path: &Path) -> Result<usize> {
        let status = self.read_status()?;
        let path = path.canonicalize()?;
        let mut count = 0;
        for (key, entry) in status {
            if entry.pending != Some(true) {
                continue;
            }
            if let Some((_, id)) = key.split_once('/') {
                if let Some((root, relative)) = id.split_once('/') {
                    if let Some(base) = self.config.repository_roots.get(root) {
                        if let Ok(repository) = base.join(relative).canonicalize() {
                            if path.starts_with(repository) {
                                count += 1;
                            }
                        }
                    }
                }
            }
        }
        Ok(count)
    }
    pub fn read_status(&self) -> Result<Status> {
        let file = self.config.state.join("status.json");
        if file.exists() {
            Ok(serde_json::from_str(&fs::read_to_string(file)?)?)
        } else {
            Ok(BTreeMap::new())
        }
    }
    fn local_repository(&self, path: &Path) -> Result<(RepositoryId, Repository)> {
        let path = path.canonicalize()?;
        self.discover()?
            .into_iter()
            .filter(|(_, repo)| repo.jj)
            .find(|(_, repo)| path.starts_with(&repo.path))
            .context("Current directory is not an allowlisted JJ repository")
    }
    pub fn has_preview(&self, path: &Path) -> Result<bool> {
        let (id, _) = self.local_repository(path)?;
        Ok(self
            .config
            .state
            .join("previews")
            .join(format!("{}.json", id.0))
            .exists())
    }
    pub fn show_pending(&self, path: &Path, json: bool) -> Result<()> {
        self.show_pending_with_color(path, json, "auto")
    }
    pub fn show_pending_with_color(&self, path: &Path, json: bool, color: &str) -> Result<()> {
        let (id, local) = self.local_repository(path)?;
        let status = self.read_status()?;
        let accounted = self.accounted_revisions(&local.path)?;
        let mut updates = BTreeMap::new();
        let tag_template =
            "name ++ \" \" ++ if(conflict, \"CONFLICT\", normal_target.commit_id()) ++ \"\\n\"";
        let local_tags = self.jj(
            &local.path,
            &["tag", "list", "--all-remotes", "-T", tag_template],
            false,
            false,
            false,
        )?;
        for peer in self.config.peers.keys() {
            let entry = status
                .get(&format!("{peer}/{}", id.0))
                .cloned()
                .unwrap_or_default();
            let error = status
                .get(peer)
                .and_then(|e| e.error.clone())
                .or(entry.error.clone());
            let mut revisions = Vec::new();
            let mut tags = Vec::new();
            let mut deleted_bookmarks = Vec::new();
            if entry.pending == Some(true) {
                let stage = self.config.state.join("staging").join(peer).join(&id.0);
                let refs = parse_pairs(&self.stage_refs(&stage, peer)?, '\t')?;
                let tips: BTreeSet<_> = refs.values().cloned().collect();
                if !tips.is_empty() {
                    revisions = self
                        .revisions(
                            &stage,
                            &format!(
                                "ancestors({})",
                                tips.into_iter().collect::<Vec<_>>().join(" | ")
                            ),
                        )?
                        .into_values()
                        .filter(|r| !accounted.contains(&r.commit_id))
                        .collect();
                }
                tags = self
                    .jj(
                        &stage,
                        &["tag", "list", "--all-remotes", "-T", tag_template],
                        false,
                        false,
                        false,
                    )?
                    .lines()
                    .filter(|tag| !local_tags.lines().any(|local| local == *tag))
                    .map(str::to_owned)
                    .collect();
                let previous = match fs::read_to_string(self.accepted_path(peer, &id)) {
                    Ok(text) => text,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                    Err(e) => return Err(e.into()),
                };
                deleted_bookmarks = previous
                    .lines()
                    .filter_map(|line| line.split_once('\t'))
                    .filter(|(name, _)| !refs.contains_key(*name))
                    .map(|(name, _)| name.to_owned())
                    .collect();
            }
            if !json {
                let state = if entry.pending == Some(true) {
                    "updates staged"
                } else if entry.last_success.is_some() {
                    "no staged updates"
                } else {
                    "not fetched yet"
                };
                println!(
                    "{peer}: {state} (last fetch: {})",
                    entry.last_success.as_deref().unwrap_or("never")
                );
                if let Some(error) = &error {
                    println!("  Fetch error: {error}");
                }
                if !revisions.is_empty() {
                    let stage = self.config.state.join("staging").join(peer).join(&id.0);
                    let revset = revisions
                        .iter()
                        .map(|r| r.commit_id.as_str())
                        .collect::<Vec<_>>()
                        .join(" | ");
                    println!("  {} incoming revisions:", revisions.len());
                    println!(
                        "{}",
                        self.jj_display(&stage, &["log", "-r", &revset, "--limit", "12"], color)?
                    );
                    if revisions.len() > 12 {
                        println!(
                            "  … {} more; use --json for all revisions",
                            revisions.len() - 12
                        );
                    }
                }
                for tag in &tags {
                    println!("  Deployment/tag update: {tag}");
                }
                for name in &deleted_bookmarks {
                    println!("  Bookmark removed: {name}");
                }
            }
            updates.insert(peer, serde_json::json!({"pending": entry.pending.unwrap_or(false), "lastSuccess": entry.last_success,
                "error": error, "revisions": revisions, "tags": tags, "deleted_bookmarks": deleted_bookmarks}));
        }
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"schema_version": 1, "repository": id.0, "peers": updates})
                )?
            );
        } else if updates.values().any(|e| e["pending"] == true) {
            println!("Inspect with `jj-sync apply --dry-run`; then `jj-sync apply`.");
        }
        Ok(())
    }
    pub fn apply_with_details(&self, path: &Path, preview: bool, verbose: bool) -> Result<()> {
        self.apply_inner(path, preview, verbose, false, "auto")
    }
    /// Apply only snapshots already downloaded by cycle. No network operations.
    pub fn apply(&self, path: &Path, preview: bool) -> Result<()> {
        self.apply_inner(path, preview, false, false, "auto")
    }
    pub fn apply_with_output(
        &self,
        path: &Path,
        preview: bool,
        verbose: bool,
        json: bool,
        color: &str,
    ) -> Result<()> {
        self.apply_inner(path, preview, verbose, json, color)
    }
    fn apply_inner(
        &self,
        path: &Path,
        preview: bool,
        verbose: bool,
        json: bool,
        color: &str,
    ) -> Result<()> {
        let path = path.canonicalize()?;
        let repos = self.discover()?;
        let (id, repo) = repos
            .iter()
            .filter(|(_, repo)| repo.jj)
            .find(|(_, repo)| path.starts_with(&repo.path))
            .context("Current directory is not an allowlisted JJ repository")?;
        self.jj(&repo.path, &["status"], true, false, false)?;
        let mut pending_peers = BTreeSet::new();
        for peer in self.config.peers.keys() {
            let stage = self.config.state.join("staging").join(peer).join(&id.0);
            if stage.join(".jj").exists() && self.pending_stage(&stage, peer, id)? {
                pending_peers.insert(peer.clone());
            }
        }
        let before_revisions = self.revisions(&repo.path, "all()")?;
        let before_refs = (
            self.refs(&repo.path, "bookmark")?,
            self.refs(&repo.path, "tag")?,
        );
        let baseline = self.jj(
            &repo.path,
            &["op", "log", "--limit", "1", "--no-graph", "-T", "id"],
            false,
            false,
            false,
        )?;
        let preview_path = self
            .config
            .state
            .join("previews")
            .join(format!("{}.json", id.0));
        let mut fingerprints = BTreeMap::new();
        for peer in self.config.peers.keys() {
            let stage = self.config.state.join("staging").join(peer).join(&id.0);
            if stage.join(".jj").exists() {
                fingerprints.insert(peer.clone(), self.stage_fingerprint(&stage, peer)?);
            }
        }
        let token = serde_json::to_string(&(baseline.clone(), &fingerprints))?;
        if !preview && preview_path.exists() {
            ensure!(
                fs::read_to_string(&preview_path)? == token,
                "Local history or staged updates changed since preview; run apply --dry-run again"
            );
        }
        let before_tip = self.jj(
            &repo.path,
            &["log", "-r", "@", "--no-graph", "-T", "commit_id"],
            false,
            false,
            false,
        )?;
        let isolated;
        let target = if preview && !pending_peers.is_empty() {
            isolated = tempfile::tempdir()?;
            copy_directory(&repo.path.join(".jj"), &isolated.path().join(".jj"))?;
            let directory = self.jj(&repo.path, &["git", "root"], false, false, false)?;
            let git = isolated.path().join("git-store");
            copy_directory(Path::new(&directory), &git)?;
            fs::write(
                isolated.path().join(".jj/repo/store/git_target"),
                git.to_str().context("Non UTF-8 Git path")?,
            )?;
            isolated.path().to_path_buf()
        } else {
            repo.path.clone()
        };
        for peer in self.config.peers.keys() {
            let stage = self.config.state.join("staging").join(peer).join(&id.0);
            if !stage.join(".jj").exists() || !self.pending_stage(&stage, peer, id)? {
                continue;
            }
            let name = format!("sync-{peer}");
            let url = self.jj(&stage, &["git", "root"], false, false, false)?;
            let remotes = parse_pairs(
                &self.jj(&target, &["git", "remote", "list"], false, false, false)?,
                ' ',
            )?;
            if let Some(old) = remotes.get(&name) {
                ensure!(
                    old == &url || old == &format!("ssh://{peer}/{}", id.0),
                    "Unexpected URL for {name}"
                );
                self.jj(
                    &target,
                    &["git", "remote", "set-url", &name, &url],
                    false,
                    false,
                    false,
                )?;
            }
            self.ensure_remote(&target, &name, &url)?;
            let args = [
                "git", "fetch", "--remote", &name, "--branch", "glob:*", "--tag", "glob:*",
            ];
            self.jj(&target, &args, !preview, false, false)?;
            if !preview {
                if let Some(hook) = &self.config.post_fetch_command {
                    let mut command = self.command(hook);
                    command.arg(&target);
                    process::output(command, None, false)?;
                }
                write_atomic(
                    &self.accepted_path(peer, id),
                    &self.stage_fingerprint(&stage, peer)?,
                )?;
            }
        }
        let report = self.report(
            &target,
            &before_revisions,
            &before_tip,
            &baseline,
            &before_refs,
            &id.0,
            pending_peers.into_iter().collect(),
            preview,
        )?;
        if preview {
            write_atomic(&preview_path, &token)?;
        } else if preview_path.exists() {
            fs::remove_file(&preview_path)?;
        }
        if json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else if report.peers.is_empty() {
            println!("No pending updates for {}.", id.0);
        } else {
            self.render_report(&target, &report, verbose, color)?;
        }
        if !preview {
            let mut status = self.read_status()?;
            for peer in self.config.peers.keys() {
                if let Some(entry) = status.get_mut(&format!("{peer}/{}", id.0)) {
                    entry.pending = Some(false);
                }
            }
            write_atomic(
                &self.config.state.join("status.json"),
                &(serde_json::to_string_pretty(&status)? + "\n"),
            )?;
        }
        Ok(())
    }
    pub fn sync_peer(
        &self,
        peer: &str,
        _local: &BTreeMap<RepositoryId, Repository>,
        status: &mut Status,
    ) -> Result<()> {
        if !self
            .config
            .state
            .join("peers")
            .join(format!("{peer}.pub"))
            .exists()
        {
            status.insert(
                peer.to_owned(),
                StatusEntry {
                    error: Some("Not paired".into()),
                    ..Default::default()
                },
            );
            return Ok(());
        }
        let mut command = self.command(&self.tools.ssh);
        command
            .arg("-F")
            .arg(&self.config.ssh_config)
            .args([peer, "jj-sync-list"]);
        let mut catalog: Catalog = serde_json::from_str(&process::output(command, None, false)?)
            .context("Invalid peer repository catalog")?;
        catalog.retain(|id, _| self.config.repositories.contains(id));
        let prefix = format!("{peer}/");
        status.retain(|key, _| {
            key.strip_prefix(&prefix)
                .is_none_or(|id| catalog.keys().any(|key| key.0 == id))
        });
        for id in catalog.keys() {
            let entry = status.entry(format!("{peer}/{}", id.0)).or_default();
            let result = (|| -> Result<PathBuf> {
                let target = self.config.state.join("staging").join(peer).join(&id.0);
                if !target.join(".jj").exists() {
                    fs::create_dir_all(target.parent().unwrap())?;
                    let mut command = self.command(&self.tools.jj);
                    command.args(["git", "init", "--no-colocate"]).arg(&target);
                    process::output(command, None, false)?;
                }
                const ENCODE: &AsciiSet = &NON_ALPHANUMERIC
                    .remove(b'/')
                    .remove(b'-')
                    .remove(b'_')
                    .remove(b'.')
                    .remove(b'~');
                let url = format!("ssh://{peer}/{}", utf8_percent_encode(&id.0, ENCODE));
                self.fetch(&target, &format!("sync-{peer}"), &url, true, false)?;
                self.export_stage(&target, peer)?;
                entry.pending = Some(self.pending_stage(&target, peer, id)?);
                Ok(target)
            })();
            match result {
                Ok(path) => {
                    entry.path = Some(path);
                    entry.last_success = Some(now());
                    entry.error = None;
                }
                Err(error) => entry.error = Some(format!("{error:#}")),
            }
        }
        status.insert(peer.to_owned(), success());
        Ok(())
    }
    /// Persist all results, then fail the command if any operation failed.
    pub fn cycle(&self) -> Result<()> {
        self.setup()?;
        let path = self.config.state.join("status.json");
        let mut status: Status = if path.exists() {
            serde_json::from_str(&fs::read_to_string(&path)?)?
        } else {
            BTreeMap::new()
        };
        for entry in status.values_mut() {
            entry.error = None;
        }
        let local = self.discover()?;
        status.retain(|key, _| {
            key.strip_prefix("local/")
                .is_none_or(|id| local.keys().any(|key| key.0 == id))
        });
        for (id, repo) in &local {
            if repo.jj {
                let entry = match self.snapshot(&repo.path, false) {
                    Ok(()) => success(),
                    Err(error) => StatusEntry {
                        error: Some(format!("{error:#}")),
                        ..Default::default()
                    },
                };
                status.insert(format!("local/{}", id.0), entry);
            }
        }
        for peer in self.config.peers.keys() {
            if let Err(error) = self.sync_peer(peer, &local, &mut status) {
                status.entry(peer.to_owned()).or_default().error = Some(format!("{error:#}"));
            }
        }
        write_atomic(&path, &(serde_json::to_string_pretty(&status)? + "\n"))?;
        let errors: BTreeMap<_, _> = status
            .iter()
            .filter_map(|(key, entry)| entry.error.as_ref().map(|error| (key, error)))
            .collect();
        if !errors.is_empty() {
            bail!("{}", serde_json::to_string_pretty(&errors)?);
        }
        Ok(())
    }
    pub fn github(&self) -> Result<()> {
        for (id, repo) in self.discover()? {
            if !repo.jj {
                continue;
            }
            let remotes = parse_pairs(
                &self.jj(&repo.path, &["git", "remote", "list"], false, false, false)?,
                ' ',
            )?;
            let github: BTreeMap<_, _> = remotes
                .into_iter()
                .filter(|(_, url)| {
                    url.starts_with("git@github.com:") || url.starts_with("ssh://git@github.com/")
                })
                .collect();
            let Some((remote, url)) = github
                .get_key_value("origin")
                .or_else(|| github.first_key_value())
            else {
                continue;
            };
            println!("Backing up {} to {remote}", id.0);
            self.fetch(&repo.path, "sync-github", url, false, true)?;
            self.snapshot(&repo.path, true)?;
            self.jj(
                &repo.path,
                &[
                    "git",
                    "push",
                    "--remote",
                    remote,
                    "--bookmark",
                    &format!("glob:hosts/{}/*", self.config.host),
                    "--tag",
                    "glob:*",
                    "--allow-empty-description",
                ],
                false,
                true,
                false,
            )?;
        }
        Ok(())
    }
}
fn parse_pairs(text: &str, delimiter: char) -> Result<BTreeMap<String, String>> {
    text.lines()
        .map(|line| {
            line.split_once(delimiter)
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .context("Malformed JJ output")
        })
        .collect()
}

// Treat '#' as data in restricted requests. shell-words enables comments,
// so protect unquoted hashes before parsing to ensure that extra arguments
// cannot disappear as comments.
fn request_words(text: &str) -> Result<Vec<String>> {
    ensure!(!text.contains("\\\n"), "Invalid command");
    let mut protected = String::new();
    let mut quote = None;
    let mut escaped = false;
    for c in text.chars() {
        if escaped {
            protected.push(c);
            escaped = false;
            continue;
        }
        match quote {
            Some('\'') => {
                if c == '\'' {
                    quote = None;
                }
            }
            Some('"') => {
                if c == '"' {
                    quote = None;
                } else if c == '\\' {
                    escaped = true;
                }
            }
            None => match c {
                '#' => protected.push('\\'),
                '\'' | '"' => quote = Some(c),
                '\\' => escaped = true,
                _ => (),
            },
            _ => unreachable!(),
        }
        protected.push(c);
    }
    shell_words::split(&protected).context("Invalid command")
}

#[cfg(test)]
mod parsing_tests {
    use super::*;
    #[test]
    fn hash_characters_are_data_instead_of_comments() {
        for (text, expected) in [
            (
                "git-upload-pack /dev/project # extra",
                vec!["git-upload-pack", "/dev/project", "#", "extra"],
            ),
            (
                "git-upload-pack '/dev/hash#name'",
                vec!["git-upload-pack", "/dev/hash#name"],
            ),
            (
                "git-upload-pack \"/dev/hash#name\"",
                vec!["git-upload-pack", "/dev/hash#name"],
            ),
            (
                "git-upload-pack /dev/hash\\#name",
                vec!["git-upload-pack", "/dev/hash#name"],
            ),
            (
                "git-upload-pack /dev/hash#name",
                vec!["git-upload-pack", "/dev/hash#name"],
            ),
        ] {
            assert_eq!(request_words(text).unwrap(), expected);
        }
        assert!(request_words("git-upload\\\n-pack /dev/project").is_err());
    }
}

fn copy_directory(source: &Path, target: &Path) -> Result<()> {
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let destination = target.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            let resolved = entry.path().canonicalize()?;
            if resolved.is_dir() {
                copy_directory(&resolved, &destination)?;
            } else {
                fs::copy(resolved, destination)?;
            }
        } else if kind.is_dir() {
            copy_directory(&entry.path(), &destination)?;
        } else {
            fs::copy(entry.path(), destination)?;
        }
    }
    Ok(())
}
