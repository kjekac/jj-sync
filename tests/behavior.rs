use jj_sync::{write_atomic, Config, Repository, RepositoryId, Status, Sync, Tools};
use serde_json::json;
use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, MutexGuard},
};

static ENVIRONMENT: Mutex<()> = Mutex::new(());
struct Fixture {
    root: tempfile::TempDir,
    config: Config,
    old_environment: Vec<(&'static str, Option<std::ffi::OsString>)>,
    _guard: MutexGuard<'static, ()>,
}
impl Fixture {
    fn new() -> Self {
        let guard = ENVIRONMENT
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = tempfile::tempdir().unwrap();
        let path = root.path();
        fs::create_dir(path.join("dev")).unwrap();
        let jj_config = path.join("jj.toml");
        fs::write(&jj_config, "user.name = \"Sync test\"\nuser.email = \"test@example.org\"\n[revset-aliases]\n\"immutable_heads()\" = \"tags() | remote_tags(remote=glob:sync-*)\"\n").unwrap();
        let names = [
            "JJ_CONFIG",
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_CACHE_HOME",
            "XDG_DATA_HOME",
            "GIT_SSH_VARIANT",
        ];
        let old_environment = names
            .into_iter()
            .map(|name| (name, std::env::var_os(name)))
            .collect();
        std::env::set_var("HOME", path);
        std::env::set_var("XDG_CONFIG_HOME", path.join("config"));
        std::env::set_var("XDG_CACHE_HOME", path.join("cache"));
        std::env::set_var("XDG_DATA_HOME", path.join("data"));
        std::env::set_var("JJ_CONFIG", jj_config);
        let config = Config::from_json(
            &json!({
                "host": "thastrom", "home": path, "state": path.join("state"),
                "repositoryRoots": {"dev": path.join("dev")},
                "repositories": ["dev/project", "dev/extra", "dev/jj", "dev/git"],
                "peers": {"remover": {"address": "localhost", "sshAlias": "remover"}},
                "sshConfig": path.join("ssh.conf"), "remoteCommand": "/bin/jj-sync"
            })
            .to_string(),
        )
        .unwrap();
        Self {
            root,
            config,
            old_environment,
            _guard: guard,
        }
    }
    fn sync(&self) -> Sync {
        Sync::new(self.config.clone())
    }
    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }
    fn repo(&self, name: &str, colocate: bool) -> PathBuf {
        let path = self.path(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let result = Command::new(Tools::default().jj)
            .args([
                "git",
                "init",
                if colocate {
                    "--colocate"
                } else {
                    "--no-colocate"
                },
            ])
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        fs::write(path.join("file"), "initial\n").unwrap();
        self.jj(&path, &["describe", "-m", "Initial work"], true);
        path
    }
    fn jj(&self, path: &Path, args: &[&str], snapshot: bool) -> String {
        self.sync().jj(path, args, snapshot, false, false).unwrap()
    }
    fn commit(&self, path: &Path, revision: &str) -> String {
        self.jj(
            path,
            &["log", "-r", revision, "--no-graph", "-T", "commit_id"],
            false,
        )
    }
    fn url(&self, path: &Path) -> String {
        self.jj(path, &["git", "root"], false)
    }
    fn script(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.path(name);
        fs::write(
            &path,
            contents.replacen(
                "#!/bin/sh",
                &format!("#!{}", option_env!("JJ_SYNC_SHELL").unwrap_or("/bin/sh")),
                1,
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for (name, value) in &self.old_environment {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}
fn id(value: &str) -> RepositoryId {
    value.to_owned().try_into().unwrap()
}
fn quote(path: &Path) -> String {
    shell_words::quote(path.to_str().unwrap()).into_owned()
}

#[test]
fn configuration_and_ids_reject_traversal_and_legacy_roots() {
    let fixture = Fixture::new();
    for value in [
        "",
        "../secret",
        "dev/../secret",
        "/dev/project",
        "dev//project",
        "dev/./project",
        "dev/project\n",
    ] {
        assert!(
            RepositoryId::try_from(value.to_owned()).is_err(),
            "{value:?}"
        );
    }
    assert!(RepositoryId::try_from("dev/a space;quote'".to_owned()).is_ok());
    let mut config = serde_json::to_value(&fixture.config).unwrap();
    config["roots"] = json!({});
    assert!(Config::from_json(&config.to_string())
        .unwrap_err()
        .to_string()
        .contains("Legacy"));
    config.as_object_mut().unwrap().remove("roots");
    config["repositories"] = json!("dev/project");
    assert!(Config::from_json(&config.to_string()).is_err());
    config["repositories"] = json!([]);
    config["host"] = json!("../../evil");
    assert!(Config::from_json(&config.to_string()).is_err());
}

#[test]
fn snapshots_preserve_parked_work_are_idempotent_and_cleanup_only_own_refs() {
    let fixture = Fixture::new();
    let sync = fixture.sync();
    for colocate in [false, true] {
        let path = fixture.repo(if colocate { "colocated" } else { "separate" }, colocate);
        fixture.jj(&path, &["bookmark", "set", "feature"], false);
        let first = fixture.commit(&path, "@");
        fixture.jj(&path, &["new", "-m", "Parked work"], true);
        fs::write(path.join("parked"), "parked").unwrap();
        fixture.jj(&path, &["bookmark", "set", "parked"], true);
        let parked = fixture.commit(&path, "@");
        fixture.jj(&path, &["new", &first, "-m", "Active work"], true);
        fixture.jj(&path, &["bookmark", "set", "hosts/other/keep"], false);
        fixture.jj(
            &path,
            &["bookmark", "set", "hosts/thastrom/peers/obsolete"],
            false,
        );
        fs::write(path.join("file"), "unfinished edit\n").unwrap();
        sync.snapshot(&path, false).unwrap();
        assert_eq!(
            fixture.commit(&path, "hosts/thastrom/work/default"),
            fixture.commit(&path, "@")
        );
        assert_eq!(fixture.commit(&path, "feature"), first);
        assert_eq!(fixture.commit(&path, "parked"), parked);
        assert!(fixture
            .jj(
                &path,
                &["bookmark", "list", "glob:hosts/thastrom/bookmarks/*"],
                false
            )
            .is_empty());
        assert!(fixture
            .jj(
                &path,
                &["bookmark", "list", "glob:hosts/thastrom/peers/*"],
                false
            )
            .is_empty());
        assert!(!fixture
            .jj(&path, &["bookmark", "list", "hosts/other/keep"], false)
            .is_empty());
        let operation = fixture.jj(
            &path,
            &["op", "log", "--limit", "1", "--no-graph", "-T", "id"],
            false,
        );
        sync.snapshot(&path, false).unwrap();
        assert_eq!(
            operation,
            fixture.jj(
                &path,
                &["op", "log", "--limit", "1", "--no-graph", "-T", "id"],
                false
            )
        );
        assert_eq!(
            fs::read_to_string(path.join("file")).unwrap(),
            "unfinished edit\n"
        );
    }
}

#[test]
fn native_fetch_rebases_children_and_deployment_tags_freeze_history() {
    let fixture = Fixture::new();
    let sync = fixture.sync();
    let source = fixture.repo("dev/project", false);
    let target = fixture.repo("target", false);
    sync.snapshot(&source, false).unwrap();
    let old = fixture.commit(&source, "@");
    let url = fixture.url(&source);
    sync.fetch(&target, "sync-remover", &url, false, false)
        .unwrap();
    fixture.jj(&target, &["new", &old, "-m", "Local child"], true);
    fs::write(target.join("local"), "keep this").unwrap();
    fixture.jj(&target, &["status"], true);
    let child = fixture.commit(&target, "@");
    fs::write(source.join("file"), "rewritten source").unwrap();
    sync.snapshot(&source, false).unwrap();
    let new = fixture.commit(&source, "@");
    assert_ne!(new, old);
    sync.fetch(&target, "sync-remover", &url, false, false)
        .unwrap();
    assert_ne!(fixture.commit(&target, "@"), child);
    assert_eq!(fixture.commit(&target, "@-"), new);
    assert_eq!(
        fixture.jj(
            &target,
            &["log", "-r", &new, "--no-graph", "-T", "immutable"],
            false
        ),
        "false"
    );
    fixture.jj(
        &source,
        &["tag", "set", "nixos/Thastrom/123", "-r", &new],
        false,
    );
    fixture.jj(&source, &["git", "export"], false);
    sync.fetch(&target, "sync-remover", &url, false, false)
        .unwrap();
    assert_eq!(
        fixture.jj(
            &target,
            &["log", "-r", &new, "--no-graph", "-T", "immutable"],
            false
        ),
        "true"
    );
    assert_eq!(
        fs::read_to_string(target.join("local")).unwrap(),
        "keep this"
    );
}

#[test]
fn offline_rewrites_move_active_workspace_and_independent_edits_diverge() {
    let fixture = Fixture::new();
    let sync = fixture.sync();
    for independent in [false, true] {
        let source = fixture.repo(if independent { "writer-b" } else { "writer-a" }, false);
        let target = fixture.repo(if independent { "reader-b" } else { "reader-a" }, false);
        sync.snapshot(&source, false).unwrap();
        let old = fixture.commit(&source, "@");
        let url = fixture.url(&source);
        sync.fetch(&target, "sync-remover", &url, false, false)
            .unwrap();
        fixture.jj(&target, &["edit", &old], true);
        fixture.jj(&target, &["bookmark", "set", "local-name"], false);
        if independent {
            fs::write(target.join("file"), "independent edit").unwrap();
            fixture.jj(&target, &["status"], true);
        }
        let local = fixture.commit(&target, "@");
        for index in 0..3 {
            fs::write(source.join("file"), format!("replacement {index}")).unwrap();
            sync.snapshot(&source, false).unwrap();
        }
        sync.fetch(&target, "sync-remover", &url, false, false)
            .unwrap();
        let expected = if independent {
            local
        } else {
            fixture.commit(&source, "@")
        };
        assert_eq!(fixture.commit(&target, "@"), expected);
        assert_eq!(fixture.commit(&target, "local-name"), expected);
        assert_eq!(
            fs::read_to_string(target.join("file")).unwrap(),
            if independent {
                "independent edit"
            } else {
                "replacement 2"
            }
        );
    }
}

#[test]
fn repeated_fetches_apply_abandonment_and_keep_split_parts() {
    let fixture = Fixture::new();
    let sync = fixture.sync();
    for mode in ["abandon", "serial", "parallel", "onto"] {
        let source = fixture.repo(&format!("source-{mode}"), false);
        let target = fixture.repo(&format!("target-{mode}"), false);
        fs::write(source.join("other"), "second part").unwrap();
        sync.snapshot(&source, false).unwrap();
        let old = fixture.commit(&source, "@");
        let url = fixture.url(&source);
        sync.fetch(&target, "sync-remover", &url, false, false)
            .unwrap();
        fixture.jj(&target, &["new", &old], true);
        fs::write(target.join("local"), "reader work").unwrap();
        let mut parts = Vec::new();
        if mode == "abandon" {
            fixture.jj(&source, &["new", "root()"], true);
            fixture.jj(&source, &["abandon", &old], false);
        } else {
            let mut args = vec!["split", "-r", &old, "-m", "First part"];
            match mode {
                "parallel" => args.push("--parallel"),
                "onto" => args.extend(["--onto", "root()"]),
                _ => (),
            }
            args.push("root:file");
            fixture.jj(&source, &args, true);
            parts = fixture
                .jj(
                    &source,
                    &[
                        "log",
                        "--no-graph",
                        "-r",
                        "heads(all())",
                        "-T",
                        "commit_id ++ \"\\n\"",
                    ],
                    false,
                )
                .lines()
                .map(str::to_owned)
                .collect();
            for (index, part) in parts.iter().enumerate() {
                fixture.jj(
                    &source,
                    &["bookmark", "set", &format!("part-{index}"), "-r", part],
                    false,
                );
            }
        }
        sync.snapshot(&source, false).unwrap();
        for _ in 0..3 {
            sync.fetch(&target, "sync-remover", &url, false, false)
                .unwrap();
            assert_eq!(
                fs::read_to_string(target.join("local")).unwrap(),
                "reader work"
            );
            if mode == "abandon" {
                assert_eq!(
                    fixture.commit(&target, "@-"),
                    fixture.commit(&target, "root()")
                );
                assert!(!target.join("file").exists());
            }
            for (index, part) in parts.iter().enumerate() {
                assert_eq!(
                    &fixture.commit(&target, &format!("part-{index}@sync-remover")),
                    part
                );
            }
        }
    }
}

#[test]
fn allowlist_defaults_empty_and_discovery_does_not_touch_unlisted_work() {
    let fixture = Fixture::new();
    let path = fixture.repo("dev/project", false);
    let blocked = fixture.repo("dev/private", false);
    let before = fixture.commit(&blocked, "@");
    fs::write(blocked.join("file"), "unrecorded edits").unwrap();
    let mut sync = fixture.sync();
    sync.config.peers.clear();
    assert_eq!(sync.discover().unwrap().len(), 1);
    sync.cycle().unwrap();
    assert_eq!(fixture.commit(&blocked, "@"), before);
    assert!(sync.serve("git-upload-pack /dev/private").is_err());
    let mut config = serde_json::to_value(&sync.config).unwrap();
    config.as_object_mut().unwrap().remove("repositories");
    let empty = Sync::new(Config::from_json(&config.to_string()).unwrap());
    fs::write(path.join("file"), "unrecorded").unwrap();
    let original = fixture.commit(&path, "@");
    empty.cycle().unwrap();
    assert!(empty.discover().unwrap().is_empty());
    assert_eq!(fixture.commit(&path, "@"), original);
}

#[test]
fn discovery_handles_ignore_files_plain_git_and_symlink_escape() {
    let fixture = Fixture::new();
    let mut sync = fixture.sync();
    sync.config
        .repository_roots
        .insert("unmounted".into(), fixture.path("missing-volume"));
    fixture.repo("dev/jj", false);
    let git = fixture.repo("dev/git", true);
    fs::rename(git.join(".jj"), fixture.path("removed-jj")).unwrap();
    let ignored = fixture.repo("dev/project", false);
    fs::write(ignored.join(".jj-sync-ignore"), "").unwrap();
    let outside = fixture.repo("outside", false);
    symlink(outside, fixture.path("dev/extra")).unwrap();
    let catalog = sync.catalog().unwrap();
    assert_eq!(catalog.len(), 2);
    assert!(!catalog[&id("dev/git")].jj);
    assert!(catalog[&id("dev/jj")].jj);
    assert!(sync
        .git_dir(&Repository {
            path: git.clone(),
            jj: false
        })
        .is_ok());
    fs::rename(git.join(".git"), fixture.path("outside-git")).unwrap();
    symlink(fixture.path("outside-git"), git.join(".git")).unwrap();
    assert!(sync
        .git_dir(&Repository {
            path: git,
            jj: false
        })
        .unwrap_err()
        .to_string()
        .contains("outside"));
}

#[test]
fn restricted_server_rejects_shells_writes_traversal_and_extra_arguments() {
    let fixture = Fixture::new();
    let sync = fixture.sync();
    for command in [
        "",
        "bash",
        "git-receive-pack '/dev/project'",
        "jj-sync-list; id",
        "git-upload-pack /dev/project # ignored extra arguments",
        "git-upload-pack '/dev/../secret'",
        "git-upload-pack /etc",
        "git-upload-pack '/dev/project'; id",
        "git-upload-pack --help /dev/project",
        "git-upload-pack '/dev/project' extra",
        "git-upload-pack '",
    ] {
        assert!(sync.serve(command).is_err(), "{command}");
    }
}

#[test]
fn pairing_validates_ed25519_refuses_replacement_and_revokes() {
    let fixture = Fixture::new();
    let sync = fixture.sync();
    let key = sync.public_key().unwrap();
    sync.accept_key("remover", &key).unwrap();
    sync.accept_key("remover", &key).unwrap();
    assert_eq!(
        fs::read_to_string(sync.config.state.join("authorized_keys")).unwrap(),
        format!("restrict {key} remover\n")
    );
    let mut other = fixture.sync();
    other.config.state = fixture.path("other-state");
    assert!(sync
        .accept_key("remover", &other.public_key().unwrap())
        .unwrap_err()
        .to_string()
        .contains("Key changed"));
    for invalid in [
        format!("{key}\n{key}"),
        format!("command=\"sh\" {key}"),
        "ssh-ed25519 bad".into(),
        String::new(),
    ] {
        assert!(sync.accept_key("remover", &invalid).is_err());
    }
    assert!(sync.accept_key("../../evil", &key).is_err());
    sync.revoke("remover").unwrap();
    sync.revoke("remover").unwrap();
    assert_eq!(
        fs::read_to_string(sync.config.state.join("authorized_keys")).unwrap(),
        ""
    );
}

#[test]
fn tracked_refs_and_remote_url_replacement_are_refused() {
    let fixture = Fixture::new();
    let sync = fixture.sync();
    let source = fixture.repo("source", false);
    let target = fixture.repo("target", false);
    sync.snapshot(&source, false).unwrap();
    let url = fixture.url(&source);
    sync.fetch(&target, "sync-remover", &url, false, false)
        .unwrap();
    fixture.jj(
        &target,
        &[
            "bookmark",
            "track",
            "hosts/thastrom/work/default@sync-remover",
        ],
        false,
    );
    assert!(sync
        .fetch(&target, "sync-remover", &url, false, false)
        .unwrap_err()
        .to_string()
        .contains("Untrack"));
    assert!(sync
        .ensure_remote(&target, "sync-remover", "unrelated")
        .unwrap_err()
        .to_string()
        .contains("different URL"));
}

#[test]
fn post_fetch_hook_sees_imported_refs_and_failure_is_reported() {
    let fixture = Fixture::new();
    let mut sync = fixture.sync();
    let source = fixture.repo("source", false);
    let target = fixture.repo("target", false);
    let marker = fixture.path("observed");
    let hook = fixture.script("hook", &format!("#!/bin/sh\n{} -R \"$1\" --ignore-working-copy log -r hosts/thastrom/work/default@sync-remover --no-graph -T commit_id > {}\n", quote(&sync.tools.jj), quote(&marker)));
    sync.config.post_fetch_command = Some(hook.clone());
    sync.snapshot(&source, false).unwrap();
    let url = fixture.url(&source);
    sync.fetch(&target, "sync-remover", &url, false, false)
        .unwrap();
    assert_eq!(
        fs::read_to_string(marker).unwrap(),
        fixture.commit(&source, "@")
    );
    fs::write(
        hook,
        format!(
            "#!{}\necho 'archive failed' >&2\nexit 1\n",
            option_env!("JJ_SYNC_SHELL").unwrap_or("/bin/sh")
        ),
    )
    .unwrap();
    assert!(sync
        .fetch(&target, "sync-remover", &url, false, false)
        .unwrap_err()
        .to_string()
        .contains("archive failed"));
}

fn transport(fixture: &Fixture, remote_config: &Config) -> PathBuf {
    let config_file = fixture.path("remote.json");
    fs::write(&config_file, serde_json::to_string(remote_config).unwrap()).unwrap();
    fixture.script("fake-ssh", &format!("#!/bin/sh\nfor last do :; done\nexport SSH_ORIGINAL_COMMAND=\"$last\"\nexec {} --config {} serve\n", shell_words::quote(env!("CARGO_BIN_EXE_jj-sync")), quote(&config_file)))
}

#[test]
fn protocol_cycles_fetch_cache_filter_catalog_and_preserve_offline_state() {
    let fixture = Fixture::new();
    let mut sync = fixture.sync();
    let source = fixture.repo("remote-dev/project", false);
    let extra = fixture.repo("remote-dev/extra", false);
    fixture.repo("remote-dev/private", false);
    let target = fixture.repo("dev/project", false);
    let mut remote = fixture.sync();
    remote.config.host = "remover".into();
    remote.config.state = fixture.path("remote-state");
    remote
        .config
        .repository_roots
        .insert("dev".into(), fixture.path("remote-dev"));
    remote.config.repositories.insert(id("dev/private"));
    remote.snapshot(&source, false).unwrap();
    remote.snapshot(&extra, false).unwrap();
    let git_dir = remote
        .git_dir(&Repository {
            path: source.clone(),
            jj: true,
        })
        .unwrap();
    use std::io::Write;
    fs::OpenOptions::new()
        .append(true)
        .open(git_dir.join("config"))
        .unwrap()
        .write_all(b"\n[uploadpack]\npackObjectsHook = /must-not-execute\n")
        .unwrap();
    sync.tools.ssh = transport(&fixture, &remote.config);
    std::env::set_var("GIT_SSH_VARIANT", "ssh");
    sync.accept_key("remover", &remote.public_key().unwrap())
        .unwrap();
    write_atomic(
        &sync.config.state.join("status.json"),
        &json!({"remover/dev/private": {"error": "obsolete"}, "local/dev/gone": {"error": "gone"}})
            .to_string(),
    )
    .unwrap();
    let before = fixture.commit(&target, "@");
    sync.cycle().unwrap();
    let status: Status =
        serde_json::from_str(&fs::read_to_string(sync.config.state.join("status.json")).unwrap())
            .unwrap();
    assert!(status.values().all(|entry| entry.error.is_none()));
    assert!(!status.contains_key("remover/dev/private"));
    assert!(!status.contains_key("local/dev/gone"));
    assert_eq!(fixture.commit(&target, "@"), before);
    assert_eq!(sync.pending(&target).unwrap(), 1);
    let cli_config = fixture.path("cli-config.json");
    fs::write(&cli_config, serde_json::to_string(&sync.config).unwrap()).unwrap();
    let cli = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_jj-sync"))
            .arg("--config")
            .arg(&cli_config)
            .args(args)
            .arg("--path")
            .arg(&target)
            .output()
            .unwrap()
    };
    let counted = cli(&["pending", "--count"]);
    assert!(counted.status.success());
    assert_eq!(String::from_utf8(counted.stdout).unwrap().trim(), "1");
    let human = cli(&["pending", "--cached"]);
    assert!(human.status.success());
    assert!(String::from_utf8(human.stdout)
        .unwrap()
        .contains("remover: updates staged"));
    let machine = cli(&["pending", "--cached", "--json"]);
    assert!(machine.status.success());
    let machine: serde_json::Value = serde_json::from_slice(&machine.stdout).unwrap();
    assert_eq!(machine["peers"]["remover"]["pending"], true);
    let dry_run = cli(&["apply", "--dry-run", "--cached"]);
    assert!(
        dry_run.status.success(),
        "{}",
        String::from_utf8_lossy(&dry_run.stderr)
    );
    let output = String::from_utf8(dry_run.stdout).unwrap();
    assert!(output.contains("Resulting history"));
    assert!(!output.contains("elided revisions"));
    assert!(!output.contains("archive/nixos"));
    assert!(
        output.lines().count() < 40,
        "Dry-run must remain concise: {output}"
    );
    let legacy = cli(&["apply", "--preview", "--cached"]);
    assert!(legacy.status.success());
    // Interactive pending performs a refresh and reports a clear pairing error.
    let mut unpaired = sync.config.clone();
    unpaired.state = fixture.path("unpaired-state");
    fs::write(&cli_config, serde_json::to_string(&unpaired).unwrap()).unwrap();
    let refresh = cli(&["pending"]);
    assert!(refresh.status.success());
    assert!(String::from_utf8(refresh.stdout)
        .unwrap()
        .contains("Not paired"));
    assert!(unpaired.state.join("status.json").exists());
    fs::write(&cli_config, serde_json::to_string(&sync.config).unwrap()).unwrap();
    let before_operation = fixture.jj(
        &target,
        &["op", "log", "--limit", "1", "--no-graph", "-T", "id"],
        false,
    );
    sync.apply(&target, true).unwrap();
    assert_eq!(fixture.commit(&target, "@"), before);
    assert_eq!(
        fixture.jj(
            &target,
            &["op", "log", "--limit", "1", "--no-graph", "-T", "id"],
            false
        ),
        before_operation
    );
    fixture.jj(
        &target,
        &["describe", "-m", "Local change after preview"],
        true,
    );
    assert!(sync
        .apply(&target, false)
        .unwrap_err()
        .to_string()
        .contains("changed since preview"));
    sync.apply(&target, true).unwrap();
    sync.apply(&target, false).unwrap();
    assert_eq!(sync.pending(&target).unwrap(), 0);
    // Even an empty dry-run is a checkpoint, so later local edits invalidate it.
    sync.apply(&target, true).unwrap();
    fixture.jj(&target, &["describe", "-m", "Another local edit"], true);
    assert!(sync.apply(&target, false).is_err());
    sync.apply(&target, true).unwrap();
    sync.apply(&target, false).unwrap();
    assert_eq!(
        fixture.commit(&target, "hosts/remover/work/default@sync-remover"),
        fixture.commit(&source, "@")
    );
    let cached = sync.config.state.join("staging/remover/dev/extra");
    let revision = fixture.commit(&extra, "@");
    assert_eq!(
        fixture.commit(&cached, "hosts/remover/work/default@sync-remover"),
        revision
    );
    assert!(!fixture.path("dev/extra").exists());
    assert!(!sync
        .config
        .state
        .join("staging/remover/dev/private")
        .exists());
    fs::rename(extra, fixture.path("removed-extra")).unwrap();
    sync.cycle().unwrap();
    let status: Status =
        serde_json::from_str(&fs::read_to_string(sync.config.state.join("status.json")).unwrap())
            .unwrap();
    assert!(!status.contains_key("remover/dev/extra"));
    sync.tools.ssh = "/missing-ssh".into();
    assert!(sync.cycle().is_err());
    let offline: Status =
        serde_json::from_str(&fs::read_to_string(sync.config.state.join("status.json")).unwrap())
            .unwrap();
    assert_eq!(
        offline["remover"].last_success,
        status["remover"].last_success
    );
    assert!(offline["remover"].error.is_some());
    assert_eq!(
        fixture.commit(&cached, "hosts/remover/work/default@sync-remover"),
        revision
    );
    std::env::remove_var("GIT_SSH_VARIANT");
}

#[test]
fn cli_preserves_config_status_pairing_and_reports_cycle_failure() {
    let fixture = Fixture::new();
    let file = fixture.path("config.json");
    fs::write(&file, serde_json::to_string(&fixture.config).unwrap()).unwrap();
    let call = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_jj-sync"))
            .arg("--config")
            .arg(&file)
            .args(args)
            .output()
            .unwrap()
    };
    assert!(call(&["setup"]).status.success());
    assert!(String::from_utf8(call(&["public-key"]).stdout)
        .unwrap()
        .starts_with("ssh-ed25519 "));
    assert!(String::from_utf8(call(&["status"]).stdout)
        .unwrap()
        .contains("No sync cycle"));
    // Failure is persisted before exit; on Darwin the process stays alive for its permission alert.
    assert!(!call(&["cycle"]).status.success());
    let status: Status = serde_json::from_slice(&call(&["status"]).stdout).unwrap();
    assert_eq!(status["remover"].error.as_deref(), Some("Not paired"));
    assert!(!call(&["pair"]).status.success());
}

#[test]
fn two_host_cycles_preserve_independent_work_across_rebases_and_deployments() {
    let fixture = Fixture::new();
    for deployed in [false, true] {
        for dependent in [false, true] {
            let name = format!("case-{deployed}-{dependent}");
            let root = fixture.path(&name);
            let mut writer = fixture.sync();
            writer.config.state = root.join("writer-state");
            writer
                .config
                .repository_roots
                .insert("dev".into(), root.join("writer"));
            let mut reader = fixture.sync();
            reader.config.host = "remover".into();
            reader.config.state = root.join("reader-state");
            reader
                .config
                .repository_roots
                .insert("dev".into(), root.join("reader"));
            reader.config.peers = std::collections::BTreeMap::from([(
                "thastrom".into(),
                writer.config.peers["remover"].clone(),
            )]);
            let source = fixture.repo(&format!("{name}/writer/project"), false);
            let target = fixture.repo(&format!("{name}/reader/project"), false);
            let writer_config = root.join("writer.json");
            let reader_config = root.join("reader.json");
            fs::write(
                &writer_config,
                serde_json::to_string(&writer.config).unwrap(),
            )
            .unwrap();
            fs::write(
                &reader_config,
                serde_json::to_string(&reader.config).unwrap(),
            )
            .unwrap();
            let ssh = fixture.script(&format!("{name}/ssh"), &format!(
                "#!/bin/sh\nlast=\nprevious=\nfor arg do previous=$last; last=$arg; done\ncase \"$previous\" in thastrom) config={};; remover) config={};; *) exit 1;; esac\nexport SSH_ORIGINAL_COMMAND=\"$last\"\nexec {} --config \"$config\" serve\n",
                quote(&writer_config), quote(&reader_config), shell_words::quote(env!("CARGO_BIN_EXE_jj-sync"))));
            writer.tools.ssh = ssh.clone();
            reader.tools.ssh = ssh;
            std::env::set_var("GIT_SSH_VARIANT", "ssh");
            writer
                .accept_key("remover", &reader.public_key().unwrap())
                .unwrap();
            reader
                .accept_key("thastrom", &writer.public_key().unwrap())
                .unwrap();
            let base = fixture.commit(&source, "@");
            fixture.jj(&source, &["new", &base, "-m", "Timezone feature"], true);
            fs::write(source.join("timezone"), "correct timezone\n").unwrap();
            fixture.jj(&source, &["bookmark", "set", "timezone"], true);
            let old = fixture.commit(&source, "@");
            writer.cycle().unwrap();
            reader.cycle().unwrap();
            reader.apply(&target, false).unwrap();
            fixture.jj(
                &target,
                &[
                    "new",
                    if dependent { &old } else { &base },
                    "-m",
                    "Alacritty feature",
                ],
                true,
            );
            fs::write(target.join("alacritty"), "local network access\n").unwrap();
            fixture.jj(&target, &["bookmark", "set", "alacritty"], true);
            let feature = fixture.commit(&target, "@");
            let change = fixture.jj(
                &target,
                &["log", "-r", "@", "--no-graph", "-T", "change_id"],
                false,
            );
            reader.cycle().unwrap();
            writer.cycle().unwrap();
            if deployed {
                fixture.jj(
                    &target,
                    &["tag", "set", "nixos/Thastrom/123", "-r", &old],
                    false,
                );
            }
            fixture.jj(&source, &["new", &base, "-m", "Containment"], true);
            fs::write(source.join("containment"), "allowlist\n").unwrap();
            fixture.jj(&source, &["bookmark", "set", "containment"], true);
            let containment = fixture.commit(&source, "@");
            fixture.jj(&source, &["rebase", "-r", &old, "-d", &containment], false);
            let replacement = fixture.commit(&source, "timezone");
            assert_ne!(replacement, old);
            for _ in 0..3 {
                writer.cycle().unwrap();
                reader.cycle().unwrap();
                // Background staging cannot rebase the active feature.
                if fixture.commit(&target, "@") == feature {
                    assert_eq!(
                        fixture.commit(&target, "@-"),
                        if dependent { old.clone() } else { base.clone() }
                    );
                }
                reader.apply(&target, true).unwrap();
                reader.apply(&target, false).unwrap();
                assert_eq!(fixture.commit(&target, "@") != feature, dependent);
                assert_eq!(
                    fixture.jj(
                        &target,
                        &["log", "-r", "@", "--no-graph", "-T", "change_id"],
                        false
                    ),
                    change
                );
                if dependent {
                    assert_eq!(fixture.commit(&target, "@-"), replacement);
                }
                assert_eq!(
                    fs::read_to_string(target.join("alacritty")).unwrap(),
                    "local network access\n"
                );
                assert_eq!(
                    fixture.commit(&target, "timezone@sync-thastrom"),
                    replacement
                );
                if deployed {
                    assert_eq!(fixture.commit(&target, "nixos/Thastrom/123"), old);
                }
            }
            std::env::remove_var("GIT_SSH_VARIANT");
        }
    }
}

#[test]
fn github_backup_round_trips_and_refuses_unexpected_remote_updates() {
    let fixture = Fixture::new();
    let mut sync = fixture.sync();
    let source = fixture.repo("dev/project", false);
    let destination = fixture.repo("github", false);
    let url = fixture.url(&destination);
    fixture.jj(&source, &["git", "remote", "add", "origin", &url], false);
    fixture.jj(&source, &["bookmark", "set", "feature"], false);
    let accepted = fixture.commit(&source, "@");
    fixture.jj(&source, &["tag", "set", "nixos/Thastrom/7"], false);
    fixture.jj(&source, &["new", "-m", "Unfinished next version"], true);
    // Substitute only the network address; JJ still performs real fetches and
    // guarded pushes against the disposable destination.
    let python = Command::new("python3")
        .args(["-c", "import sys; print(sys.executable)"])
        .output()
        .unwrap();
    assert!(python.status.success());
    let calls = fixture.path("jj-calls.jsonl");
    let github_url = "git@github.com:owner/project.git";
    let wrapper = fixture.script("jj-address-wrapper", &format!(
        "#!{}\nimport json, os, subprocess, sys\nurl = {}\ngithub_url = {}\nargs = sys.argv[1:]\nwith open({}, 'a') as file:\n file.write(json.dumps([args, os.environ.get('GIT_SSH_COMMAND')]) + '\\n')\np = subprocess.run([{}, *[url if arg == github_url else arg for arg in args]], capture_output=True)\nstdout = p.stdout\nif args[-3:] == ['git', 'remote', 'list']:\n stdout = stdout.replace(url.encode(), github_url.encode())\nsys.stdout.buffer.write(stdout)\nsys.stderr.buffer.write(p.stderr)\nsys.exit(p.returncode)\n",
        String::from_utf8(python.stdout).unwrap().trim(), serde_json::to_string(&url).unwrap(),
        serde_json::to_string(github_url).unwrap(), serde_json::to_string(&calls).unwrap(), serde_json::to_string(&sync.tools.jj).unwrap()));
    sync.tools.jj = wrapper;
    sync.github().unwrap();
    fixture.jj(&destination, &["git", "import"], false);
    assert_eq!(
        fixture.commit(&destination, "hosts/thastrom/work/default"),
        fixture.commit(&source, "@")
    );
    assert_eq!(fixture.commit(&destination, "nixos/Thastrom/7"), accepted);
    let records: Vec<serde_json::Value> = fs::read_to_string(&calls)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let push = records
        .iter()
        .find(|record| {
            record[0]
                .as_array()
                .unwrap()
                .iter()
                .any(|arg| arg == "push")
        })
        .unwrap();
    assert!(push[0]
        .as_array()
        .unwrap()
        .iter()
        .any(|arg| arg == "glob:hosts/thastrom/*"));
    assert!(push[0].as_array().unwrap().iter().any(|arg| arg == "--tag"));
    assert!(!push[0].as_array().unwrap().iter().any(|arg| arg == "--all"));
    assert!(push[1].is_null());
    fs::write(source.join("file"), "newer unfinished work").unwrap();
    sync.github().unwrap();
    fixture.jj(&destination, &["git", "import"], false);
    assert_eq!(
        fixture.commit(&destination, "hosts/thastrom/work/default"),
        fixture.commit(&source, "@")
    );
    fixture.jj(
        &destination,
        &[
            "bookmark",
            "set",
            "hosts/thastrom/work/default",
            "-r",
            "@",
            "--allow-backwards",
        ],
        false,
    );
    fixture.jj(&destination, &["git", "export"], false);
    let foreign = fixture.commit(&destination, "@");
    fs::write(source.join("file"), "another local version").unwrap();
    assert!(sync.github().is_err());
    assert_eq!(
        fixture.commit(&destination, "hosts/thastrom/work/default"),
        foreign
    );
}

#[test]
fn stale_peer_marker_does_not_veto_rewrite_and_handoffs_do_not_accumulate_refs() {
    let fixture = Fixture::new();
    let writer = fixture.sync();
    let mut reader = fixture.sync();
    reader.config.host = "remover".into();
    let source = fixture.repo("writer", false);
    let target = fixture.repo("reader", false);
    fixture.jj(&source, &["bookmark", "set", "feature"], false);
    writer.snapshot(&source, false).unwrap();
    let old = fixture.commit(&source, "@");
    let source_url = fixture.url(&source);
    let target_url = fixture.url(&target);
    reader
        .fetch(&target, "sync-thastrom", &source_url, false, false)
        .unwrap();
    fixture.jj(&target, &["edit", &old], true);
    reader.snapshot(&target, false).unwrap();
    writer
        .fetch(&source, "sync-remover", &target_url, false, false)
        .unwrap();
    fs::write(source.join("file"), "replacement").unwrap();
    writer.snapshot(&source, false).unwrap();
    let replacement = fixture.commit(&source, "@");
    assert_eq!(
        fixture.jj(
            &source,
            &[
                "log",
                "-r",
                &format!("{old} & all()"),
                "--no-graph",
                "-T",
                "commit_id"
            ],
            false
        ),
        old
    );
    reader
        .fetch(&target, "sync-thastrom", &source_url, false, false)
        .unwrap();
    assert_eq!(fixture.commit(&target, "@"), replacement);
    assert!(fixture
        .jj(
            &target,
            &["log", "-r", "divergent()", "--no-graph", "-T", "commit_id"],
            false
        )
        .is_empty());
    fixture.jj(
        &target,
        &["bookmark", "set", "hosts/remover/peers/obsolete"],
        false,
    );
    for index in 0..5 {
        fs::write(target.join("file"), format!("handoff {index}")).unwrap();
        reader.snapshot(&target, false).unwrap();
        writer
            .fetch(&source, "sync-remover", &target_url, false, false)
            .unwrap();
        writer.snapshot(&source, false).unwrap();
        reader
            .fetch(&target, "sync-thastrom", &source_url, false, false)
            .unwrap();
    }
    for (path, host) in [(&source, "thastrom"), (&target, "remover")] {
        let refs = fixture.jj(
            path,
            &["bookmark", "list", "-T", "if(!remote, name ++ \"\\n\")"],
            false,
        );
        let expected = format!("hosts/{host}/work/default");
        assert!(
            refs.lines()
                .all(|name| name == "feature" || name == expected),
            "{refs}"
        );
    }
}

#[test]
fn snapshot_uses_local_feature_when_tracked_remote_differs() {
    let fixture = Fixture::new();
    let sync = fixture.sync();
    let source = fixture.repo("source", false);
    let target = fixture.repo("target", false);
    fixture.jj(&source, &["bookmark", "set", "feature"], false);
    fixture.jj(&source, &["git", "export"], false);
    sync.fetch(&target, "sync-remover", &fixture.url(&source), false, false)
        .unwrap();
    fixture.jj(
        &target,
        &["bookmark", "track", "feature@sync-remover"],
        false,
    );
    fixture.jj(
        &target,
        &["bookmark", "set", "feature", "-r", "@", "--allow-backwards"],
        false,
    );
    sync.snapshot(&target, false).unwrap();
    assert_eq!(
        fixture.commit(&target, "feature"),
        fixture.commit(&target, "@")
    );
    assert!(fixture
        .jj(
            &target,
            &["bookmark", "list", "glob:hosts/thastrom/bookmarks/*"],
            false
        )
        .is_empty());
}

#[test]
fn surgery_snapshots_stay_isolated_until_explicit_application() {
    let fixture = Fixture::new();
    for mode in ["serial", "parallel", "onto"] {
        let mut sync = fixture.sync();
        sync.config.state = fixture.path(&format!("state-{mode}"));
        sync.config
            .repository_roots
            .insert("dev".into(), fixture.path(&format!("local-{mode}")));
        let target = fixture.repo(&format!("local-{mode}/project"), true);
        let source = fixture.repo(&format!("remote-{mode}/project"), false);
        let mut remote = fixture.sync();
        remote.config.host = "remover".into();
        remote.config.state = fixture.path(&format!("remote-state-{mode}"));
        remote
            .config
            .repository_roots
            .insert("dev".into(), fixture.path(&format!("remote-{mode}")));
        sync.tools.ssh = transport(&fixture, &remote.config);
        std::env::set_var("GIT_SSH_VARIANT", "ssh");
        sync.accept_key("remover", &remote.public_key().unwrap())
            .unwrap();
        fs::write(source.join("other"), "second part").unwrap();
        remote.snapshot(&source, false).unwrap();
        sync.cycle().unwrap();
        sync.apply(&target, false).unwrap();
        let old = fixture.commit(&source, "@");
        fixture.jj(
            &target,
            &["new", &old, "-m", "Independent local work"],
            true,
        );
        fs::write(target.join("local"), "keep local edits").unwrap();
        fixture.jj(&target, &["status"], true);
        let local = fixture.commit(&target, "@");
        let mut args = vec!["split", "-m", "First part"];
        if mode == "parallel" {
            args.push("--parallel");
        }
        if mode == "onto" {
            args.extend(["--onto", "root()"]);
        }
        args.push("root:file");
        fixture.jj(&source, &args, true);
        let heads = fixture.jj(
            &source,
            &[
                "log",
                "-r",
                "heads(all())",
                "--no-graph",
                "-T",
                "commit_id ++ \"\\n\"",
            ],
            false,
        );
        for (index, revision) in heads.lines().enumerate() {
            fixture.jj(
                &source,
                &["bookmark", "set", &format!("part-{index}"), "-r", revision],
                false,
            );
        }
        for _ in 0..3 {
            remote.snapshot(&source, false).unwrap();
            sync.cycle().unwrap();
            assert_eq!(fixture.commit(&target, "@"), local);
            assert_eq!(fixture.commit(&target, "@-"), old);
            assert_eq!(
                fs::read_to_string(target.join("other")).unwrap(),
                "second part"
            );
            assert!(fixture
                .jj(
                    &target,
                    &["log", "-r", "divergent()", "--no-graph", "-T", "commit_id"],
                    false
                )
                .is_empty());
        }
        let config = fixture.path("split-cli.json");
        fs::write(&config, serde_json::to_string(&sync.config).unwrap()).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_jj-sync"))
            .arg("--config")
            .arg(&config)
            .args(["apply", "--dry-run", "--cached", "--json", "--path"])
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        // Git transport may omit cross-change evolution edges. Every split
        // part must still be represented with its actual change ID and parents.
        for revision in heads.lines() {
            let incoming = report["revisions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["commit_id"] == revision)
                .expect("split part missing from preview");
            let actual = fixture.jj(
                &source,
                &["log", "-r", revision, "--no-graph", "-T", "json(self)"],
                false,
            );
            let actual: serde_json::Value = serde_json::from_str(&actual).unwrap();
            assert_eq!(incoming["change_id"], actual["change_id"]);
            assert_eq!(incoming["parents"], actual["parents"]);
        }
        assert_eq!(fixture.commit(&target, "@"), local);
        sync.apply(&target, false).unwrap();
        assert_eq!(
            fs::read_to_string(target.join("local")).unwrap(),
            "keep local edits"
        );
        assert_eq!(sync.pending(&target).unwrap(), 0);
        for (index, revision) in heads.lines().enumerate() {
            assert_eq!(
                fixture.commit(&target, &format!("part-{index}@sync-remover")),
                revision
            );
        }
        std::env::remove_var("GIT_SSH_VARIANT");
    }
}

#[test]
fn peer_echoes_are_quiet_but_independent_edits_tags_and_deletions_are_pending() {
    let fixture = Fixture::new();
    let mut writer = fixture.sync();
    let source = fixture.repo("dev/project", false);
    fixture.jj(&source, &["bookmark", "set", "feature"], false);
    writer.snapshot(&source, false).unwrap();
    let original = fixture.commit(&source, "@");
    let mut peer = fixture.sync();
    peer.config.host = "remover".into();
    peer.config.state = fixture.path("remote-state");
    peer.config
        .repository_roots
        .insert("dev".into(), fixture.path("remote-dev"));
    let replica = fixture.repo("remote-dev/project", false);
    peer.fetch(
        &replica,
        "sync-thastrom",
        &fixture.url(&source),
        false,
        false,
    )
    .unwrap();
    fixture.jj(&replica, &["edit", &original], true);
    peer.snapshot(&replica, false).unwrap();
    writer.tools.ssh = transport(&fixture, &peer.config);
    std::env::set_var("GIT_SSH_VARIANT", "ssh");
    writer
        .accept_key("remover", &peer.public_key().unwrap())
        .unwrap();
    // Reproduce the first snapshot arriving only after the author rebased.
    fixture.jj(&source, &["new", "root()", "-m", "New base"], true);
    fs::write(source.join("base"), "new base").unwrap();
    fixture.jj(&source, &["status"], true);
    let base = fixture.commit(&source, "@");
    fixture.jj(&source, &["rebase", "-r", &original, "-d", &base], true);
    fixture.jj(&source, &["edit", "feature"], true);
    let rebased = fixture.commit(&source, "@");
    for _ in 0..2 {
        writer.cycle().unwrap();
        assert_eq!(writer.pending(&source).unwrap(), 0);
    }
    let before = fixture.jj(
        &source,
        &["op", "log", "--limit", "1", "--no-graph", "-T", "id"],
        false,
    );
    writer.apply(&source, true).unwrap();
    writer.apply(&source, false).unwrap();
    assert_eq!(fixture.commit(&source, "@"), rebased);
    assert_eq!(
        fixture.jj(
            &source,
            &["op", "log", "--limit", "1", "--no-graph", "-T", "id"],
            false
        ),
        before
    );
    assert!(fixture
        .jj(
            &source,
            &["log", "-r", "divergent()", "--no-graph", "-T", "commit_id"],
            false
        )
        .is_empty());
    // A peer's independent edit has the same change ID but no local evolution edge.
    fs::write(replica.join("peer"), "independent edit").unwrap();
    peer.snapshot(&replica, false).unwrap();
    writer.cycle().unwrap();
    assert_eq!(writer.pending(&source).unwrap(), 1);
    writer.apply(&source, true).unwrap();
    writer.apply(&source, false).unwrap();
    assert!(!fixture
        .jj(
            &source,
            &["log", "-r", "divergent()", "--no-graph", "-T", "commit_id"],
            false
        )
        .is_empty());
    // New tags still matter when all their commit objects are already known.
    fixture.jj(&replica, &["tag", "set", "nixos/Remover/1"], false);
    peer.snapshot(&replica, false).unwrap();
    writer.cycle().unwrap();
    assert_eq!(writer.pending(&source).unwrap(), 1);
    writer.apply(&source, false).unwrap();
    // Deleting a previously observed named bookmark must not be swallowed.
    fixture.jj(&replica, &["bookmark", "set", "parked"], false);
    peer.snapshot(&replica, false).unwrap();
    writer.cycle().unwrap();
    fixture.jj(&replica, &["bookmark", "delete", "parked"], false);
    peer.snapshot(&replica, false).unwrap();
    writer.cycle().unwrap();
    assert_eq!(writer.pending(&source).unwrap(), 1);
    std::env::remove_var("GIT_SSH_VARIANT");
}

#[test]
fn structured_preview_pairs_rewrites_and_keeps_descendants_available_without_checking_them_out() {
    let fixture = Fixture::new();
    let mut sync = fixture.sync();
    let source = fixture.repo("remote-dev/project", false);
    let target = fixture.repo("dev/project", false);
    let mut remote = fixture.sync();
    remote.config.host = "remover".into();
    remote.config.state = fixture.path("remote-state");
    remote
        .config
        .repository_roots
        .insert("dev".into(), fixture.path("remote-dev"));
    fixture.jj(&source, &["bookmark", "set", "base"], false);
    let base = fixture.commit(&source, "@");
    fixture.jj(&source, &["tag", "set", "deployed-base"], false);
    fixture.jj(&source, &["new", "-m", "Alacritty"], true);
    fs::write(source.join("alacritty"), "local network access\n").unwrap();
    fixture.jj(&source, &["bookmark", "set", "alacritty"], true);
    let alacritty_old = fixture.commit(&source, "@");
    fixture.jj(&source, &["new", "-m", "Staging"], true);
    fixture.jj(&source, &["bookmark", "set", "staging"], false);
    remote.snapshot(&source, false).unwrap();
    sync.tools.ssh = transport(&fixture, &remote.config);
    std::env::set_var("GIT_SSH_VARIANT", "ssh");
    sync.accept_key("remover", &remote.public_key().unwrap())
        .unwrap();
    sync.cycle().unwrap();
    sync.apply(&target, false).unwrap();
    let old_tip = fixture.commit(&source, "@");
    fixture.jj(&target, &["edit", &old_tip], true);
    fixture.jj(&source, &["rebase", "-r", "staging", "-d", &base], true);
    fixture.jj(
        &source,
        &["rebase", "-r", "alacritty", "-d", "staging"],
        true,
    );
    let new_tip = fixture.commit(&source, "staging");
    let alacritty_new = fixture.commit(&source, "alacritty");
    remote.snapshot(&source, false).unwrap();
    sync.cycle().unwrap();
    let config = fixture.path("cli.json");
    fs::write(&config, serde_json::to_string(&sync.config).unwrap()).unwrap();
    let cli = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_jj-sync"))
            .arg("--config")
            .arg(&config)
            .args(args)
            .arg("--cached")
            .arg("--path")
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };
    let pending = cli(&["pending", "--json"]);
    let pending: serde_json::Value = serde_json::from_slice(&pending.stdout).unwrap();
    assert!(!pending["peers"]["remover"]["revisions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["commit_id"] == base));
    let result = cli(&["apply", "--dry-run", "--json"]);
    let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report["workspace"]["before"], old_tip);
    assert_eq!(report["workspace"]["after"], new_tip);
    assert_eq!(report["workspace"]["files"][0]["path"], "alacritty");
    assert_eq!(report["workspace"]["files"][0]["status"], "removed");
    let change = report["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| {
            c["before"]
                .as_array()
                .unwrap()
                .contains(&json!(alacritty_old))
        })
        .unwrap();
    assert!(change["after"]
        .as_array()
        .unwrap()
        .contains(&json!(alacritty_new)));
    assert_eq!(change["kind"], "rewritten");
    assert!(report["workspace"]["available_outside_ancestry"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["commit_id"] == alacritty_new));
    assert_eq!(fixture.commit(&target, "@"), old_tip);
    assert!(target.join("alacritty").exists());
    let terminal = cli(&["apply", "--dry-run", "--color", "always"]);
    let terminal = String::from_utf8(terminal.stdout).unwrap();
    assert!(terminal.contains("\x1b["));
    assert!(!terminal.contains(".tmp"), "{terminal}");
    assert!(terminal.contains("Alacritty"));
    // The Nu commands return records, and ^ still selects the executable.
    let module = Path::new(env!("CARGO_MANIFEST_DIR")).join("nushell/jj-sync-commands.nu");
    let script = format!("use {} *; let p = (jj-sync pending --cached --config {} --path {}); if ($p | first | get peer) != 'remover' {{ error make {{msg: 'missing peer'}} }}; let r = (jj-sync apply --dry-run --cached --details --config {} --path {}); if $r.workspace.after != '{}' {{ error make {{msg: 'wrong checkout'}} }}; ^jj-sync --version", quote(&module), quote(&config), quote(&target), quote(&config), quote(&target), new_tip);
    let nu = Command::new("nu")
        .env(
            "PATH",
            format!(
                "{}:{}",
                Path::new(env!("CARGO_BIN_EXE_jj-sync"))
                    .parent()
                    .unwrap()
                    .display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .args(["--no-config-file", "-c", &script])
        .output()
        .unwrap();
    assert!(
        nu.status.success(),
        "{}",
        String::from_utf8_lossy(&nu.stderr)
    );
    let applied = cli(&["apply", "--json"]);
    let applied: serde_json::Value = serde_json::from_slice(&applied.stdout).unwrap();
    assert_eq!(applied["workspace"]["after"], new_tip);
    assert_eq!(fixture.commit(&target, "@"), new_tip);
    assert_eq!(
        fixture.commit(&target, "alacritty@sync-remover"),
        alacritty_new
    );
    assert!(!target.join("alacritty").exists());
    std::env::remove_var("GIT_SSH_VARIANT");
}

#[test]
fn notification_hints_are_allowlisted_durable_coalesced_and_never_apply() {
    let fixture = Fixture::new();
    let mut writer = fixture.sync();
    let source = fixture.repo("dev/project", false);
    let target = fixture.repo("remote-dev/project", false);
    let mut reader = fixture.sync();
    reader.config.host = "remover".into();
    reader.config.state = fixture.path("remote-state");
    reader
        .config
        .repository_roots
        .insert("dev".into(), fixture.path("remote-dev"));
    writer.tools.ssh = transport(&fixture, &reader.config);
    std::env::set_var("GIT_SSH_VARIANT", "ssh");
    writer
        .accept_key("remover", &reader.public_key().unwrap())
        .unwrap();
    assert_eq!(writer.publish().unwrap(), 1);
    assert_eq!(writer.publish().unwrap(), 0);
    // A failed delivery survives in the sender's queue.
    let transport = writer.tools.ssh.clone();
    writer.tools.ssh = fixture.script("offline-notify", "#!/bin/sh\nexit 1\n");
    assert!(writer.flush_notifications().is_err());
    writer.tools.ssh = transport;
    writer.flush_notifications().unwrap();
    let before = fixture.commit(&target, "@");
    assert_eq!(reader.pending(&target).unwrap(), 0);
    assert_eq!(fixture.commit(&target, "@"), before);
    let incoming = reader.config.state.join("notifications/incoming");
    assert_eq!(fs::read_dir(&incoming).unwrap().count(), 1);
    // A repeated delivery coalesces to a single durable hint.
    reader
        .serve("jj-sync-notify dev/project duplicate")
        .unwrap();
    reader
        .serve("jj-sync-notify dev/project duplicate")
        .unwrap();
    assert_eq!(fs::read_dir(&incoming).unwrap().count(), 1);
    for request in [
        "jj-sync-notify dev/private token",
        "jj-sync-notify ../project token",
        "jj-sync-notify dev/project token extra",
        "jj-sync-notify dev/project 'token; id'",
        "jj-sync-notify dev/project ''",
        "jj-sync-notify dev/project",
    ] {
        assert!(reader.serve(request).is_err(), "{request}");
    }
    assert_eq!(fixture.commit(&target, "@"), before);
    // Queueing needs no staging lock; a long-running apply cannot block it.
    let guard = reader.lock().unwrap();
    reader.serve("jj-sync-notify dev/project newer").unwrap();
    reader.refresh_notifications().unwrap();
    assert_eq!(fs::read_dir(&incoming).unwrap().count(), 1);
    drop(guard);
    // Publication continues even while the sender's staging lock is held.
    let guard = writer.lock().unwrap();
    fs::write(source.join("file"), "new work\n").unwrap();
    fixture.jj(&source, &["status"], true);
    assert_eq!(writer.publish().unwrap(), 1);
    drop(guard);
    std::env::remove_var("GIT_SSH_VARIANT");
}

#[test]
fn operation_events_publish_notify_and_fetch_without_a_timer_or_feedback_loop() {
    use std::{
        sync::atomic::{AtomicBool, Ordering},
        thread,
        time::{Duration, Instant},
    };
    let fixture = Fixture::new();
    let mut writer = fixture.sync();
    writer.config.repositories = [id("dev/project")].into();
    let source = fixture.repo("dev/project", true);
    let target = fixture.repo("remote-dev/project", false);
    let original = fixture.commit(&source, "@");
    let mut reader = fixture.sync();
    reader.config.host = "remover".into();
    reader.config.state = fixture.path("remote-state");
    reader
        .config
        .repository_roots
        .insert("dev".into(), fixture.path("remote-dev"));
    reader.config.repositories = [id("dev/project")].into();
    reader.config.peers = [(
        "thastrom".into(),
        jj_sync::Peer {
            address: "localhost".into(),
            ssh_alias: "thastrom".into(),
        },
    )]
    .into();
    writer.snapshot(&source, false).unwrap();
    reader
        .fetch(
            &target,
            "sync-thastrom",
            &fixture.url(&source),
            false,
            false,
        )
        .unwrap();
    fixture.jj(&target, &["edit", &original], true);
    fixture.jj(
        &target,
        &[
            "git",
            "remote",
            "set-url",
            "sync-thastrom",
            "ssh://thastrom/dev/project",
        ],
        false,
    );
    let writer_config = fixture.path("writer-events.json");
    let reader_config = fixture.path("reader-events.json");
    fs::write(
        &writer_config,
        serde_json::to_string(&writer.config).unwrap(),
    )
    .unwrap();
    fs::write(
        &reader_config,
        serde_json::to_string(&reader.config).unwrap(),
    )
    .unwrap();
    let calls = fixture.path("event-calls");
    let transport = fixture.script("event-ssh", &format!(
        "#!/bin/sh\nlast=\nprevious=\nfor arg do previous=$last; last=$arg; done\nprintf '%s %s\\n' \"$previous\" \"$last\" >> {}\ncase \"$previous\" in thastrom) config={};; remover) config={};; *) exit 1;; esac\nexport SSH_ORIGINAL_COMMAND=\"$last\"\nexec {} --config \"$config\" serve\n",
        quote(&calls), quote(&writer_config), quote(&reader_config), shell_words::quote(env!("CARGO_BIN_EXE_jj-sync"))));
    writer.tools.ssh = transport.clone();
    reader.tools.ssh = transport;
    std::env::set_var("GIT_SSH_VARIANT", "ssh");
    writer
        .accept_key("remover", &reader.public_key().unwrap())
        .unwrap();
    reader
        .accept_key("thastrom", &writer.public_key().unwrap())
        .unwrap();
    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        let w = scope.spawn(|| {
            writer.watch_until(Duration::from_secs(600), Duration::from_millis(50), &stop)
        });
        let r = scope.spawn(|| {
            reader.watch_until(Duration::from_secs(600), Duration::from_millis(50), &stop)
        });
        let wait = |condition: &dyn Fn() -> bool| {
            let deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < deadline {
                if condition() {
                    return true;
                }
                thread::sleep(Duration::from_millis(50));
            }
            false
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert!(wait(&|| fs::read_to_string(&calls)
                .unwrap_or_default()
                .contains("jj-sync-notify")));
            // Neither watcher audits nor the fallback timer can run during this test.
            // Block staging on the source to prove publication is independent.
            let guard = writer.lock().unwrap();
            fs::write(source.join("file"), "event-driven work\n").unwrap();
            fixture.jj(&source, &["status"], true);
            let replacement = fixture.commit(&source, "@");
            assert_ne!(replacement, original);
            assert!(wait(&|| fixture
                .commit(&source, "hosts/thastrom/work/default")
                == replacement));
            assert!(
                wait(&|| reader.pending(&target).unwrap_or(0) == 1),
                "Notification must trigger a fetch without polling the recovery interval"
            );
            let stage = reader.config.state.join("staging/thastrom/dev/project");
            assert_eq!(
                fixture.commit(&stage, "hosts/thastrom/work/default@sync-thastrom"),
                replacement
            );
            assert_eq!(fixture.commit(&target, "@"), original);
            assert_eq!(
                fs::read_to_string(target.join("file")).unwrap(),
                "initial\n"
            );
            drop(guard);
            // Once publication and notification acknowledgements settle, the
            // watcher must ignore its own ref operations and staging imports.
            thread::sleep(Duration::from_millis(1500));
            let before = fs::read_to_string(&calls).unwrap().lines().count();
            thread::sleep(Duration::from_millis(1200));
            let after = fs::read_to_string(&calls).unwrap().lines().count();
            assert_eq!(
                after, before,
                "Self-generated operations must not cause notification loops"
            );
            reader.apply(&target, true).unwrap();
            reader.apply(&target, false).unwrap();
            assert_eq!(fixture.commit(&target, "@"), replacement);
        }));
        stop.store(true, Ordering::Relaxed);
        w.join().unwrap().unwrap();
        r.join().unwrap().unwrap();
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    });
    std::env::remove_var("GIT_SSH_VARIANT");
}

#[test]
fn publication_does_not_swallow_an_operation_that_finishes_during_export() {
    let fixture = Fixture::new();
    let mut sync = fixture.sync();
    let source = fixture.repo("dev/project", false);
    let old = fixture.commit(&source, "@");
    let real_jj = sync.tools.jj.clone();
    let marker = fixture.path("advance-on-export");
    let python = Command::new("python3")
        .args(["-c", "import sys; print(sys.executable)"])
        .output()
        .unwrap();
    let wrapper = fixture.script("concurrent-publication", &format!(
        "#!{}\nimport os, subprocess, sys\nargs = sys.argv[1:]\np = subprocess.run([{}, *args], capture_output=True)\nif p.returncode == 0 and args[-2:] == ['git', 'export'] and not os.path.exists({}):\n open({}, 'w').close()\n open({}, 'w').write('new concurrent work')\n subprocess.run([{}, '-R', {}, 'status'], check=True, capture_output=True)\nsys.stdout.buffer.write(p.stdout)\nsys.stderr.buffer.write(p.stderr)\nsys.exit(p.returncode)\n",
        String::from_utf8(python.stdout).unwrap().trim(), serde_json::to_string(&real_jj).unwrap(),
        serde_json::to_string(&marker).unwrap(), serde_json::to_string(&marker).unwrap(),
        serde_json::to_string(&source.join("file")).unwrap(), serde_json::to_string(&real_jj).unwrap(), serde_json::to_string(&source).unwrap()));
    sync.tools.jj = wrapper;
    assert_eq!(sync.publish().unwrap(), 1);
    assert_ne!(fixture.commit(&source, "@"), old);
    // JJ has already moved its local bookmark, while the last exported Git
    // ref still describes the older snapshot advertised to the receiver.
    assert_eq!(
        fixture.commit(&source, "hosts/thastrom/work/default@git"),
        old
    );
    assert_eq!(
        sync.publish().unwrap(),
        1,
        "The second export must generate a fresh notification"
    );
    assert_eq!(
        fixture.commit(&source, "hosts/thastrom/work/default"),
        fixture.commit(&source, "@")
    );
    assert_eq!(sync.publish().unwrap(), 0);
}

#[test]
fn notification_acknowledgement_preserves_a_newer_publication() {
    let fixture = Fixture::new();
    let mut sync = fixture.sync();
    let source = fixture.repo("dev/project", false);
    sync.accept_key("remover", &sync.public_key().unwrap())
        .unwrap();
    sync.publish().unwrap();
    let config = fixture.path("publication-race.json");
    fs::write(&config, serde_json::to_string(&sync.config).unwrap()).unwrap();
    let advanced = fixture.path("advanced");
    sync.tools.ssh = fixture.script("publish-before-ack", &format!(
        "#!/bin/sh\nif [ ! -f {} ]; then\n : > {}\n printf 'newer work\\n' > {}\n {} --config {} publish\nfi\nprintf queued\n",
        quote(&advanced), quote(&advanced), quote(&source.join("file")), shell_words::quote(env!("CARGO_BIN_EXE_jj-sync")), quote(&config)));
    let queue = sync.config.state.join("notifications/outgoing");
    let path = fs::read_dir(&queue)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let old = fs::read_to_string(&path).unwrap();
    sync.flush_notifications().unwrap();
    let new =
        fs::read_to_string(&path).expect("Acknowledging an old hint must preserve its replacement");
    assert_ne!(old, new);
    sync.flush_notifications().unwrap();
    assert_eq!(fs::read_dir(queue).unwrap().count(), 0);
}

#[test]
fn small_peer_rewrite_has_bounded_details_and_nushell_display_in_a_large_history() {
    let fixture = Fixture::new();
    let mut sync = fixture.sync();
    let source = fixture.repo("remote-dev/project", false);
    let target = fixture.repo("dev/project", true);
    let base = fixture.commit(&source, "@");
    fixture.jj(
        &source,
        &[
            "new",
            "-m",
            "Collect display failure diagnostics from a TTY",
        ],
        true,
    );
    fs::write(source.join("diagnostics"), "capture diagnostics\n").unwrap();
    fixture.jj(&source, &["bookmark", "set", "diagnostics"], true);
    let old = fixture.commit(&source, "@");
    fixture.jj(
        &source,
        &["new", &base, "-m", "Historical configuration 0"],
        true,
    );
    for i in 1..165 {
        fixture.jj(
            &source,
            &["new", "-m", &format!("Historical configuration {i}")],
            true,
        );
    }
    fixture.jj(&source, &["tag", "set", "nixos/Thastrom/123"], false);
    let deployed = fixture.commit(&source, "@");
    let mut remote = fixture.sync();
    remote.config.host = "remover".into();
    remote.config.state = fixture.path("remote-state");
    remote
        .config
        .repository_roots
        .insert("dev".into(), fixture.path("remote-dev"));
    remote.snapshot(&source, false).unwrap();
    sync.tools.ssh = transport(&fixture, &remote.config);
    std::env::set_var("GIT_SSH_VARIANT", "ssh");
    sync.accept_key("remover", &remote.public_key().unwrap())
        .unwrap();
    sync.cycle().unwrap();
    sync.apply(&target, false).unwrap();
    let checkout = fixture.commit(&target, "@");
    fixture.jj(
        &source,
        &["rebase", "-r", "diagnostics", "-d", &deployed],
        true,
    );
    let replacement = fixture.commit(&source, "diagnostics");
    remote.snapshot(&source, false).unwrap();
    sync.cycle().unwrap();
    let config = fixture.path("compact-cli.json");
    fs::write(&config, serde_json::to_string(&sync.config).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_jj-sync"))
        .arg("--config")
        .arg(&config)
        .args(["apply", "--dry-run", "--cached", "--json", "--path"])
        .arg(&target)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["changes"].as_array().unwrap().len(), 1);
    assert_eq!(report["changes"][0]["kind"], "rewritten");
    assert_eq!(report["changes"][0]["before"], json!([old]));
    assert_eq!(report["changes"][0]["after"], json!([replacement]));
    assert_eq!(report["workspace"]["before"], checkout);
    assert_eq!(report["workspace"]["after"], checkout);
    assert!(report["workspace"]["files"].as_array().unwrap().is_empty());
    assert!(
        report["revisions"].as_array().unwrap().len() <= 8,
        "Only affected history belongs in the detailed report: {} revisions",
        report["revisions"].as_array().unwrap().len()
    );
    assert!(output.stdout.len() < 16000);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Historical configuration 70"));
    let module = Path::new(env!("CARGO_MANIFEST_DIR")).join("nushell/jj-sync-commands.nu");
    let script = format!(
        r#"
use {} *
let summary = (jj-sync apply --dry-run --cached --config {} --path {})
if $summary.workspace.files != 0 or $summary.conflicts != 0 or $summary.divergent != 0 {{ error make {{msg: 'incorrect workspace summary'}} }}
if not ($summary.workspace.checkout | str starts-with 'unchanged') {{ error make {{msg: 'checkout should be unchanged'}} }}
if ($summary.changes | length) != 1 {{ error make {{msg: 'only one change is affected'}} }}
for width in [80 120] {{
  let display = ($summary | table --expand --width $width | into string)
  if ($display | lines | length) > 40 {{ error make {{msg: $'Unbounded display at width ($width): ($display)'}} }}
  if ($display | str contains 'Historical configuration') {{ error make {{msg: 'unrelated history leaked into summary'}} }}
}}
let pending = (jj-sync pending --cached --config {} --path {})
if ($pending | length) != 1 or ($pending | first | get kind) != 'incoming' {{ error make {{msg: 'pending should show one incoming revision'}} }}
if ($pending | first | get revision | str length) != 8 {{ error make {{msg: 'pending IDs should be short'}} }}
let details = (jj-sync apply --dry-run --cached --details --config {} --path {})
if $details.workspace.after != '{}' {{ error make {{msg: 'details must retain full IDs'}} }}
if not ('operation_before' in ($details | columns)) {{ error make {{msg: 'details missing recovery operation'}} }}
if 'revisions' in ($summary | columns) {{ error make {{msg: 'raw graph must be opt-in'}} }}
$summary | table --expand --width 120
"#,
        quote(&module),
        quote(&config),
        quote(&target),
        quote(&config),
        quote(&target),
        quote(&config),
        quote(&target),
        checkout
    );
    let output = Command::new("nu")
        .env(
            "PATH",
            format!(
                "{}:{}",
                Path::new(env!("CARGO_BIN_EXE_jj-sync"))
                    .parent()
                    .unwrap()
                    .display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .args(["--no-config-file", "-c", &script])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).lines().count() < 40);
    assert_eq!(fixture.commit(&target, "@"), checkout);
    sync.apply(&target, false).unwrap();
    assert_eq!(fixture.commit(&target, "@"), checkout);
    assert_eq!(
        fixture.commit(&target, "diagnostics@sync-remover"),
        replacement
    );
    std::env::remove_var("GIT_SSH_VARIANT");
}

#[test]
fn nushell_summaries_bound_divergence_and_keep_errors_and_full_details_accessible() {
    let fixture = Fixture::new();
    let ids: Vec<_> = (0..12).map(|n| format!("{n:040x}")).collect();
    let revision = json!({"commit_id": ids[0], "change_id": "xvsnuyqtksquqktrsqpzqztxnktuqotp", "description": "Feature\n", "parents": []});
    let changes: Vec<_> = (0..15).map(|n| json!({"change_id": "xvsnuyqtksquqktrsqpzqztxnktuqotp", "before": ids, "after": ids, "kind": "divergent", "description": format!("Feature {n}")})).collect();
    let report = json!({
        "dry_run": true, "peers": ["remover"], "changes": changes,
        "workspace": {"before": ids[0], "after": ids[1],
            "files": (0..15).map(|n| json!({"path": format!("file-{n}"), "status": "modified"})).collect::<Vec<_>>(),
            "available_outside_ancestry": vec![revision.clone(); 13]},
        "conflicts": ids, "divergent": ids, "bookmarks": [], "tags": [],
        "revisions": vec![revision.clone(); 200], "evolution": [], "operation_before": "recovery-operation"
    });
    let pending = json!({"repository": "dev/project", "peers": {
        "remover": {"pending": true, "lastSuccess": "2026-11-17T16:38:51Z", "error": "SSH connection refused", "revisions": vec![revision; 15], "tags": [], "deleted_bookmarks": []},
        "empty": {"pending": false, "lastSuccess": null, "error": null, "revisions": [], "tags": [], "deleted_bookmarks": []},
        "same": {"pending": false, "lastSuccess": "2026-11-17T16:38:51Z", "error": null, "revisions": [], "tags": [], "deleted_bookmarks": []}
    }});
    let apply_file = fixture.path("apply-report.json");
    let pending_file = fixture.path("pending-report.json");
    fs::write(&apply_file, report.to_string()).unwrap();
    fs::write(&pending_file, pending.to_string()).unwrap();
    fs::create_dir(fixture.path("mock-bin")).unwrap();
    fixture.script("mock-bin/jj-sync", &format!("#!/bin/sh\nfor arg do\n if [ \"$arg\" = pending ]; then exec cat {}; fi\ndone\nexec cat {}\n", quote(&pending_file), quote(&apply_file)));
    let module = Path::new(env!("CARGO_MANIFEST_DIR")).join("nushell/jj-sync-commands.nu");
    let script = format!(
        r#"
use {} *
let summary = (jj-sync apply --dry-run)
if ($summary.changes | length) != 10 or $summary.more_changes != 5 {{ error make {{msg: 'changes must have visible overflow counts'}} }}
if not ($summary.changes.0.before | str ends-with '+10') {{ error make {{msg: 'divergent version lists must be bounded'}} }}
if $summary.more_files != 5 or $summary.more_outside_checkout != 3 {{ error make {{msg: 'workspace lists must have overflow counts'}} }}
if $summary.conflicts != 12 or ($summary.conflicts_revisions | length) != 10 {{ error make {{msg: 'conflicts must remain visible'}} }}
let full = (jj-sync apply --dry-run --details)
let compatible = (jj-sync apply --dry-run --json)
if ($full.revisions | length) != 200 or $compatible != $full {{ error make {{msg: 'full details must remain accessible'}} }}
if ($full.changes.0.before.0 | str length) != 40 {{ error make {{msg: 'details must retain full IDs'}} }}
let pending = (jj-sync pending --cached)
if ($pending | where kind == error | get description) != ['SSH connection refused'] {{ error make {{msg: 'fetch errors must survive truncation'}} }}
if ($pending | where kind == more | length) != 1 {{ error make {{msg: 'pending overflow must be explicit'}} }}
if ($pending | where peer == empty | get kind) != ['unfetched'] {{ error make {{msg: 'unfetched peer missing'}} }}
if ($pending | where peer == same | get kind) != ['up to date'] {{ error make {{msg: 'up-to-date peer missing'}} }}
let full_pending = (jj-sync pending --details)
if ($full_pending | where peer == remover | first | get revisions | length) != 15 {{ error make {{msg: 'pending details were truncated'}} }}
"#,
        quote(&module)
    );
    let output = Command::new("nu")
        .env(
            "PATH",
            format!(
                "{}:{}",
                fixture.path("mock-bin").display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .args(["--no-config-file", "-c", &script])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
