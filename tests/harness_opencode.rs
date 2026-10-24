//! `fbrain install|uninstall opencode-v1|opencode-v2` through the CLI, and the installed plugin
//! files run under node. Every opencode config dir is inside the sandbox.
mod common;

use common::Sandbox;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

const FBRAIN: &str = env!("CARGO_BIN_EXE_fbrain");
const OPENCODE_JSON: &str = "{\"model\":\"x\",\"plugin\":[\"other\"]}";
const NOT_LOADED: &str = "fbrain memory not loaded: fbrain binary not found. Run fbrain doctor.";

fn config_dir(sb: &Sandbox) -> PathBuf {
    sb.root.join("xdg-config/opencode")
}

fn plugin(sb: &Sandbox) -> PathBuf {
    config_dir(sb).join("plugins/fbrain.js")
}

fn seed_json(sb: &Sandbox) -> PathBuf {
    let p = config_dir(sb).join("opencode.json");
    fs::create_dir_all(config_dir(sb)).unwrap();
    fs::write(&p, OPENCODE_JSON).unwrap();
    p
}

fn install(sb: &Sandbox, args: &[&str]) -> String {
    let o = sb
        .cmd()
        .arg("install")
        .args(args)
        .assert()
        .success()
        .get_output()
        .clone();
    String::from_utf8(o.stdout).unwrap()
}

fn records(sb: &Sandbox) -> Vec<Value> {
    let t = fs::read_to_string(sb.config_dir().join("installs.json")).unwrap_or("[]".into());
    serde_json::from_str(&t).unwrap()
}

#[test]
fn install_v1_writes_only_the_plugin_file_and_records_it() {
    let sb = Sandbox::new();
    let json = seed_json(&sb);
    install(&sb, &["opencode-v1"]);
    let js = fs::read_to_string(plugin(&sb)).unwrap();
    assert!(js.contains(&serde_json::to_string(FBRAIN).unwrap()), "{js}");
    assert!(js.contains("opencode-v1"));
    assert_eq!(fs::read_to_string(&json).unwrap(), OPENCODE_JSON);
    assert!(!config_dir(&sb).join("opencode.json.fbrain-bak").exists());
    let recs = records(&sb);
    assert_eq!(recs.len(), 1, "{recs:?}");
    assert_eq!(recs[0]["harness"], "opencode-v1");
    assert_eq!(recs[0]["files"].as_array().unwrap().len(), 1);
    assert_eq!(recs[0]["files"][0]["kind"], "file");
    assert_eq!(
        Path::new(recs[0]["files"][0]["path"].as_str().unwrap()),
        plugin(&sb)
    );
}

#[test]
fn install_twice_leaves_one_plugin_file_and_one_record() {
    let sb = Sandbox::new();
    install(&sb, &["opencode-v1"]);
    install(&sb, &["opencode-v1"]);
    let names: Vec<_> = fs::read_dir(plugin(&sb).parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("fbrain.js")]);
    let recs = records(&sb);
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0]["files"].as_array().unwrap().len(), 1);
}

#[test]
fn uninstall_removes_the_plugin_and_leaves_opencode_json_byte_identical() {
    let sb = Sandbox::new();
    let json = seed_json(&sb);
    install(&sb, &["opencode-v1"]);
    sb.cmd()
        .args(["uninstall", "opencode-v1"])
        .assert()
        .success();
    assert!(!plugin(&sb).exists());
    assert_eq!(fs::read_to_string(&json).unwrap(), OPENCODE_JSON);
    assert!(records(&sb).is_empty());
}

#[test]
fn install_v2_writes_the_v2_plugin_to_the_same_path() {
    let sb = Sandbox::new();
    let json = seed_json(&sb);
    install(&sb, &["opencode-v2"]);
    let js = fs::read_to_string(plugin(&sb)).unwrap();
    assert!(js.contains("session.hook"), "{js}");
    assert!(js.contains("--harness") && js.contains("opencode-v2"));
    assert!(!js.contains("opencode-v1"));
    assert_eq!(fs::read_to_string(&json).unwrap(), OPENCODE_JSON);
}

