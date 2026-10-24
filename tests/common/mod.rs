//! The only way tests build a `Command` for the binary. Every later task uses it.
#![allow(dead_code)]

use assert_cmd::Command;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

pub struct Sandbox {
    _tmp: TempDir,
    pub root: PathBuf,
}

impl Sandbox {
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for d in [
            "config",
            "cache",
            "home",
            "appdata",
            "localappdata",
            "xdg-config",
            "xdg-cache",
            "claude",
            "codex",
            "bin",
        ] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        for tool in ["claude", "codex", "opencode"] {
            write_stub(&root.join("bin"), tool);
        }
        Sandbox { _tmp: tmp, root }
    }

    pub fn config_dir(&self) -> PathBuf {
        self.root.join("config")
    }

    pub fn stub_log(&self) -> PathBuf {
        self.root.join("stub.log")
    }

    /// A command for the `fbrain` binary with every user-level location redirected into the sandbox.
    pub fn cmd(&self) -> Command {
        Command::from_std(self.std_cmd())
    }

    /// The same sandbox as a `std::process::Command`, for tests that spawn and keep a process.
    pub fn std_cmd(&self) -> std::process::Command {
        self.sandboxed(std::process::Command::new(fbrain_exe()))
    }

    /// The same sandbox for another copy of the binary (an install that is later deleted).
    pub fn std_cmd_for(&self, exe: &Path) -> std::process::Command {
        self.sandboxed(std::process::Command::new(exe))
    }

    /// `sh -c <line>` in the same sandbox: runs a hook command string the way a harness does.
    #[cfg(unix)]
    pub fn shell_cmd(&self, line: &str) -> std::process::Command {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", line]);
        self.sandboxed(c)
    }

    /// `FBRAIN_RUNTIME_DIR` of this sandbox: the resident process's socket, token and pid file.
    pub fn runtime_dir(&self) -> PathBuf {
        self.root.join("run")
    }

    /// The pid in this sandbox's server pid file, if there is one.
    pub fn server_pid(&self) -> Option<u32> {
        fs::read_to_string(self.runtime_dir().join("serve.pid"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    pub fn sandboxed(&self, mut cmd: std::process::Command) -> std::process::Command {
        let r = &self.root;
        let paths = [
            ("FBRAIN_CONFIG_DIR", "config"),
            ("FBRAIN_CACHE_DIR", "cache"),
            ("HOME", "home"),
            ("USERPROFILE", "home"),
            ("APPDATA", "appdata"),
            ("LOCALAPPDATA", "localappdata"),
            ("XDG_CONFIG_HOME", "xdg-config"),
            ("XDG_CACHE_HOME", "xdg-cache"),
            ("CLAUDE_CONFIG_DIR", "claude"),
            ("CODEX_HOME", "codex"),
            ("FBRAIN_RUNTIME_DIR", "run"),
        ];
        for (var, dir) in paths {
            cmd.env(var, r.join(dir));
        }
        let mut dirs = vec![r.join("bin")];
        dirs.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        cmd.env("PATH", std::env::join_paths(dirs).unwrap());
        cmd.env("FBRAIN_STUB_LOG", self.stub_log());
        cmd.env_remove("FBRAIN_BRAIN");
        cmd.env_remove("FBRAIN_HARNESS");
        // Tests run inside agent sessions; the markers those set must not reach the binary.
        for var in ["CLAUDECODE", "CLAUDE_CODE_CHILD_SESSION", "CLAUDE_ENV_FILE"] {
            cmd.env_remove(var);
        }
        // No test may reach the network; tests that need a model opt in with env_remove.
        cmd.env("FBRAIN_OFFLINE", "1");
        cmd.env_remove("FBRAIN_MODEL");
        for var in [
            "FBRAIN_SERVE_IDLE_SECS",
            "FBRAIN_TEST_SERVE_DELAY_MS",
            "FBRAIN_TEST_SERVE_FAIL",
            "FBRAIN_TEST_FAKE_VERSION",
        ] {
            cmd.env_remove(var);
        }
        for (k, v) in [
            ("GIT_AUTHOR_NAME", "Test"),
            ("GIT_AUTHOR_EMAIL", "test@example.com"),
            ("GIT_COMMITTER_NAME", "Test"),
            ("GIT_COMMITTER_EMAIL", "test@example.com"),
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "commit.gpgsign"),
            ("GIT_CONFIG_VALUE_0", "false"),
        ] {
            cmd.env(k, v);
        }
        cmd.current_dir(r);
        cmd
    }

    /// Copies `tests/fixtures/brains/<name>` into the sandbox and returns the copy.
    pub fn fixture_brain(&self, name: &str) -> PathBuf {
        // The bench crate includes this module too, so look upward for the fixtures.
        let src = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .map(|a| a.join("tests/fixtures/brains").join(name))
            .find(|p| p.exists())
            .expect("fixture brain");
        let dst = self.root.join(format!("fixture-{name}"));
        copy_dir(&src, &dst);
        dst
    }
}

/// Runs a command with a pseudo terminal as its stdin, the way Faisal runs it, and collects the
/// output through pipes. Callers set any agent marker on `cmd` themselves.
#[cfg(unix)]
pub fn owner_output(mut cmd: std::process::Command) -> std::process::Output {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::process::Stdio;
    let (mut m, mut s) = (0, 0);
    let rc = unsafe {
        libc::openpty(
            &mut m,
            &mut s,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, 0);
    let (_master, slave) = unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s)) };
    cmd.stdin(Stdio::from(slave));
    cmd.output().unwrap()
}

