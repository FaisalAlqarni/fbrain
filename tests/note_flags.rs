use fbrain::note::Note;

#[test]
fn note_with_broken_frontmatter_counts_as_private_and_protected() {
    let n = Note::parse("a.md", "---\nprivate: true\nprotected: [oops\n---\nbody\n");
    assert!(n.frontmatter_error.is_some());
    assert!(
        !n.fm.private,
        "default flags are false, so callers must use the helpers"
    );
    assert!(n.is_private());
    assert!(n.is_protected());
}

#[test]
fn note_with_valid_frontmatter_uses_its_flags() {
    let n = Note::parse("a.md", "---\nprivate: true\n---\nbody\n");
    assert!(n.is_private());
    assert!(!n.is_protected());
    let plain = Note::parse("b.md", "body\n");
    assert!(!plain.is_private() && !plain.is_protected());
}
