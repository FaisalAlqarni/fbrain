//! Seam 1: help and version output.
mod common;
use common::Sandbox;

const COMMANDS: [&str; 16] = [
    "init",
    "brains",
    "get",
    "search",
    "reindex",
    "propose",
    "inbox",
    "review",
    "lint",
    "brief",
    "install",
    "uninstall",
    "doctor",
    "mcp",
    "onboard",
    "interview",
];

fn out(args: &[&str]) -> String {
    let sb = Sandbox::new();
    let o = sb
        .cmd()
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(o).unwrap()
}

#[test]
fn version_prints_the_cargo_version() {
    assert_eq!(
        out(&["--version"]).trim(),
        concat!("fbrain ", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn help_lists_exactly_the_public_commands() {
    let help = out(&["--help"]);
    let mut listed: Vec<&str> = help
        .lines()
        .skip_while(|l| *l != "Commands:")
        .skip(1)
        .take_while(|l| l.starts_with("  "))
        .filter_map(|l| l.split_whitespace().next())
        .filter(|c| *c != "help")
        .collect();
    listed.sort();
    let mut want = COMMANDS.to_vec();
    want.sort();
    assert_eq!(listed, want);
}

// Ruling (state.md, security lens 09): the flag is hidden from help and documented in the README.
#[test]
fn override_flag_is_in_the_readme_but_not_in_review_help() {
    assert!(!out(&["review", "--help"]).contains("yes-i-am-faisal"));
    let readme =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md")).unwrap();
    assert!(readme.contains("--yes-i-am-faisal"));
}

#[test]
fn help_and_docs_contain_no_em_or_en_dashes() {
    let mut all = String::new();
    for c in COMMANDS {
        all += &out(&[c, "--help"]);
    }
    all += &out(&["--help"]);
    for f in ["README.md", "CHANGELOG.md"] {
        let p = concat!(env!("CARGO_MANIFEST_DIR"), "/");
        all +=
            &std::fs::read_to_string(format!("{p}{f}")).unwrap_or_else(|_| panic!("{f} missing"));
    }
    assert!(!all.contains('\u{2013}') && !all.contains('\u{2014}'));
}
