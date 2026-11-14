//! Operation events publish locally; peer hints only wake a staging fetch worker.
use crate::{private_dir, process, received_signal, write_atomic, RepositoryId, Sync};
use anyhow::{ensure, Context, Result};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    os::fd::AsRawFd,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Hint {
    repository: RepositoryId,
    token: String,
}
fn key(id: &RepositoryId) -> String {
    id.as_str().bytes().map(|b| format!("{b:02x}")).collect()
}
fn read(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
impl Sync {
    fn events(&self) -> PathBuf {
        self.config.state.join("notifications")
    }
    fn event_setup(&self) -> Result<()> {
        for name in ["incoming", "outgoing", "received", "published"] {
            private_dir(&self.events().join(name))?;
        }
        Ok(())
    }
    fn event_lock(&self) -> Result<File> {
        private_dir(&self.events())?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(self.events().join("lock"))?;
        ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0,
            "Locking notification queue"
        );
        Ok(file)
    }
    /// Publish before queueing a hint. The short publication lock is independent
    /// of the staging/apply lock, so a slow fetch cannot delay publication.
    pub fn publish(&self) -> Result<usize> {
        self.setup()?;
        self.event_setup()?;
        let _publication = self
            .named_lock("publication.lock", false)?
            .context("Publication lock unavailable")?;
        let mut changed = 0;
        for (id, repo) in self.discover()? {
            if !repo.jj {
                continue;
            }
            self.snapshot(&repo.path, false)?;
            // Read the exported Git refs, rather than rereading live JJ tips.
            // If an operation finishes during export, a later export must still
            // count as a new publication. Workspace markers are publication roots.
            let prefix = format!("refs/heads/hosts/{}/work/", self.config.host);
            let mut command = self.command(&self.tools.git);
            command.arg("--git-dir").arg(self.git_dir(&repo)?).args([
                "for-each-ref",
                "--format=%(refname)%09%(objectname)",
                "refs/heads",
                "refs/tags",
            ]);
            let refs = process::output(command, None, false)?;
            let fingerprint = refs
                .lines()
                .filter(|line| !line.starts_with("refs/heads/hosts/") || line.starts_with(&prefix))
                .collect::<Vec<_>>()
                .join("\n");
            let published = self
                .events()
                .join("published")
                .join(format!("{}.json", key(&id)));
            let _queue = self.event_lock()?;
            if read(&published)?.as_deref() == Some(&fingerprint) {
                continue;
            }
            let hint = Hint {
                repository: id.clone(),
                token: format!(
                    "{}-{}",
                    std::process::id(),
                    chrono::Utc::now()
                        .timestamp_nanos_opt()
                        .context("Clock out of range")?
                ),
            };
            let text = serde_json::to_string(&hint)?;
            // Persist every peer's hint before recording the baseline. Crash
            // recovery may resend a hint, but cannot lose a new publication.
            for peer in self.config.peers.keys() {
                write_atomic(
                    &self
                        .events()
                        .join("outgoing")
                        .join(format!("{peer}-{}.json", key(&id))),
                    &text,
                )?;
            }
            write_atomic(&published, &fingerprint)?;
            changed += 1;
        }
        Ok(changed)
    }
    pub(crate) fn receive_notification(&self, repository: &str, token: &str) -> Result<()> {
        let id = RepositoryId::try_from(repository.to_owned())?;
        ensure!(
            self.config.repositories.contains(&id),
            "Repository is not allowlisted"
        );
        ensure!(
            !token.is_empty()
                && token.len() <= 96
                && token
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "Invalid notification token"
        );
        self.event_setup()?;
        let _queue = self.event_lock()?;
        let hint = Hint {
            repository: id.clone(),
            token: token.into(),
        };
        let text = serde_json::to_string(&hint)?;
        let done = self
            .events()
            .join("received")
            .join(format!("{}.json", key(&id)));
        if read(&done)?.as_deref() != Some(&text) {
            write_atomic(
                &self
                    .events()
                    .join("incoming")
                    .join(format!("{}.json", key(&id))),
                &text,
            )?;
        }
        Ok(())
    }
    /// Send queued availability hints, using only the paired restricted endpoint.
    /// Network I/O happens outside queue and publication locks.
    pub fn flush_notifications(&self) -> Result<()> {
        self.event_setup()?;
        let mut errors = Vec::new();
        for peer in self.config.peers.keys() {
            if !self
                .config
                .state
                .join("peers")
                .join(format!("{peer}.pub"))
                .exists()
            {
                continue;
            }
            for id in &self.config.repositories {
                let path = self
                    .events()
                    .join("outgoing")
                    .join(format!("{peer}-{}.json", key(id)));
                let Some(text) = read(&path)? else {
                    continue;
                };
                let hint: Hint = serde_json::from_str(&text)?;
                let result = (|| -> Result<()> {
                    let mut command = self.command(&self.tools.ssh);
                    command
                        .arg("-F")
                        .arg(&self.config.ssh_config)
                        .arg(peer)
                        .arg(shell_words::join([
                            "jj-sync-notify",
                            hint.repository.as_str(),
                            &hint.token,
                        ]));
                    ensure!(
                        process::output(command, None, false)? == "queued",
                        "Peer did not acknowledge notification"
                    );
                    let _queue = self.event_lock()?;
                    if read(&path)?.as_deref() == Some(&text) {
                        fs::remove_file(&path)?;
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    errors.push(format!("{peer}/{}: {error:#}", id.as_str()));
                }
            }
        }
        ensure!(
            errors.is_empty(),
            "Notification delivery failed: {}",
            errors.join("; ")
        );
        Ok(())
    }
    /// Fetch accepted hints into staging. No local snapshot or application occurs.
    /// Successful hints are acknowledged only if no newer hint replaced them.
    pub fn refresh_notifications(&self) -> Result<()> {
        self.event_setup()?;
        let mut incoming = BTreeMap::new();
        for id in &self.config.repositories {
            let path = self
                .events()
                .join("incoming")
                .join(format!("{}.json", key(id)));
            if let Some(text) = read(&path)? {
                incoming.insert(id.clone(), (path, text));
            }
        }
        if incoming.is_empty() {
            return Ok(());
        }
        let Some(_staging) = self.named_lock("lock", true)? else {
            return Ok(());
        };
        let ids = incoming.keys().cloned().collect();
        let mut status = self.read_status()?;
        let local = self.discover()?;
        for peer in self.config.peers.keys() {
            if let Err(error) = self.sync_peer_repositories(peer, &local, &mut status, Some(&ids)) {
                status.entry(peer.clone()).or_default().error = Some(format!("{error:#}"));
            }
        }
        write_atomic(
            &self.config.state.join("status.json"),
            &(serde_json::to_string_pretty(&status)? + "\n"),
        )?;
        let _queue = self.event_lock()?;
        for (id, (path, text)) in incoming {
            let failed = self.config.peers.keys().any(|peer| {
                status.get(peer).is_some_and(|s| s.error.is_some())
                    || status
                        .get(&format!("{peer}/{}", id.as_str()))
                        .is_some_and(|s| s.error.is_some())
            });
            if !failed && read(&path)?.as_deref() == Some(&text) {
                write_atomic(
                    &self
                        .events()
                        .join("received")
                        .join(format!("{}.json", key(&id))),
                    &text,
                )?;
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }
    pub(crate) fn named_lock(&self, name: &str, nonblocking: bool) -> Result<Option<File>> {
        private_dir(&self.config.state)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(self.config.state.join(name))?;
        loop {
            ensure!(received_signal() == 0, "Interrupted");
            let flags = libc::LOCK_EX | if nonblocking { libc::LOCK_NB } else { 0 };
            if unsafe { libc::flock(file.as_raw_fd(), flags) } == 0 {
                return Ok(Some(file));
            }
            let error = std::io::Error::last_os_error();
            if nonblocking && error.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error.into());
            }
        }
    }
    fn operation_paths(&self) -> Result<BTreeSet<PathBuf>> {
        self.discover()?
            .into_values()
            .filter(|r| r.jj)
            .map(|repo| {
                let path = repo.path.join(".jj/repo");
                let path = if path.is_file() {
                    repo.path.join(".jj").join(fs::read_to_string(path)?.trim())
                } else {
                    path
                };
                Ok(path.canonicalize()?.join("op_heads/heads"))
            })
            .collect()
    }
    /// Watch only live operation heads and notification queues, never staging.
    pub fn watch(&self, interval: Duration, debounce: Duration) -> Result<()> {
        self.watch_until(interval, debounce, &AtomicBool::new(false))
    }
    /// Run the watcher until shutdown is requested by the caller or a signal.
    pub fn watch_until(
        &self,
        interval: Duration,
        debounce: Duration,
        stop: &AtomicBool,
    ) -> Result<()> {
        ensure!(
            !interval.is_zero() && !debounce.is_zero(),
            "Watch intervals must be positive"
        );
        let _singleton = self
            .named_lock("watch.lock", true)?
            .context("A watcher is already running")?;
        self.setup()?;
        self.event_setup()?;
        let (events_tx, events_rx) = mpsc::channel();
        let mut watcher: RecommendedWatcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if !matches!(&event, Ok(event) if matches!(event.kind, EventKind::Access(_))) {
                    let _ = events_tx.send(event);
                }
            })?;
        let incoming = self.events().join("incoming");
        let outgoing = self.events().join("outgoing");
        watcher.watch(&incoming, RecursiveMode::NonRecursive)?;
        watcher.watch(&outgoing, RecursiveMode::NonRecursive)?;
        let mut watched = BTreeSet::new();

        thread::scope(|scope| -> Result<()> {
            let (send_tx, send_rx) = mpsc::sync_channel(1);
            let (fetch_tx, fetch_rx) = mpsc::sync_channel(1);
            scope.spawn(|| self.event_worker(stop, send_rx, || self.flush_notifications()));
            scope.spawn(|| self.event_worker(stop, fetch_rx, || self.refresh_notifications()));
            let result = (|| -> Result<()> {
                let mut audit = Instant::now();
                let mut dirty = Some((Instant::now(), Instant::now()));
                let mut first = true;
                while !stop.load(Ordering::Relaxed) && received_signal() == 0 {
                    if first || audit.elapsed() >= interval {
                        let paths = self.operation_paths()?;
                        for old in watched.difference(&paths) {
                            let _ = watcher.unwatch(old);
                        }
                        for new in paths.difference(&watched) {
                            watcher.watch(new, RecursiveMode::NonRecursive)?;
                        }
                        watched = paths;
                        dirty = Some((Instant::now() - debounce, Instant::now() - debounce));
                        audit = Instant::now();
                        first = false;
                        let _ = send_tx.try_send(());
                        let _ = fetch_tx.try_send(());
                    }
                    match events_rx.recv_timeout(Duration::from_millis(50)) {
                        Ok(Ok(event)) => {
                            if event.paths.iter().any(|p| p.starts_with(&incoming)) {
                                let _ = fetch_tx.try_send(());
                            }
                            if event.paths.iter().any(|p| p.starts_with(&outgoing)) {
                                let _ = send_tx.try_send(());
                            }
                            if event
                                .paths
                                .iter()
                                .any(|p| watched.iter().any(|root| p.starts_with(root)))
                            {
                                let now = Instant::now();
                                dirty = Some((dirty.map_or(now, |(start, _)| start), now));
                            }
                        }
                        Ok(Err(error)) => {
                            eprintln!("jj-sync watch: {error}; auditing repositories");
                            dirty = Some((Instant::now() - debounce, Instant::now() - debounce));
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => (),
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            anyhow::bail!("Filesystem watcher disconnected")
                        }
                    }
                    if dirty.is_some_and(|(start, last)| {
                        last.elapsed() >= debounce || start.elapsed() >= Duration::from_secs(1)
                    }) {
                        match self.publish() {
                            Ok(_) => {
                                let _ = send_tx.try_send(());
                            }
                            Err(error) => eprintln!("jj-sync publication: {error:#}"),
                        }
                        dirty = None;
                    }
                }
                Ok(())
            })();
            stop.store(true, Ordering::Relaxed);
            let _ = send_tx.try_send(());
            let _ = fetch_tx.try_send(());
            result
        })
    }
    fn event_worker(
        &self,
        stop: &AtomicBool,
        rx: mpsc::Receiver<()>,
        action: impl Fn() -> Result<()>,
    ) {
        let mut retry = Instant::now() - Duration::from_secs(10);
        while !stop.load(Ordering::Relaxed) && received_signal() == 0 {
            let requested = rx.recv_timeout(Duration::from_millis(100)).is_ok();
            if requested || retry.elapsed() >= Duration::from_secs(10) {
                while rx.try_recv().is_ok() {}
                if let Err(error) = action() {
                    eprintln!("jj-sync notification worker: {error:#}");
                }
                retry = Instant::now();
            }
        }
    }
}