/// Stops the resident process this sandbox started, found through its own pid file only.
impl Drop for Sandbox {
    fn drop(&mut self) {
        for pid in sandbox_servers(self) {
            terminate(pid);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while alive(pid) && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            if alive(pid) {
                force_kill(pid);
            }
        }
    }
}

/// Servers of this sandbox. On Linux every live `fbrain serve` whose environment names this
/// sandbox's runtime dir, so a test that changed the pid file still cleans up; elsewhere the
/// pid file's server.
#[cfg(target_os = "linux")]
fn sandbox_servers(sb: &Sandbox) -> Vec<u32> {
    let mut out = Vec::new();
    for e in fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let cmd = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        if cmd.split(|b| *b == 0).any(|a| a == b"serve")
            && is_sandbox_server(pid, &sb.runtime_dir())
            && alive(pid)
        {
            out.push(pid);
        }
    }
    out
}

#[cfg(not(target_os = "linux"))]
fn sandbox_servers(sb: &Sandbox) -> Vec<u32> {
    sb.server_pid()
        .filter(|p| is_sandbox_server(*p, &sb.runtime_dir()))
        .into_iter()
        .collect()
}

/// Whether `pid` is a live (not zombie) process.
#[cfg(target_os = "linux")]
pub fn alive(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| {
        s.rsplit_once(") ")
            .is_some_and(|(_, rest)| !rest.starts_with('Z'))
    })
}

#[cfg(all(unix, not(target_os = "linux")))]
pub fn alive(pid: u32) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .is_ok_and(|o| {
            let s = String::from_utf8_lossy(&o.stdout);
            o.status.success() && !s.trim().is_empty() && !s.trim().starts_with('Z')
        })
}

#[cfg(windows)]
pub fn alive(pid: u32) -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")))
}

/// The pid belongs to a server of this sandbox, not to a process that reused the number.
#[cfg(target_os = "linux")]
fn is_sandbox_server(pid: u32, runtime: &Path) -> bool {
    let want = format!("FBRAIN_RUNTIME_DIR={}", runtime.display());
    fs::read(format!("/proc/{pid}/environ"))
        .is_ok_and(|e| e.split(|b| *b == 0).any(|kv| kv == want.as_bytes()))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn is_sandbox_server(pid: u32, _runtime: &Path) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .is_ok_and(|o| {
            let s = String::from_utf8_lossy(&o.stdout);
            s.contains("fbrain") && s.contains("serve")
        })
}

#[cfg(windows)]
fn is_sandbox_server(pid: u32, _runtime: &Path) -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("fbrain"))
}

#[cfg(unix)]
fn terminate(pid: u32) {
    let _ = std::process::Command::new("kill")
        .arg(pid.to_string())
        .status();
}

#[cfg(unix)]
fn force_kill(pid: u32) {
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status();
}

#[cfg(windows)]
fn terminate(pid: u32) {
    force_kill(pid)
}

#[cfg(windows)]
fn force_kill(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .output();
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for e in fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        let to = dst.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &to);
        } else {
            fs::copy(e.path(), to).unwrap();
        }
    }
}

#[cfg(unix)]
fn write_stub(bin: &Path, tool: &str) {
    use std::os::unix::fs::PermissionsExt;
    let p = bin.join(tool);
    fs::write(
        &p,
        format!("#!/bin/sh\necho \"{tool} $@\" >> \"$FBRAIN_STUB_LOG\"\n"),
    )
    .unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(windows)]
fn write_stub(bin: &Path, tool: &str) {
    fs::write(
        bin.join(format!("{tool}.cmd")),
        format!("@echo off\r\necho {tool} %* >> \"%FBRAIN_STUB_LOG%\"\r\n"),
    )
    .unwrap();
}

/// The `fbrain` binary: Cargo's path in this package's tests, else the one next to the test binary
/// (the bench crate's tests, built into the same target directory).
fn fbrain_exe() -> PathBuf {
    if let Some(p) = option_env!("CARGO_BIN_EXE_fbrain") {
        return p.into();
    }
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.join(format!("fbrain{}", std::env::consts::EXE_SUFFIX))
}