#[test]
fn switching_majors_keeps_one_plugin_and_one_record() {
    let sb = Sandbox::new();
    install(&sb, &["opencode-v1"]);
    install(&sb, &["opencode-v2"]);
    let recs = records(&sb);
    assert_eq!(recs.len(), 1, "{recs:?}");
    assert_eq!(recs[0]["harness"], "opencode-v2");
    assert!(fs::read_to_string(plugin(&sb))
        .unwrap()
        .contains("session.hook"));
    sb.cmd()
        .args(["uninstall", "opencode-v2"])
        .assert()
        .success();
    assert!(!plugin(&sb).exists());
}

#[test]
fn xdg_config_home_unset_falls_back_to_home_dot_config() {
    let sb = Sandbox::new();
    let o = sb
        .cmd()
        .env_remove("XDG_CONFIG_HOME")
        .args(["install", "opencode-v1"])
        .assert()
        .success()
        .get_output()
        .clone();
    let _ = o;
    assert!(sb
        .root
        .join("home/.config/opencode/plugins/fbrain.js")
        .exists());
}

#[test]
fn agents_md_adds_one_marked_block_and_uninstall_removes_only_it() {
    let sb = Sandbox::new();
    let agents = config_dir(&sb).join("AGENTS.md");
    fs::create_dir_all(config_dir(&sb)).unwrap();
    let original = "# My rules\n\nBe kind.\n";
    fs::write(&agents, original).unwrap();
    install(&sb, &["opencode-v1", "--agents-md"]);
    let after = fs::read_to_string(&agents).unwrap();
    assert_eq!(after.matches("<!-- fbrain:start -->").count(), 1, "{after}");
    assert_eq!(after.matches("<!-- fbrain:end -->").count(), 1);
    assert!(after.starts_with(original));
    assert!(after.contains(
        "At the start of a session, run fbrain brief --harness opencode-v1 and read its output."
    ));
    assert!(after.contains("Do not approve or reject proposals"));
    assert!(!after.contains("yes-i-am-faisal") && !after.contains("fm-set"));
    assert_eq!(
        fs::read_to_string(config_dir(&sb).join("AGENTS.md.fbrain-bak")).unwrap(),
        original
    );
    install(&sb, &["opencode-v1", "--agents-md"]);
    assert_eq!(fs::read_to_string(&agents).unwrap(), after);
    assert_eq!(records(&sb)[0]["files"].as_array().unwrap().len(), 2);
    sb.cmd()
        .args(["uninstall", "opencode-v1"])
        .assert()
        .success();
    assert_eq!(fs::read_to_string(&agents).unwrap(), original);
    assert!(!plugin(&sb).exists());
}

#[test]
fn agents_md_for_v2_names_the_v2_harness() {
    let sb = Sandbox::new();
    install(&sb, &["opencode-v2", "--agents-md"]);
    let after = fs::read_to_string(config_dir(&sb).join("AGENTS.md")).unwrap();
    assert!(
        after.contains("run fbrain brief --harness opencode-v2 and read"),
        "{after}"
    );
}

#[test]
fn agents_md_is_refused_for_harnesses_that_have_no_such_file() {
    let sb = Sandbox::new();
    let o = sb
        .cmd()
        .args(["install", "claude-code", "--agents-md"])
        .assert()
        .failure()
        .get_output()
        .clone();
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("applies to opencode-v1 and opencode-v2 only"),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
}

