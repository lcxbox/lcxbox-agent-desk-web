use super::*;

#[test]
fn unsupported_workers_refuse_before_touching_roots() {
    let private = tempfile::tempdir().unwrap();
    for reason in [
        ExecSandboxUnsupportedReason::NotImplemented,
        ExecSandboxUnsupportedReason::KernelUnsupported,
        ExecSandboxUnsupportedReason::SystemIdentity,
    ] {
        let sandbox = ExecSandbox::with_support(
            ExecSandboxSupport::Unsupported(reason),
            private.path().to_path_buf(),
            ProtectedRoots::default(),
            Vec::new(),
        );
        assert_eq!(sandbox.support(), ExecSandboxSupport::Unsupported(reason));
        match sandbox.prepare_handles(&[], None) {
            Err(SandboxError::Unsupported(refused)) => assert_eq!(refused, reason),
            other => panic!("expected a refusal, got {:?}", other.err()),
        }
    }
    assert!(
        !private.path().join("exec-tmp").exists(),
        "no temporary directory is created for a refused command"
    );
}

#[cfg(unix)]
#[test]
fn the_identity_follows_the_effective_user() {
    let root = unsafe { libc::geteuid() } == 0;
    assert_eq!(
        execution_identity(),
        ExecIdentity {
            elevated: false,
            system: false,
            root
        }
    );
    if root {
        assert_eq!(
            probe_support(),
            ExecSandboxSupport::Unsupported(ExecSandboxUnsupportedReason::SystemIdentity)
        );
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[test]
fn other_platforms_report_not_implemented() {
    assert!(matches!(
        probe_support(),
        ExecSandboxSupport::Unsupported(
            ExecSandboxUnsupportedReason::NotImplemented
                | ExecSandboxUnsupportedReason::SystemIdentity
        )
    ));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix {
    use super::*;
    use crate::worker::exec_containment::Containment;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::process::{Output, Stdio};

    /// Whether this host can run the sandbox. macOS always can (as a
    /// non-root user); a Linux CI kernel below Landlock ABI 3 or a root
    /// runner cannot, and the behaviour tests are skipped there.
    pub(super) fn sandbox_available() -> bool {
        match sandbox_support() {
            ExecSandboxSupport::Supported => true,
            other => {
                assert!(
                    !cfg!(target_os = "macos") || unsafe { libc::geteuid() } == 0,
                    "sandbox-exec must work on macOS: {other:?}"
                );
                eprintln!("command sandbox unavailable here, skipping: {other:?}");
                false
            }
        }
    }

    pub(super) struct Fixture {
        _scratch: tempfile::TempDir,
        pub base: PathBuf,
        pub private: PathBuf,
        pub root: PathBuf,
        pub outside: PathBuf,
        pub sandbox: ExecSandbox,
    }

    /// A scratch tree in `/tmp`, outside the macOS user temporary directory
    /// (which the macOS profile leaves writable).
    pub(super) fn fixture() -> Fixture {
        fixture_with(|private| ProtectedRoots::new(None, Vec::new(), [private.to_path_buf()]))
    }

    pub(super) fn fixture_with(protected: impl FnOnce(&Path) -> ProtectedRoots) -> Fixture {
        let scratch = tempfile::Builder::new()
            .prefix("exec-sandbox")
            .tempdir_in("/tmp")
            .unwrap();
        let base = std::fs::canonicalize(scratch.path()).unwrap();
        let private = base.join("private");
        let root = base.join("root");
        let outside = base.join("outside");
        for directory in [&private, &root, &outside] {
            std::fs::create_dir(directory).unwrap();
        }
        std::fs::write(outside.join("existing.txt"), "keep").unwrap();
        std::fs::write(private.join("config.toml"), "secret").unwrap();
        let sandbox = ExecSandbox::with_support(
            sandbox_support(),
            private.clone(),
            protected(&private),
            vec![private.clone()],
        );
        Fixture {
            _scratch: scratch,
            base,
            private,
            root,
            outside,
            sandbox,
        }
    }

    pub(super) fn open_directory(path: &Path) -> File {
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)
            .unwrap()
    }

    /// Run `script` with `sh -c` in the prepared sandbox, as the pipe
    /// executor does: `setsid` containment, piped output, no stdin.
    pub(super) async fn run_prepared(
        prepared: &PreparedSandbox,
        cwd: &Path,
        variables: &[(&str, &Path)],
        script: &str,
    ) -> Output {
        let containment = Containment::prepare("sandbox-test").unwrap();
        let mut command = prepared.pipe_command("/bin/sh", &["-c".to_owned(), script.to_owned()]);
        command
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (name, value) in variables {
            command.env(name, value);
        }
        containment.apply_new_session(&mut command);
        command.output().await.unwrap()
    }

    /// Run `script` with the fixture's root as the only write root and
    /// working directory; `ROOT` and `OUT` name the root and an outside
    /// directory.
    pub(super) async fn run(fixture: &Fixture, script: &str) -> Output {
        run_in(fixture, &[&fixture.root], script).await
    }

    pub(super) async fn run_in(fixture: &Fixture, roots: &[&Path], script: &str) -> Output {
        let handles: Vec<File> = roots.iter().map(|root| open_directory(root)).collect();
        let handles: Vec<&File> = handles.iter().collect();
        let prepared = fixture.sandbox.prepare_handles(&handles, None).unwrap();
        run_prepared(
            &prepared,
            roots.first().copied().unwrap_or(&fixture.base),
            &[
                ("ROOT", fixture.root.as_path()),
                ("OUT", fixture.outside.as_path()),
                ("PRIVATE", fixture.private.as_path()),
            ],
            script,
        )
        .await
    }

    pub(super) fn succeeded(output: &Output) -> bool {
        output.status.success()
    }

    pub(super) fn describe(output: &Output) -> String {
        format!(
            "status={:?} stdout={} stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    pub(super) async fn assert_allowed(fixture: &Fixture, script: &str) {
        let output = run(fixture, script).await;
        assert!(
            succeeded(&output),
            "`{script}` should succeed: {}",
            describe(&output)
        );
    }

    pub(super) async fn assert_denied(fixture: &Fixture, script: &str) {
        let output = run(fixture, script).await;
        assert!(
            !succeeded(&output),
            "`{script}` should fail: {}",
            describe(&output)
        );
    }

    #[tokio::test]
    async fn writes_stay_inside_the_roots() {
        if !sandbox_available() {
            return;
        }
        let fixture = fixture();
        assert_allowed(
            &fixture,
            r#"set -e; echo a > "$ROOT/a"; mkdir -p "$ROOT/x/y" "$ROOT/z"; echo b > "$ROOT/x/y/b";
               mv "$ROOT/x/y/b" "$ROOT/z/b"; : > "$ROOT/a"; rm "$ROOT/z/b"; rmdir "$ROOT/x/y""#,
        )
        .await;
        assert!(fixture.root.join("a").exists() && fixture.root.join("z").is_dir());

        for script in [
            r#"echo x > "$OUT/new.txt""#,
            r#"echo x >> "$OUT/existing.txt""#,
            r#": > "$OUT/existing.txt""#,
            r#"perl -e 'truncate($ARGV[0], 0) or exit 1' "$OUT/existing.txt""#,
            r#"mkdir "$OUT/dir""#,
            r#"rm "$OUT/existing.txt""#,
            r#"mv "$OUT/existing.txt" "$ROOT/moved.txt""#,
            r#"echo x > "$ROOT/in.txt" && mv "$ROOT/in.txt" "$OUT/in.txt""#,
            r#"ln "$OUT/existing.txt" "$ROOT/link""#,
            r#"cd .. && echo x > outside/new.txt"#,
            r#"ln -s "$OUT" "$ROOT/escape" && echo x > "$ROOT/escape/new.txt""#,
            r#"mv "$ROOT" "$ROOT-renamed""#,
        ] {
            assert_denied(&fixture, script).await;
        }
        assert_eq!(
            std::fs::read_to_string(fixture.outside.join("existing.txt")).unwrap(),
            "keep"
        );
        let mut outside: Vec<_> = std::fs::read_dir(&fixture.outside)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        outside.sort();
        assert_eq!(outside, ["existing.txt"]);
        assert!(fixture.root.is_dir(), "the root itself cannot be renamed");
    }

    #[tokio::test]
    async fn descendants_inherit_the_sandbox() {
        if !sandbox_available() {
            return;
        }
        let fixture = fixture();
        assert_denied(&fixture, r#"sh -c 'sh -c "echo x > \"$OUT/deep.txt\""'"#).await;
        assert_denied(&fixture, r#"(echo x > "$OUT/background.txt") & wait $!"#).await;
        assert_allowed(&fixture, r#"sh -c 'sh -c "echo x > \"$ROOT/deep.txt\""'"#).await;
        assert!(!fixture.outside.join("deep.txt").exists());
    }

    #[tokio::test]
    async fn null_device_and_standard_streams_stay_writable() {
        if !sandbox_available() {
            return;
        }
        let fixture = fixture();
        let output = run(
            &fixture,
            "set -e; echo quiet > /dev/null; echo out > /dev/stdout; echo err | tee /dev/stderr > /dev/null",
        )
        .await;
        assert!(succeeded(&output), "{}", describe(&output));
        assert_eq!(String::from_utf8_lossy(&output.stdout), "out\n");
        assert_eq!(String::from_utf8_lossy(&output.stderr), "err\n");
    }

    #[tokio::test]
    async fn each_execution_gets_a_private_temporary_directory() {
        if !sandbox_available() {
            return;
        }
        let fixture = fixture();
        let root = open_directory(&fixture.root);
        let prepared = fixture.sandbox.prepare_handles(&[&root], None).unwrap();
        let temp = prepared.temp_dir().to_path_buf();
        assert!(temp.starts_with(fixture.private.join("exec-tmp")));
        let mode = std::fs::metadata(&temp).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        let output = run_prepared(
            &prepared,
            &fixture.root,
            &[],
            r#"set -e; test "$TMPDIR" = "$TEMP"; test "$TMP" = "$TEMP"; printf %s "$TMPDIR";
               echo x > "$TMPDIR/file"; mkdir "$TMPDIR/dir"; cd "$TMPDIR/dir"; pwd -P >/dev/null"#,
        )
        .await;
        assert!(succeeded(&output), "{}", describe(&output));
        assert_eq!(
            PathBuf::from(String::from_utf8_lossy(&output.stdout).as_ref()),
            temp
        );
        assert!(temp.join("file").is_file());
        let parent = run_prepared(
            &prepared,
            &fixture.root,
            &[],
            r#"echo x > "$TMPDIR/../sibling" || mkdir "$TMPDIR/../sibling" || mv "$TMPDIR" "$TMPDIR.x""#,
        )
        .await;
        assert!(!succeeded(&parent), "{}", describe(&parent));

        let second = fixture.sandbox.prepare_handles(&[&root], None).unwrap();
        assert_ne!(second.temp_dir(), temp, "a directory is never reused");
        drop(prepared);
        assert!(!temp.exists(), "removed with the execution");
        assert!(second.temp_dir().exists());
    }

    #[tokio::test]
    async fn crafted_directory_names_stay_data() {
        if !sandbox_available() {
            return;
        }
        let fixture = fixture();
        for name in [
            "quote\"name",
            "paren)name",
            "new\nline",
            "inj\")) (allow file-write* (subpath \"/\")) (allow file-read* (subpath \"/tmp",
            "\") (allow file-write* (subpath \"/\")) (subpath \"",
        ] {
            // Names with `/` become nested directories with the same text.
            let root = fixture.base.join("crafted").join(name);
            std::fs::create_dir_all(&root).unwrap();
            let output = run_in(
                &fixture,
                &[&root],
                r#"echo in > ./inside.txt && { echo x > "$OUT/injected.txt" && exit 3; exit 0; }"#,
            )
            .await;
            assert!(succeeded(&output), "{name:?}: {}", describe(&output));
            assert!(root.join("inside.txt").is_file());
            assert!(!fixture.outside.join("injected.txt").exists(), "{name:?}");
        }
    }

    #[tokio::test]
    async fn a_root_path_close_to_the_path_limit_works() {
        if !sandbox_available() {
            return;
        }
        let fixture = fixture();
        let mut root = fixture.base.clone();
        let segment = "d".repeat(200);
        while root.as_os_str().len() + segment.len() + 1 < 1000 {
            root.push(&segment);
        }
        let filler = 1000 - root.as_os_str().len() - 1;
        root.push("e".repeat(filler));
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(root.as_os_str().len(), 1000);
        let output = run_in(
            &fixture,
            &[&root],
            r#"echo in > ./f && { echo x > "$OUT/long.txt" && exit 3; exit 0; }"#,
        )
        .await;
        assert!(succeeded(&output), "{}", describe(&output));
        assert!(root.join("f").is_file());
        assert!(!fixture.outside.join("long.txt").exists());
    }

    #[tokio::test]
    async fn git_works_inside_a_root() {
        if !sandbox_available() || !Path::new("/usr/bin/git").is_file() {
            return;
        }
        let fixture = fixture();
        let output = run(
            &fixture,
            r#"set -e; export HOME="$TMPDIR" GIT_CONFIG_NOSYSTEM=1;
               git init -q . && echo a > a.txt && git add a.txt &&
               git -c user.name=t -c user.email=t@example.invalid commit -qm init && git gc -q && git status --short"#,
        )
        .await;
        assert!(succeeded(&output), "{}", describe(&output));
        assert!(fixture.root.join(".git/HEAD").is_file());
    }

    #[test]
    fn forbidden_roots_are_refused_after_resolving_the_handle() {
        if !sandbox_available() {
            return;
        }
        let fixture = fixture_with(|private| {
            let base = private.parent().unwrap();
            let home = base.join("home");
            std::fs::create_dir_all(home.join("project")).unwrap();
            std::fs::create_dir_all(home.join(".ssh")).unwrap();
            std::fs::create_dir_all(base.join("tools/bin")).unwrap();
            let path =
                std::env::join_paths([base.join("tools/bin"), PathBuf::from("."), PathBuf::new()])
                    .unwrap();
            ProtectedRoots::new(Some(home), Vec::new(), [private.to_path_buf()])
                .with_executable_search_path(&path)
        });
        let refused = |path: &Path| {
            let handle = open_directory(path);
            matches!(
                fixture.sandbox.prepare_handles(&[&handle], None),
                Err(SandboxError::ForbiddenRoot(_))
            )
        };
        let home = fixture.base.join("home");
        assert!(refused(&home), "the home directory");
        assert!(refused(&fixture.base), "an ancestor of home");
        assert!(refused(&home.join(".ssh")));
        assert!(refused(&fixture.base.join("tools/bin")), "a PATH directory");
        assert!(
            refused(&fixture.base.join("tools")),
            "contains a PATH directory"
        );
        assert!(refused(&fixture.private), "product data");
        assert!(refused(Path::new("/")));
        assert!(!refused(&home.join("project")));
        // A link to a forbidden directory is judged by what it resolves to.
        std::os::unix::fs::symlink(&home, fixture.root.join("home-link")).unwrap();
        let through_link = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY)
            .open(fixture.root.join("home-link"))
            .unwrap();
        assert!(matches!(
            fixture.sandbox.prepare_handles(&[&through_link], None),
            Err(SandboxError::ForbiddenRoot(_))
        ));
        assert!(
            fixture.sandbox.prepare_handles(&[], None).is_ok(),
            "no roots: only the temporary directory"
        );
    }

    #[test]
    fn stale_temporary_directories_of_dead_workers_are_swept() {
        let private = tempfile::tempdir().unwrap();
        let parent = private.path().join("exec-tmp");
        std::fs::create_dir(&parent).unwrap();
        let mut exited = std::process::Command::new("/usr/bin/true").spawn().unwrap();
        let dead = exited.id();
        exited.wait().unwrap();
        let stale = parent.join(format!("{dead}-stale"));
        std::fs::create_dir_all(stale.join("locked")).unwrap();
        std::fs::set_permissions(stale.join("locked"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        let own = parent.join(format!("{}-live", std::process::id()));
        std::fs::create_dir(&own).unwrap();
        let unrelated = parent.join("not-an-execution");
        std::fs::create_dir(&unrelated).unwrap();
        PerExecTemp::sweep_stale(private.path());
        assert!(!stale.exists(), "even with a directory the command locked");
        assert!(own.exists() && unrelated.exists());
    }

    #[tokio::test]
    async fn sandboxed_commands_keep_their_pid_and_lead_a_new_session() {
        if !sandbox_available() {
            return;
        }
        let fixture = fixture();
        let root = open_directory(&fixture.root);
        let prepared = fixture.sandbox.prepare_handles(&[&root], None).unwrap();
        let mut containment = Containment::prepare("sandbox-test").unwrap();
        let mut command = prepared.pipe_command(
            "/bin/sh",
            &["-c".to_owned(), "sleep 30 & echo $$ $!; wait".to_owned()],
        );
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .kill_on_drop(true);
        containment.apply_new_session(&mut command);
        let mut child = command.spawn().unwrap();
        containment.adopt(&child).unwrap();
        let pid = child.id().unwrap() as libc::pid_t;
        let mut stdout = tokio::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        tokio::io::AsyncBufReadExt::read_line(&mut stdout, &mut line)
            .await
            .unwrap();
        let mut ids = line
            .split_whitespace()
            .map(|id| id.parse::<libc::pid_t>().unwrap());
        let (shell, background) = (ids.next().unwrap(), ids.next().unwrap());
        assert_eq!(shell, pid, "sandbox-exec replaces itself with the command");
        assert_eq!(unsafe { libc::getpgid(pid) }, pid);
        assert_eq!(unsafe { libc::getsid(pid) }, pid, "a new session");
        assert_eq!(unsafe { libc::getpgid(background) }, pid);
        containment.reclaim().unwrap();
        let _ = child.wait().await;
        let mut gone = false;
        for _ in 0..50 {
            if unsafe { libc::kill(background, 0) } != 0 {
                gone = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(gone, "the background child was reclaimed with the group");
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::unix::*;
    use super::*;

    #[test]
    fn the_full_profile_probe_passes() {
        assert_eq!(probe_support(), ExecSandboxSupport::Supported);
    }

    #[test]
    fn the_profile_names_parameters_and_never_paths() {
        let fixture = fixture();
        let root = open_directory(&fixture.root);
        let prepared = fixture
            .sandbox
            .prepare_handles(&[&root], Some(Path::new("/dev/ttys999")))
            .unwrap();
        let profile = prepared.platform.profile();
        assert!(!profile.contains(fixture.base.to_str().unwrap()));
        assert!(!profile.contains("/dev/ttys999"));
        for parameter in ["ROOT0", "TMP", "PRIVATE0", "PTY"] {
            assert!(
                profile.contains(&format!("(param \"{parameter}\")")),
                "{parameter}"
            );
        }
    }

    #[tokio::test]
    async fn metadata_outside_the_roots_is_restricted() {
        let fixture = fixture();
        for script in [
            r#"chmod 600 "$OUT/existing.txt""#,
            r#"touch -t 200001010000 "$OUT/existing.txt""#,
            r#"xattr -w com.example.test 1 "$OUT/existing.txt""#,
        ] {
            assert_denied(&fixture, script).await;
        }
        assert_allowed(&fixture, r#"touch "$ROOT/f" && chmod 600 "$ROOT/f""#).await;
    }

    #[tokio::test]
    async fn the_user_temporary_directory_is_writable_except_product_entries() {
        let fixture = fixture();
        let user_temp = super::super::platform::user_temp_dir().unwrap();
        let probe = user_temp.join(format!("sandbox-probe-{}", uuid::Uuid::new_v4().simple()));
        let output = run_prepared(
            &fixture.sandbox.prepare_handles(&[], None).unwrap(),
            &fixture.base,
            &[("PROBE", &probe), ("USER_TMP", &user_temp)],
            r#"set -e; echo x > "$PROBE"; mktemp > /dev/null
               if echo x > "$USER_TMP/lcxl-rd-sandbox-test"; then exit 3; fi
               if mkdir "$USER_TMP/lcxl-rd-x"; then exit 4; fi
               if echo x >> "$USER_TMP/lcxl_remote_desk_server.lock"; then exit 5; fi
               if echo x > "$USER_TMP/LCXL-RD-case"; then exit 6; fi"#,
        )
        .await;
        let _ = std::fs::remove_file(&probe);
        assert!(succeeded(&output), "{}", describe(&output));
        assert!(!user_temp.join("lcxl-rd-sandbox-test").exists());
        assert!(!user_temp.join("lcxl-rd-x").exists());
    }

    #[tokio::test]
    async fn product_data_content_is_unreadable() {
        let fixture = fixture();
        assert_denied(&fixture, r#"cat "$PRIVATE/config.toml""#).await;
        assert_denied(&fixture, r#"ls "$PRIVATE""#).await;
        assert_allowed(&fixture, r#"cat /etc/hosts > /dev/null"#).await;
        assert_allowed(
            &fixture,
            r#"echo x > "$TMPDIR/f" && cat "$TMPDIR/f" && cd "$TMPDIR""#,
        )
        .await;
    }

    #[tokio::test]
    async fn setuid_programs_do_not_run() {
        let fixture = fixture();
        let output = run(&fixture, "/usr/bin/sudo -n true").await;
        assert_eq!(output.status.code(), Some(126), "{}", describe(&output));
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::unix::*;
    use super::*;
    use crate::worker::exec_sandbox::platform::{MINIMUM_ABI, support_for};

    #[test]
    fn kernels_without_abi_three_are_unsupported() {
        let unsupported =
            ExecSandboxSupport::Unsupported(ExecSandboxUnsupportedReason::KernelUnsupported);
        for probe in [
            Err(libc::ENOSYS),
            Err(libc::EOPNOTSUPP),
            Err(libc::EPERM),
            Ok(1),
            Ok(MINIMUM_ABI - 1),
        ] {
            assert_eq!(support_for(probe), unsupported, "{probe:?}");
        }
        assert_eq!(support_for(Ok(MINIMUM_ABI)), ExecSandboxSupport::Supported);
        assert_eq!(support_for(Ok(7)), ExecSandboxSupport::Supported);
    }

    #[tokio::test]
    async fn metadata_outside_the_roots_is_not_restricted() {
        if !sandbox_available() {
            return;
        }
        let fixture = fixture();
        assert_allowed(&fixture, r#"chmod 600 "$OUT/existing.txt""#).await;
        assert_allowed(
            &fixture,
            r#"perl -e 'utime(1, 1, $ARGV[0]) or exit 1' "$OUT/existing.txt""#,
        )
        .await;
        assert_eq!(
            std::fs::read_to_string(fixture.outside.join("existing.txt")).unwrap(),
            "keep"
        );
    }

    #[tokio::test]
    async fn shared_memory_is_writable_and_the_system_temp_is_not() {
        if !sandbox_available() {
            return;
        }
        let fixture = fixture();
        let shm = PathBuf::from("/dev/shm").join(format!("sandbox-{}", uuid::Uuid::new_v4()));
        if Path::new("/dev/shm").is_dir() {
            let output = run_prepared(
                &fixture.sandbox.prepare_handles(&[], None).unwrap(),
                &fixture.base,
                &[("SHM", &shm)],
                r#"echo x > "$SHM""#,
            )
            .await;
            let _ = std::fs::remove_file(&shm);
            assert!(succeeded(&output), "{}", describe(&output));
        }
        let temp = PathBuf::from("/tmp").join(format!("sandbox-{}", uuid::Uuid::new_v4()));
        let output = run_prepared(
            &fixture.sandbox.prepare_handles(&[], None).unwrap(),
            &fixture.base,
            &[("SYSTEM_TMP", &temp)],
            r#"echo x > "$SYSTEM_TMP""#,
        )
        .await;
        assert!(!succeeded(&output), "{}", describe(&output));
        assert!(!temp.exists());
    }

    #[tokio::test]
    async fn privileges_cannot_be_gained_and_there_is_no_terminal() {
        if !sandbox_available() {
            return;
        }
        let fixture = fixture();
        let output = run(&fixture, "grep NoNewPrivs /proc/self/status").await;
        assert!(
            String::from_utf8_lossy(&output.stdout).contains('1'),
            "{}",
            describe(&output)
        );
        assert_denied(&fixture, "echo x > /dev/tty").await;
        assert_denied(&fixture, "tty").await;
        assert_denied(&fixture, "setsid -w sh -c 'echo x > \"$OUT/detached.txt\"'").await;
    }

    #[test]
    fn restricted_spawn_threads_restrict_only_their_children() {
        if !sandbox_available() {
            return;
        }
        let fixture = fixture();
        let second_root = fixture.base.join("second");
        std::fs::create_dir(&second_root).unwrap();
        let spawn = |roots: &[&Path], target: &Path| {
            let handles: Vec<File> = roots.iter().map(|root| open_directory(root)).collect();
            let handles: Vec<&File> = handles.iter().collect();
            let prepared = fixture.sandbox.prepare_handles(&handles, None).unwrap();
            let target = target.to_path_buf();
            prepared
                .spawn_restricted(move || {
                    std::process::Command::new("/bin/sh")
                        .arg("-c")
                        .arg("echo x > \"$TARGET\"")
                        .env("TARGET", &target)
                        .status()
                })
                .unwrap()
                .unwrap()
        };
        assert!(spawn(&[&fixture.root], &fixture.root.join("a")).success());
        assert!(!spawn(&[&fixture.root], &second_root.join("a")).success());
        // A second execution does not stack the first one's rules.
        assert!(spawn(&[&second_root], &second_root.join("b")).success());
        assert!(!spawn(&[&second_root], &fixture.root.join("b")).success());
        // The worker's own threads stay unrestricted.
        std::fs::write(fixture.outside.join("worker.txt"), "worker").unwrap();
        std::fs::write(fixture.private.join("vault.txt"), "worker").unwrap();
    }

    #[test]
    fn a_pty_command_can_use_its_own_terminal_only() {
        use portable_pty::{CommandBuilder, PtySize, native_pty_system};
        use std::io::Read;
        if !sandbox_available() {
            return;
        }
        let fixture = fixture();
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let slave = pair.master.tty_name().unwrap();
        let root = open_directory(&fixture.root);
        let prepared = fixture
            .sandbox
            .prepare_handles(&[&root], Some(&slave))
            .unwrap();
        let mut command = CommandBuilder::new("/bin/sh");
        command.args([
            "-c",
            r#"echo tty-ok > /dev/tty && echo slave-ok > "$(tty)" && stty -a > /dev/null && echo stty-ok;
               if echo x > "$OUT/pty.txt"; then echo outside-written; fi"#,
        ]);
        command.env("OUT", &fixture.outside);
        command.cwd(&fixture.root);
        for (name, value) in prepared.environment() {
            command.env(name, value);
        }
        let slave_pty = pair.slave;
        let mut child = prepared
            .spawn_restricted(move || slave_pty.spawn_command(command))
            .unwrap()
            .unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let status = child.wait().unwrap();
        drop(pair.master);
        let mut output = Vec::new();
        let mut buffer = [0u8; 4096];
        while let Ok(read) = reader.read(&mut buffer) {
            if read == 0 {
                break;
            }
            output.extend_from_slice(&buffer[..read]);
        }
        let output = String::from_utf8_lossy(&output);
        assert!(status.success(), "{output}");
        for expected in ["tty-ok", "slave-ok", "stty-ok"] {
            assert!(output.contains(expected), "{expected}: {output}");
        }
        assert!(!output.contains("outside-written"), "{output}");
        assert!(!fixture.outside.join("pty.txt").exists());
    }
}

#[cfg(target_os = "macos")]
#[test]
fn refused_setuid_programs_get_a_run_after_hint() {
    assert!(unavailable_program_hint(Some(126), "sh: /bin/ps: Operation not permitted").is_some());
    assert!(unavailable_program_hint(Some(126), "sudo: Permission denied").is_some());
    assert!(unavailable_program_hint(Some(1), "Operation not permitted").is_none());
    assert!(unavailable_program_hint(Some(126), "cannot execute binary file").is_none());
}

#[cfg(target_os = "linux")]
#[test]
fn setuid_programs_refused_by_no_new_privs_get_a_run_after_hint() {
    assert!(
        unavailable_program_hint(
            Some(1),
            "sudo: The \"no new privileges\" flag is set, which prevents sudo from running as root."
        )
        .is_some()
    );
    assert!(
        unavailable_program_hint(
            Some(1),
            "sudo: effective uid is not 0, is /usr/bin/sudo on a file system with the 'nosuid' option set?"
        )
        .is_some()
    );
    assert!(unavailable_program_hint(Some(127), "sh: foo: command not found").is_none());
}
