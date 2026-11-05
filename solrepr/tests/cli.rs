use std::process::{Command, Stdio};

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_solrepr"))
}

#[test]
fn version_prints_name_and_version() {
    let output = binary().arg("--version").output().expect("run solrepr");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        concat!("solrepr ", env!("CARGO_PKG_VERSION"), "\n")
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn short_version_flag_matches_long() {
    let short = binary().arg("-V").output().expect("run solrepr");
    let long = binary().arg("--version").output().expect("run solrepr");

    assert_eq!(short.stdout, long.stdout);
    assert!(short.status.success());
}

#[test]
fn help_lists_flags() {
    let output = binary().arg("--help").output().expect("run solrepr");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(output.status.success());
    assert!(stdout.contains("USAGE"));
    assert!(stdout.contains("--version"));
    assert!(stdout.contains("--stdio"));
}

#[test]
fn unknown_argument_errors() {
    let output = binary().arg("--nope").output().expect("run solrepr");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(stderr.contains("--nope"));
    assert!(stderr.contains("--help"));
}

#[test]
fn stdio_flag_is_accepted() {
    let output = binary()
        .arg("--stdio")
        .stdin(Stdio::null())
        .output()
        .expect("run solrepr");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!stderr.contains("unrecognized"));
    assert!(stderr.contains("starting"));
}

#[test]
fn no_arguments_enters_server_mode() {
    let output = binary()
        .stdin(Stdio::null())
        .output()
        .expect("run solrepr");

    assert!(!String::from_utf8_lossy(&output.stdout).contains("USAGE"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("unrecognized"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("starting"));
}
