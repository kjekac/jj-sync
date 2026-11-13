mod process;
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
            jj: locate(option_env!("JJ_REPO_SYNC_JJ"), "jj"),
            git: locate(option_env!("JJ_REPO_SYNC_GIT"), "git"),
            ssh: locate(option_env!("JJ_REPO_SYNC_SSH"), "ssh"),
            keygen: locate(option_env!("JJ_REPO_SYNC_KEYGEN"), "ssh-keygen"),
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
                .arg(format!("jj-repo-sync@{}", self.config.host))
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
        if original == "jj-repo-sync-list" {
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
    pub fn sync_peer(
        &self,
        peer: &str,
        local: &BTreeMap<RepositoryId, Repository>,
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
            .args([peer, "jj-repo-sync-list"]);
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
                let target = if let Some(repo) = local.get(id).filter(|repo| repo.jj) {
                    repo.path.clone()
                } else {
                    let target = self.config.state.join("repos").join(peer).join(&id.0);
                    if !target.join(".jj").exists() {
                        fs::create_dir_all(target.parent().unwrap())?;
                        let mut command = self.command(&self.tools.jj);
                        command.args(["git", "init", "--no-colocate"]).arg(&target);
                        process::output(command, None, false)?;
                    }
                    target
                };
                const ENCODE: &AsciiSet = &NON_ALPHANUMERIC
                    .remove(b'/')
                    .remove(b'-')
                    .remove(b'_')
                    .remove(b'.')
                    .remove(b'~');
                let url = format!("ssh://{peer}/{}", utf8_percent_encode(&id.0, ENCODE));
                self.fetch(&target, &format!("sync-{peer}"), &url, true, false)?;
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