fn line(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn doctor_fails_when_the_plugin_file_is_gone() {
    let sb = Sandbox::new();
    install(&sb, &["opencode-v1"]);
    fs::remove_file(plugin(&sb)).unwrap();
    let o = sb.cmd().arg("doctor").assert().code(1).get_output().clone();
    let want = format!(
        "fail: harness:opencode-v1: missing {}",
        plugin(&sb).display()
    );
    assert!(line(&o).contains(&want), "{}", line(&o));
}

#[test]
fn doctor_is_ok_for_an_intact_install() {
    let sb = Sandbox::new();
    install(&sb, &["opencode-v1"]);
    let o = sb.cmd().arg("doctor").output().unwrap();
    assert!(
        line(&o).contains("ok: harness:opencode-v1: 1 entries present"),
        "{}",
        line(&o)
    );
}

#[cfg(unix)]
#[test]
fn doctor_warns_when_opencode_reports_another_major() {
    use std::os::unix::fs::PermissionsExt;
    let sb = Sandbox::new();
    install(&sb, &["opencode-v1"]);
    let stub = sb.root.join("bin/opencode");
    fs::write(&stub, "#!/bin/sh\necho 2.3.1\n").unwrap();
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    let o = sb.cmd().arg("doctor").output().unwrap();
    assert!(
        line(&o).contains("warn: harness:opencode-v1: opencode reports version 2.3.1"),
        "{}",
        line(&o)
    );
}

#[test]
fn doctor_fix_restores_a_deleted_plugin_with_the_recorded_binary() {
    let sb = Sandbox::new();
    install(&sb, &["opencode-v2"]);
    fs::remove_file(plugin(&sb)).unwrap();
    sb.cmd().args(["doctor", "--fix"]).output().unwrap();
    assert!(fs::read_to_string(plugin(&sb))
        .unwrap()
        .contains("session.hook"));
}

// ---- plugin behaviour under node ----

fn node() -> std::process::Command {
    let ok = std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    assert!(ok, "node not found: install node to run plugin tests");
    std::process::Command::new("node")
}

/// Runs the installed plugin for `harness` with the embedded binary replaced by `bin`.
/// `calls` is a list of session ids; returns what each call left in `output.system`.
fn run_plugin(sb: &Sandbox, harness: &str, bin: &Path, calls: &[&str]) -> Vec<Value> {
    run_plugin_with(sb, harness, bin, calls, None)
}

/// `timeout_ms` replaces the plugin's 15 s spawn timeout so a test need not wait for it.
fn run_plugin_with(
    sb: &Sandbox,
    harness: &str,
    bin: &Path,
    calls: &[&str],
    timeout_ms: Option<u32>,
) -> Vec<Value> {
    install(sb, &[harness]);
    let js = fs::read_to_string(plugin(sb)).unwrap().replace(
        &serde_json::to_string(FBRAIN).unwrap(),
        &serde_json::to_string(&bin.to_string_lossy()).unwrap(),
    );
    let js = match timeout_ms {
        Some(ms) => {
            assert!(js.contains("const TIMEOUT_MS = 15000;"));
            js.replace(
                "const TIMEOUT_MS = 15000;",
                &format!("const TIMEOUT_MS = {ms};"),
            )
        }
        None => js,
    };
    let dir = sb.root.join("node");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("plugin.mjs");
    fs::write(&file, js).unwrap();
    let driver = dir.join("driver.mjs");
    fs::write(
        &driver,
        r#"
import { pathToFileURL } from "node:url";
const [, , file, harness, cwd, ...sessions] = process.argv;
const m = await import(pathToFileURL(file).href);
const results = [];
if (harness === "opencode-v1") {
  const p = await m.fbrain({ directory: cwd });
  for (const s of sessions) {
    const output = { system: [] };
    await p["experimental.chat.system.transform"]({ sessionID: s }, output);
    results.push(output.system);
  }
} else {
  let handler;
  await m.default.setup({
    location: { directory: cwd },
    session: { hook: async (name, fn) => { if (name === "context") handler = fn; } },
    shell: { hook: async () => {} },
  });
  if (m.default.id !== "fbrain") throw new Error("id");
  for (const s of sessions) {
    const ev = { sessionID: s, system: [] };
    await handler(ev);
    results.push(ev.system);
  }
}
console.log(JSON.stringify(results));
"#,
    )
    .unwrap();
    let mut child = node()
        .arg(&driver)
        .arg(&file)
        .arg(harness)
        .arg(sb.root.join("session dir"))
        .args(calls)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let start = std::time::Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if start.elapsed() > std::time::Duration::from_secs(4) {
            child.kill().unwrap();
            panic!("the plugin did not answer within 4 s (it hangs)");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let o = child.wait_with_output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    serde_json::from_slice(&o.stdout).unwrap()
}

#[cfg(unix)]
fn fake_brief(sb: &Sandbox) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = sb.root.join("fake-fbrain");
    let log = sb.root.join("fake.log");
    fs::write(
        &p,
        format!(
            "#!/bin/sh\necho \"$* stdin=$(cat | wc -c | tr -d ' ')\" >> '{}'\necho PAYLOAD\n",
            log.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[cfg(unix)]
#[test]
fn v1_plugin_pushes_the_brief_once_per_session_with_cwd_and_closed_stdin() {
    let sb = Sandbox::new();
    let bin = fake_brief(&sb);
    let r = run_plugin(&sb, "opencode-v1", &bin, &["s1", "s1", "s2"]);
    assert_eq!(
        r,
        serde_json::json!([["PAYLOAD"], ["PAYLOAD"], ["PAYLOAD"]])
            .as_array()
            .unwrap()
            .clone()
    );
    let log = fs::read_to_string(sb.root.join("fake.log")).unwrap();
    let lines: Vec<_> = log.lines().collect();
    let cwd = sb.root.join("session dir");
    let want = format!(
        "brief --harness opencode-v1 --cwd {} stdin=0",
        cwd.display()
    );
    assert_eq!(
        lines,
        vec![want.as_str(), want.as_str()],
        "two sessions, two runs"
    );
}

#[cfg(unix)]
#[test]
fn v2_plugin_pushes_a_text_part_once_per_session_with_cwd_and_closed_stdin() {
    let sb = Sandbox::new();
    let bin = fake_brief(&sb);
    let r = run_plugin(&sb, "opencode-v2", &bin, &["s1", "s1"]);
    let part = serde_json::json!([{"type": "text", "text": "PAYLOAD"}]);
    assert_eq!(r, vec![part.clone(), part]);
    let log = fs::read_to_string(sb.root.join("fake.log")).unwrap();
    let cwd = sb.root.join("session dir");
    assert_eq!(
        log.lines().collect::<Vec<_>>(),
        vec![format!(
            "brief --harness opencode-v2 --cwd {} stdin=0",
            cwd.display()
        )]
    );
}

#[test]
fn plugins_show_the_not_loaded_notice_when_the_binary_is_missing() {
    let sb = Sandbox::new();
    let missing = sb.root.join("no such fbrain");
    let r = run_plugin(&sb, "opencode-v1", &missing, &["s1"]);
    assert_eq!(
        r,
        serde_json::json!([[NOT_LOADED]])
            .as_array()
            .unwrap()
            .clone()
    );
    let r = run_plugin(&sb, "opencode-v2", &missing, &["s1"]);
    assert_eq!(
        r,
        vec![serde_json::json!([{"type": "text", "text": NOT_LOADED}])]
    );
}

#[cfg(unix)]
fn script(sb: &Sandbox, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = sb.root.join(name);
    fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[cfg(unix)]
#[test]
fn plugins_give_up_on_a_binary_that_ignores_sigterm() {
    for h in ["opencode-v1", "opencode-v2"] {
        let sb = Sandbox::new();
        let bin = script(&sb, "stubborn", "trap '' TERM\nexec sleep 6");
        let start = std::time::Instant::now();
        let r = run_plugin_with(&sb, h, &bin, &["s1"], Some(300));
        assert!(start.elapsed().as_secs() < 3, "{h}: waited for the process");
        let text = r[0].to_string();
        assert!(text.contains(NOT_LOADED), "{h}: {text}");
    }
}

#[cfg(unix)]
#[test]
fn plugins_drop_a_brief_that_floods_past_the_output_cap() {
    for h in ["opencode-v1", "opencode-v2"] {
        let sb = Sandbox::new();
        let bin = script(&sb, "flood", "yes 0123456789abcdef | head -c 300000");
        let r = run_plugin(&sb, h, &bin, &["s1"]);
        let text = r[0].to_string();
        assert!(
            text.contains(NOT_LOADED),
            "{h}: {}",
            &text[..80.min(text.len())]
        );
        assert!(text.len() < 1000);
    }
}

// ---- AGENTS.md markers ----

const S: &str = "<!-- fbrain:start -->";
const E: &str = "<!-- fbrain:end -->";

fn agents(sb: &Sandbox) -> PathBuf {
    config_dir(sb).join("AGENTS.md")
}

fn seed_agents(sb: &Sandbox, text: &str) {
    fs::create_dir_all(config_dir(sb)).unwrap();
    fs::write(agents(sb), text).unwrap();
}

fn install_fails(sb: &Sandbox, args: &[&str]) -> String {
    let o = sb
        .cmd()
        .arg("install")
        .args(args)
        .assert()
        .failure()
        .get_output()
        .clone();
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn markers_mentioned_in_prose_are_left_alone_through_install_and_uninstall() {
    let sb = Sandbox::new();
    let original = format!("Wrap rules in {S} and {E} like so.\nand `{S}` alone.\n");
    seed_agents(&sb, &original);
    install(&sb, &["opencode-v1", "--agents-md"]);
    let after = fs::read_to_string(agents(&sb)).unwrap();
    assert!(after.starts_with(&original), "{after}");
    sb.cmd()
        .args(["uninstall", "opencode-v1"])
        .assert()
        .success();
    assert_eq!(fs::read_to_string(agents(&sb)).unwrap(), original);
}

#[test]
fn removal_is_byte_exact_whatever_the_final_newline_state() {
    for original in ["# rules", "# rules\n", "# rules\n\n\n", ""] {
        let sb = Sandbox::new();
        if !original.is_empty() {
            seed_agents(&sb, original);
        }
        install(&sb, &["opencode-v1", "--agents-md"]);
        sb.cmd()
            .args(["uninstall", "opencode-v1"])
            .assert()
            .success();
        let got = fs::read_to_string(agents(&sb)).unwrap_or_default();
        assert_eq!(got, original, "{original:?}");
    }
}

#[test]
fn a_malformed_marker_structure_is_refused_and_nothing_is_written() {
    let block = format!("{S}\nold\n{E}\n");
    for original in [
        format!("{E}\nx\n{S}\n"),
        format!("{block}{block}"),
        format!("{S}\nno end\n"),
    ] {
        let sb = Sandbox::new();
        seed_agents(&sb, &original);
        let err = install_fails(&sb, &["opencode-v1", "--agents-md"]);
        assert!(
            err.contains("AGENTS.md") && err.contains("fbrain:start"),
            "{err}"
        );
        assert_eq!(fs::read_to_string(agents(&sb)).unwrap(), original);
        assert!(!plugin(&sb).exists(), "plugin written before the refusal");
        assert!(records(&sb).is_empty());
    }
}

#[test]
fn doctor_flags_a_block_that_was_duplicated_after_install() {
    let sb = Sandbox::new();
    install(&sb, &["opencode-v1", "--agents-md"]);
    let t = fs::read_to_string(agents(&sb)).unwrap();
    fs::write(agents(&sb), format!("{t}{t}")).unwrap();
    let o = sb.cmd().arg("doctor").output().unwrap();
    let out = line(&o);
    assert!(
        out.lines()
            .any(|l| l.starts_with("fail: harness:opencode-v1:") && l.contains("AGENTS.md")),
        "{out}"
    );
}

// ---- uninstall only removes what is ours ----

#[test]
fn uninstall_keeps_a_plugin_file_the_user_replaced_and_says_so() {
    let sb = Sandbox::new();
    install(&sb, &["opencode-v1"]);
    fs::write(plugin(&sb), "export default {}; // mine\n").unwrap();
    let o = sb
        .cmd()
        .args(["uninstall", "opencode-v1"])
        .assert()
        .success()
        .get_output()
        .clone();
    assert_eq!(
        fs::read_to_string(plugin(&sb)).unwrap(),
        "export default {}; // mine\n"
    );
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(
        err.contains("warning") && err.contains(&plugin(&sb).display().to_string()),
        "{err}"
    );
    assert!(records(&sb).is_empty());
}

#[test]
fn uninstall_never_deletes_a_recorded_path_that_is_not_a_plugin_file() {
    let sb = Sandbox::new();
    install(&sb, &["opencode-v1"]);
    let victim = sb.root.join("home/precious.txt");
    fs::write(&victim, "// fbrain plugin for opencode\n").unwrap();
    let path = sb.config_dir().join("installs.json");
    let mut recs: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    recs[0]["files"][0]["path"] = victim.to_string_lossy().to_string().into();
    fs::write(&path, recs.to_string()).unwrap();
    sb.cmd()
        .args(["uninstall", "opencode-v1"])
        .assert()
        .success();
    assert!(victim.exists());
}

// ---- backups and links ----

fn backups(dir: &Path, name: &str) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(&format!("{name}.fbrain-bak")))
        .map(|n| fs::read_to_string(dir.join(n)).unwrap())
        .collect();
    v.sort();
    v
}

#[test]
fn an_existing_backup_is_never_overwritten_or_made_the_reason_to_skip_one() {
    let sb = Sandbox::new();
    seed_agents(&sb, "first\n");
    install(&sb, &["opencode-v1", "--agents-md"]);
    // The user edits their own text, then installs again after an uninstall.
    sb.cmd()
        .args(["uninstall", "opencode-v1"])
        .assert()
        .success();
    seed_agents(&sb, "second, edited later\n");
    install(&sb, &["opencode-v1", "--agents-md"]);
    let all = backups(&config_dir(&sb), "AGENTS.md");
    assert!(all.contains(&"first\n".to_string()), "{all:?}");
    assert!(
        all.contains(&"second, edited later\n".to_string()),
        "{all:?}"
    );
}

#[test]
fn a_foreign_plugin_is_backed_up_even_when_an_old_backup_exists() {
    let sb = Sandbox::new();
    let dir = plugin(&sb).parent().unwrap().to_path_buf();
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("fbrain.js.fbrain-bak"), "stale backup").unwrap();
    fs::write(plugin(&sb), "// somebody else's plugin\n").unwrap();
    install(&sb, &["opencode-v1"]);
    let all = backups(&dir, "fbrain.js");
    assert!(
        all.contains(&"// somebody else's plugin\n".to_string()),
        "{all:?}"
    );
    assert!(all.contains(&"stale backup".to_string()));
}

#[test]
fn reinstalling_unchanged_adds_no_backup() {
    let sb = Sandbox::new();
    seed_agents(&sb, "mine\n");
    install(&sb, &["opencode-v1", "--agents-md"]);
    install(&sb, &["opencode-v1", "--agents-md"]);
    assert_eq!(
        backups(&config_dir(&sb), "AGENTS.md"),
        vec!["mine\n".to_string()]
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_agents_md_or_plugin_is_refused_not_replaced() {
    use std::os::unix::fs::symlink;
    let sb = Sandbox::new();
    let real = sb.root.join("dotfiles-AGENTS.md");
    fs::write(&real, "kept\n").unwrap();
    fs::create_dir_all(config_dir(&sb)).unwrap();
    symlink(&real, agents(&sb)).unwrap();
    let err = install_fails(&sb, &["opencode-v1", "--agents-md"]);
    assert!(
        err.contains("symbolic link") && err.contains("AGENTS.md"),
        "{err}"
    );
    assert!(fs::symlink_metadata(agents(&sb))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_to_string(&real).unwrap(), "kept\n");

    let sb = Sandbox::new();
    let real = sb.root.join("dotfiles-plugin.js");
    fs::write(&real, "kept\n").unwrap();
    fs::create_dir_all(plugin(&sb).parent().unwrap()).unwrap();
    symlink(&real, plugin(&sb)).unwrap();
    let err = install_fails(&sb, &["opencode-v1"]);
    assert!(
        err.contains("symbolic link") && err.contains("fbrain.js"),
        "{err}"
    );
    assert!(fs::symlink_metadata(plugin(&sb))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_to_string(&real).unwrap(), "kept\n");
}
