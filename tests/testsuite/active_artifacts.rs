//! Behavioral tests for complete-unit retention, publication, and eviction.
use crate::prelude::*;
use cargo_test_support::{Execs, Project, project};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

fn fixture() -> Project {
    project()
        .file("Cargo.toml", r#"
[package]
name = "foo"
version = "0.1.0"
edition = "2024"
[features]
first = []
second = []
[dependencies]
shared = { path = "shared" }
"#)
        .file("src/lib.rs", "/// ```\n/// assert_eq!(foo::answer(), 42);\n/// ```\npub fn answer() -> u32 { let warning = 1; shared::answer() }\n#[test] fn test_answer() { assert_eq!(answer(), 42); }")
        .file("src/main.rs", "fn main() { assert_eq!(foo::answer(), 42); }")
        .file("shared/Cargo.toml", "[package]\nname = 'shared'\nversion = '0.1.0'\nedition = '2024'")
        .file("shared/src/lib.rs", "pub fn answer() -> u32 { 42 }")
        .build()
}

fn cargo(p: &Project, command: &str, session: &str) -> Execs {
    let mut e = p.cargo(command);
    e.arg("-Zactive-artifacts")
        .env("CARGO_BUILD_ARTIFACT_SESSION", session)
        .env("CARGO_INCREMENTAL", "1")
        .env("CARGO_PROFILE_DEV_DEBUG", "2")
        .env("CARGO_PROFILE_TEST_DEBUG", "2")
        .env("CARGO_PROFILE_DEV_SPLIT_DEBUGINFO", "unpacked")
        .env("CARGO_PROFILE_TEST_SPLIT_DEBUGINFO", "unpacked")
        .masquerade_as_nightly_cargo(&["active artifact retention"]);
    e
}

fn state(p: &Project) -> Value {
    serde_json::from_slice(
        &fs::read(p.root().join("target/.cargo-active-artifacts/state.json")).unwrap(),
    )
    .unwrap()
}

fn units(p: &Project, session: &str) -> BTreeSet<PathBuf> {
    state(p)["sessions"][session]["slots"]
        .as_object()
        .unwrap()
        .values()
        .flat_map(|v| {
            v.as_array()
                .unwrap()
                .iter()
                .map(|p| PathBuf::from(p.as_str().unwrap()))
        })
        .collect()
}

fn fresh(p: &Project, command: &str, session: &str) {
    let out = cargo(p, command, session)
        .arg("--message-format=json")
        .exec_with_output()
        .unwrap();
    let messages: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let artifacts: Vec<_> = messages
        .iter()
        .filter(|v| v["reason"] == "compiler-artifact")
        .collect();
    assert!(!artifacts.is_empty());
    assert!(artifacts.iter().all(|v| v["fresh"] == true), "{messages:?}");
}

#[cargo_test]
fn commands_stay_warm_and_incremental_is_owned() {
    let p = fixture();
    for cmd in ["check", "build", "test --no-run", "doc --no-deps"] {
        cargo(&p, cmd, "agent").run();
    }
    assert_eq!(
        state(&p)["sessions"]["agent"]["slots"]
            .as_object()
            .unwrap()
            .len(),
        4
    );
    for cmd in ["check", "build", "test --no-run"] {
        fresh(&p, cmd, "agent");
    }
    cargo(&p, "test", "agent").run(); // includes the doctest runtime oracle
    cargo(&p, "run", "agent").run();
    let current = units(&p, "agent");
    assert!(current.iter().any(|dir| {
        p.root()
            .join("target")
            .join(dir)
            .join("incremental")
            .exists()
    }));
    assert_eq!(
        fs::read_dir(p.root().join("target/debug/incremental"))
            .unwrap()
            .count(),
        0
    );
    p.change_file("shared/src/lib.rs", "pub fn answer() -> u32 { 40 + 2 }");
    cargo(&p, "test", "agent").run();
}

#[cargo_test]
fn switching_features_collects_old_units_and_reuses_shared_units() {
    let p = fixture();
    cargo(&p, "build --features first", "agent").run();
    let old = units(&p, "agent");
    cargo(&p, "build --features second", "agent").run();
    let new = units(&p, "agent");
    assert!(!old.difference(&new).collect::<Vec<_>>().is_empty());
    for dir in old.difference(&new) {
        assert!(!p.root().join("target").join(dir).exists(), "{dir:?}");
    }
    assert!(!old.intersection(&new).collect::<Vec<_>>().is_empty());
    fresh(&p, "build --features second", "agent");
    let output = cargo(&p, "build --features first", "agent")
        .arg("--message-format=json")
        .exec_with_output()
        .unwrap();
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("\"fresh\":false")
    );
}

#[cargo_test]
fn other_sessions_and_published_outputs_survive() {
    let p = fixture();
    cargo(&p, "check --features first", "alice").run();
    let alice = units(&p, "alice");
    cargo(&p, "check --features second", "bob").run();
    cargo(&p, "check", "bob").run();
    for dir in &alice {
        assert!(p.root().join("target").join(dir).exists());
    }
    fresh(&p, "check --features first", "alice");
    cargo(&p, "build --features first", "alice").run();
    let published: BTreeSet<PathBuf> = state(&p)["outputs"]
        .as_object()
        .unwrap()
        .values()
        .flat_map(|v| {
            v.as_array()
                .unwrap()
                .iter()
                .map(|p| PathBuf::from(p.as_str().unwrap()))
        })
        .collect();
    cargo(&p, "check --features second", "alice").run();
    for dir in &published {
        assert!(p.root().join("target").join(dir).exists());
    }
    assert!(
        state(&p)["outputs"]
            .as_object()
            .unwrap()
            .contains_key(p.bin("foo").to_str().unwrap())
    );
    cargo(&p, "clean --artifact-session alice", "alice").run();
    assert!(p.bin("foo").exists());
    for dir in &published {
        assert!(p.root().join("target").join(dir).exists());
    }
}

#[cargo_test]
fn failed_compilation_does_not_replace_successful_roots() {
    let p = fixture();
    cargo(&p, "check --features first", "agent").run();
    let before = state(&p)["sessions"].clone();
    p.change_file("src/main.rs", "fn main() { missing(); }");
    assert!(
        cargo(&p, "check --features second", "agent")
            .exec_with_output()
            .is_err()
    );
    assert_eq!(state(&p)["sessions"], before);
    for dir in units(&p, "agent") {
        assert!(p.root().join("target").join(dir).exists());
    }
    p.change_file(
        "src/main.rs",
        "fn main() { assert_eq!(foo::answer(), 42); }",
    );
    cargo(&p, "check --features second", "agent").run();
}

#[cargo_test]
fn release_dry_run_and_unowned_artifacts() {
    let p = fixture();
    cargo(&p, "check", "agent").run();
    let before = state(&p);
    let unowned = p.root().join("target/debug/build/unowned/1234567890abcdef");
    fs::create_dir_all(&unowned).unwrap();
    fs::write(unowned.join("keep"), "not registered").unwrap();
    cargo(&p, "clean --artifact-session agent --dry-run", "agent").run();
    assert_eq!(state(&p), before);
    let current = units(&p, "agent");
    cargo(&p, "clean --artifact-session agent", "agent").run();
    for dir in current {
        assert!(!p.root().join("target").join(dir).exists());
    }
    assert!(unowned.join("keep").exists());
}

#[cargo_test]
fn execution_lease_defers_eviction_until_a_later_collection() {
    let p = fixture();
    cargo(&p, "check --features first", "agent").run();
    let old = units(&p, "agent");
    let lease_root = p.root().join("target/.cargo-active-artifacts/leases");
    let handles: Vec<_> = fs::read_dir(&lease_root)
        .unwrap()
        .map(|entry| {
            let file = fs::File::open(entry.unwrap().path()).unwrap();
            file.lock_shared().unwrap();
            file
        })
        .collect();
    cargo(&p, "check --features second", "agent").run();
    let new = units(&p, "agent");
    for dir in old.difference(&new) {
        assert!(p.root().join("target").join(dir).exists());
    }
    drop(handles);
    fresh(&p, "check --features second", "agent");
    for dir in old.difference(&new) {
        assert!(!p.root().join("target").join(dir).exists());
    }
}

#[cargo_test]
fn flags_change_configuration() {
    let p = fixture();
    cargo(&p, "check", "agent")
        .env("RUSTFLAGS", "--cfg old_config")
        .run();
    let old = units(&p, "agent");
    cargo(&p, "check", "agent")
        .env("RUSTFLAGS", "--cfg new_config")
        .run();
    for dir in old.difference(&units(&p, "agent")) {
        assert!(!p.root().join("target").join(dir).exists());
    }
}

#[cargo_test]
fn corrupt_state_refuses_collection() {
    let p = fixture();
    cargo(&p, "check", "agent").run();
    let before = units(&p, "agent");
    fs::write(
        p.root().join("target/.cargo-active-artifacts/state.json"),
        "{invalid",
    )
    .unwrap();
    assert!(
        cargo(&p, "check --features first", "agent")
            .exec_with_output()
            .is_err()
    );
    for dir in before {
        assert!(p.root().join("target").join(dir).exists());
    }
}

#[cargo_test]
fn separate_build_directory_and_explicit_target() {
    let p = fixture();
    p.change_file(".cargo/config.toml", "[build]\nbuild-dir = 'intermediates'");
    let target = cargo_test_support::rustc_host();
    for cmd in ["build", "check", "test --no-run"] {
        cargo(&p, cmd, "agent").arg("--target").arg(&target).run();
    }
    let s: Value = serde_json::from_slice(
        &fs::read(
            p.root()
                .join("intermediates/.cargo-active-artifacts/state.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        s["sessions"]["agent"]["slots"].as_object().unwrap().len(),
        3
    );
    for path in s["units"].as_object().unwrap().keys() {
        assert!(p.root().join("intermediates").join(path).exists());
    }
    for cmd in ["build", "check", "test --no-run"] {
        cargo(&p, cmd, "agent").arg("--target").arg(&target).run();
    }
}

#[cargo_test]
fn running_test_keeps_its_units_through_a_configuration_change() {
    use std::time::{Duration, Instant};
    let p = fixture();
    p.change_file(
        "tests/sleeper.rs",
        r#"
#[test] fn sleeper() {
    std::fs::write("started", "ready").unwrap();
    let start = std::time::Instant::now();
    while !std::path::Path::new("finished").exists() {
        assert!(start.elapsed().as_secs() < 15);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
"#,
    );
    let mut child = cargo(&p, "test --test sleeper --features first", "agent")
        .build_command()
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !p.root().join("started").exists() {
        if start.elapsed() > Duration::from_secs(15) {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("test did not start");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let sleeper = units(&p, "agent")
        .into_iter()
        .find(|dir| {
            fs::read_dir(p.root().join("target").join(dir).join("out")).is_ok_and(|mut entries| {
                entries.any(|e| {
                    e.unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with("sleeper")
                })
            })
        })
        .unwrap();
    cargo(&p, "check --features second", "agent").run();
    assert!(p.root().join("target").join(&sleeper).exists());
    p.change_file("finished", "done");
    assert!(child.wait().unwrap().success());
    fresh(&p, "check --features second", "agent");
    assert!(!p.root().join("target").join(sleeper).exists());
}

#[cargo_test]
fn invalid_unit_paths_are_rejected() {
    let p = fixture();
    cargo(&p, "check", "agent").run();
    let original = units(&p, "agent");
    let mut s = state(&p);
    s["units"]
        .as_object_mut()
        .unwrap()
        .insert("../outside/build/foo/1234567890abcdef".into(), Value::Null);
    fs::write(
        p.root().join("target/.cargo-active-artifacts/state.json"),
        serde_json::to_vec(&s).unwrap(),
    )
    .unwrap();
    assert!(
        cargo(&p, "clean --artifact-session agent", "agent")
            .exec_with_output()
            .is_err()
    );
    for dir in original {
        assert!(p.root().join("target").join(dir).exists());
    }
}

#[cargo_test]
fn profile_setting_changes_configuration() {
    let p = fixture();
    cargo(&p, "check", "agent")
        .env("CARGO_PROFILE_DEV_OPT_LEVEL", "0")
        .run();
    let old = units(&p, "agent");
    cargo(&p, "check", "agent")
        .env("CARGO_PROFILE_DEV_OPT_LEVEL", "1")
        .run();
    let new = units(&p, "agent");
    assert!(!old.difference(&new).collect::<Vec<_>>().is_empty());
    for dir in old.difference(&new) {
        assert!(!p.root().join("target").join(dir).exists());
    }
}

fn wait_for_marker(p: &Project, name: &str, child: &mut std::process::Child) {
    let start = std::time::Instant::now();
    while !p.root().join(name).exists() {
        if start.elapsed().as_secs() > 20 {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child did not write {name}");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cargo_test]
fn narrower_check_replaces_its_slot() {
    let p = fixture();
    cargo(&p, "check --all-targets", "agent").run();
    let wide = units(&p, "agent");
    cargo(&p, "check", "agent").run();
    let narrow = units(&p, "agent");
    assert!(!wide.difference(&narrow).collect::<Vec<_>>().is_empty());
    for dir in wide.difference(&narrow) {
        assert!(!p.root().join("target").join(dir).exists());
    }
    fresh(&p, "check", "agent");
}

#[cargo_test]
fn explicit_slots_keep_two_target_selections_warm() {
    let p = fixture();
    cargo(&p, "check --all-targets", "agent")
        .env("CARGO_BUILD_ARTIFACT_SLOT", "wide")
        .run();
    let wide = units(&p, "agent");
    cargo(&p, "check", "agent")
        .env("CARGO_BUILD_ARTIFACT_SLOT", "narrow")
        .run();
    assert_eq!(
        state(&p)["sessions"]["agent"]["slots"]
            .as_object()
            .unwrap()
            .len(),
        2
    );
    for dir in wide {
        assert!(p.root().join("target").join(dir).exists());
    }
    cargo(&p, "check --all-targets", "agent")
        .env("CARGO_BUILD_ARTIFACT_SLOT", "wide")
        .arg("--message-format=json")
        .run();
}

#[cargo_test]
fn explicit_generation_replaces_other_slots_without_forcing_rebuild() {
    let p = fixture();
    cargo(&p, "check", "agent").run();
    cargo(&p, "test --no-run", "agent").run();
    let before = state(&p)["sessions"]["agent"]["configuration"].clone();
    cargo(&p, "check", "agent")
        .env("CARGO_BUILD_ARTIFACT_CONFIGURATION", "new-sdk")
        .run();
    let after = state(&p);
    assert_ne!(after["sessions"]["agent"]["configuration"], before);
    assert_eq!(
        after["sessions"]["agent"]["slots"]
            .as_object()
            .unwrap()
            .len(),
        1
    );
}

#[cargo_test]
fn custom_profile_switch_collects_old_profile_units() {
    let p = fixture();
    p.change_file(
        "Cargo.toml",
        &format!(
            "{}\n[profile.one]\ninherits='dev'\n[profile.two]\ninherits='dev'\n",
            p.read_file("Cargo.toml")
        ),
    );
    cargo(&p, "check --profile one", "agent").run();
    let old = units(&p, "agent");
    cargo(&p, "check --profile two", "agent").run();
    for dir in old {
        assert!(!p.root().join("target").join(dir).exists());
    }
    fresh(&p, "check --profile two", "agent");
}

#[cargo_test]
fn denied_warnings_preserve_successful_roots() {
    let p = fixture();
    cargo(&p, "check --features first", "agent").run();
    let before = state(&p)["sessions"].clone();
    assert!(
        cargo(&p, "check --features second", "agent")
            .env("CARGO_BUILD_WARNINGS", "deny")
            .exec_with_output()
            .is_err()
    );
    assert_eq!(state(&p)["sessions"], before);
    for dir in units(&p, "agent") {
        assert!(p.root().join("target").join(dir).exists());
    }
}

#[cargo_test]
fn denied_warnings_preserve_newly_published_output_backing() {
    let p = fixture();
    p.change_file(
        "src/lib.rs",
        &p.read_file("src/lib.rs").replace("let warning = 1; ", ""),
    );
    p.change_file(
        "src/main.rs",
        "fn main() { let warning = 1; println!(\"{}\", if cfg!(feature = \"second\") { \"second\" } else { \"first\" }); }",
    );
    cargo(&p, "build --features first", "agent").run();
    let before = state(&p)["sessions"].clone();
    let old = state(&p)["outputs"][p.bin("foo").to_str().unwrap()].clone();
    assert!(
        cargo(&p, "build --features second", "agent")
            .env("CARGO_BUILD_WARNINGS", "deny")
            .exec_with_output()
            .is_err()
    );
    p.process(p.bin("foo")).with_stdout_data("second\n").run();
    assert_eq!(state(&p)["sessions"], before);
    let pinned = state(&p)["outputs"][p.bin("foo").to_str().unwrap()].clone();
    assert!(pinned.as_array().unwrap().len() > old.as_array().unwrap().len());
    cargo(&p, "check", "agent").run();
    for dir in pinned.as_array().unwrap() {
        assert!(p.root().join("target").join(dir.as_str().unwrap()).exists());
    }
    p.process(p.bin("foo")).with_stdout_data("second\n").run();
    cargo(&p, "build --features second", "agent").run();
    assert!(
        state(&p)["outputs"][p.bin("foo").to_str().unwrap()]
            .as_array()
            .unwrap()
            .len()
            < pinned.as_array().unwrap().len()
    );
    fresh(&p, "build --features second", "agent");
}

#[cargo_test]
fn atomic_state_write_failure_preserves_prior_artifacts() {
    let p = fixture();
    cargo(&p, "check", "agent").run();
    let before = state(&p);
    let old = units(&p, "agent");
    let path = p.root().join("target/.cargo-active-artifacts/state.json");
    fs::rename(&path, path.with_extension("saved")).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(
        cargo(&p, "check --features second", "agent")
            .exec_with_output()
            .is_err()
    );
    for dir in old {
        assert!(p.root().join("target").join(dir).exists());
    }
    fs::remove_dir(&path).unwrap();
    fs::rename(path.with_extension("saved"), &path).unwrap();
    assert_eq!(state(&p), before);
    fresh(&p, "check", "agent");
}

#[cargo_test]
fn locked_profile_defers_release_collection() {
    let p = fixture();
    cargo(&p, "check", "agent").run();
    let old = units(&p, "agent");
    let file = fs::File::open(p.root().join("target/debug/.cargo-build-lock")).unwrap();
    file.lock_shared().unwrap();
    cargo(&p, "clean --artifact-session agent", "agent").run();
    assert!(state(&p)["sessions"].as_object().unwrap().is_empty());
    for dir in &old {
        assert!(p.root().join("target").join(dir).exists());
    }
    drop(file);
    cargo(&p, "clean --artifact-session agent", "agent").run();
    for dir in &old {
        assert!(!p.root().join("target").join(dir).exists());
    }
}

#[cargo_test]
fn unknown_version_refuses_collection() {
    let p = fixture();
    cargo(&p, "check", "agent").run();
    let old = units(&p, "agent");
    let mut s = state(&p);
    s["version"] = 999.into();
    fs::write(
        p.root().join("target/.cargo-active-artifacts/state.json"),
        serde_json::to_vec(&s).unwrap(),
    )
    .unwrap();
    assert!(
        cargo(&p, "clean --artifact-session agent", "agent")
            .exec_with_output()
            .is_err()
    );
    for dir in old {
        assert!(p.root().join("target").join(dir).exists());
    }
}

#[cargo_test]
fn invalid_execution_lease_key_refuses_collection() {
    let p = fixture();
    cargo(&p, "check", "agent").run();
    let old = units(&p, "agent");
    let mut s = state(&p);
    let graph = s["executions"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .clone();
    s["executions"] = serde_json::json!({"../../outside": graph});
    fs::write(
        p.root().join("target/.cargo-active-artifacts/state.json"),
        serde_json::to_vec(&s).unwrap(),
    )
    .unwrap();
    assert!(
        cargo(&p, "clean --artifact-session agent", "agent")
            .exec_with_output()
            .is_err()
    );
    for dir in old {
        assert!(p.root().join("target").join(dir).exists());
    }
}

#[cargo_test]
#[cfg(unix)]
fn wide_graph_works_with_a_small_file_limit() {
    use std::os::unix::process::CommandExt;
    let mut manifest = String::from(
        "[workspace]\n[package]\nname='wide'\nversion='0.1.0'\nedition='2024'\n[dependencies]\n",
    );
    for i in 0..160 {
        manifest.push_str(&format!("dep_{i}={{path='deps/dep_{i}'}}\n"));
    }
    let p = project()
        .file("Cargo.toml", &manifest)
        .file("src/lib.rs", "pub fn value()->u64 { dep_0::value() }\n")
        .build();
    for i in 0..160 {
        p.change_file(
            format!("deps/dep_{i}/Cargo.toml"),
            &format!("[package]\nname='dep_{i}'\nversion='0.1.0'\nedition='2024'\n"),
        );
        p.change_file(
            format!("deps/dep_{i}/src/lib.rs"),
            "pub fn value()->u64 {42}\n",
        );
    }
    let mut command = cargo(&p, "check -j2", "agent").build_command();
    // SAFETY: pre_exec only calls the async-signal-safe setrlimit syscall.
    unsafe {
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 64,
                rlim_max: 64,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(units(&p, "agent").len(), 161);
    assert_eq!(state(&p)["executions"].as_object().unwrap().len(), 1);
    let leases = p.root().join("target/.cargo-active-artifacts/leases");
    assert_eq!(fs::read_dir(&leases).unwrap().count(), 1);
    fresh(&p, "check", "agent");
    cargo(&p, "clean --artifact-session agent", "agent").run();
    assert_eq!(fs::read_dir(&leases).unwrap().count(), 0);
}

#[cargo_test]
fn execution_graph_payload_must_match_lease_key() {
    let p = fixture();
    cargo(&p, "check", "agent").run();
    let old = units(&p, "agent");
    let mut s = state(&p);
    *s["executions"]
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap() = serde_json::json!([]);
    fs::write(
        p.root().join("target/.cargo-active-artifacts/state.json"),
        serde_json::to_vec(&s).unwrap(),
    )
    .unwrap();
    assert!(
        cargo(&p, "clean --artifact-session agent", "agent")
            .exec_with_output()
            .is_err()
    );
    for dir in old {
        assert!(p.root().join("target").join(dir).exists());
    }
}

#[cargo_test]
#[cfg(unix)]
fn symlinked_unit_refuses_collection() {
    let p = fixture();
    cargo(&p, "check", "agent").run();
    let dir = units(&p, "agent").into_iter().next().unwrap();
    let original = p.root().join("target").join(dir);
    let outside = p.root().join("outside");
    fs::rename(&original, &outside).unwrap();
    fs::write(outside.join("keep"), "sentinel").unwrap();
    std::os::unix::fs::symlink(&outside, &original).unwrap();
    assert!(
        cargo(&p, "clean --artifact-session agent", "agent")
            .exec_with_output()
            .is_err()
    );
    assert_eq!(
        fs::read_to_string(outside.join("keep")).unwrap(),
        "sentinel"
    );
}

#[cargo_test]
fn feature_off_rewrite_is_conservatively_retained() {
    let p = fixture();
    cargo(&p, "check --features first", "agent").run();
    let old = units(&p, "agent");
    p.change_file(
        "src/lib.rs",
        "pub fn answer() -> u32 { shared::answer() }\npub fn new_item() {}\n",
    );
    p.cargo("check --features first")
        .env("CARGO_INCREMENTAL", "1")
        .env("CARGO_PROFILE_DEV_DEBUG", "2")
        .env("CARGO_PROFILE_TEST_DEBUG", "2")
        .env("CARGO_PROFILE_DEV_SPLIT_DEBUGINFO", "unpacked")
        .env("CARGO_PROFILE_TEST_SPLIT_DEBUGINFO", "unpacked")
        .run();
    cargo(&p, "check --features second", "agent").run();
    let new = units(&p, "agent");
    assert!(
        old.difference(&new)
            .any(|dir| p.root().join("target").join(dir).exists())
    );
    cargo(&p, "check --features first", "agent").run();
    cargo(&p, "check --features second", "agent").run();
    for dir in old.difference(&units(&p, "agent")) {
        assert!(!p.root().join("target").join(dir).exists());
    }
}

#[cargo_test]
fn interrupted_compilation_preserves_and_recovers_previous_configuration() {
    let p = fixture();
    p.change_file(
        "build.rs",
        r#"
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var_os("CARGO_FEATURE_SECOND").is_some() {
        let root = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
        std::fs::write(root.join("started"), "ready").unwrap();
        let start = std::time::Instant::now();
        while !root.join("finished").exists() {
            assert!(start.elapsed().as_secs() < 30);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::fs::write(root.join("script-finished"), "done").unwrap();
    }
}
"#,
    );
    cargo(&p, "check --features first", "agent").run();
    let before = state(&p)["sessions"].clone();
    let old = units(&p, "agent");
    let mut child = cargo(&p, "check --features second", "agent")
        .build_command()
        .spawn()
        .unwrap();
    wait_for_marker(&p, "started", &mut child);
    #[cfg(unix)]
    let candidates: BTreeSet<_> = state(&p)["units"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    assert_eq!(state(&p)["sessions"], before);
    for dir in old {
        assert!(p.root().join("target").join(dir).exists());
    }
    #[cfg(unix)]
    {
        cargo(&p, "check --features first", "agent").run();
        for dir in &candidates {
            assert!(p.root().join("target").join(dir).exists());
        }
    }
    p.change_file("finished", "done");
    let start = std::time::Instant::now();
    while !p.root().join("script-finished").exists() {
        assert!(start.elapsed().as_secs() < 10);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    cargo(&p, "check --features second", "agent").run();
    fresh(&p, "check --features second", "agent");
}

#[cargo_test]
#[cfg(unix)]
fn orphaned_test_keeps_lease_after_cargo_is_killed() {
    let p = fixture();
    p.change_file(
        "tests/sleeper.rs",
        r#"
#[test] fn sleeper() {
    std::fs::write("started", "ready").unwrap();
    let start = std::time::Instant::now();
    while !std::path::Path::new("finished").exists() {
        assert!(start.elapsed().as_secs() < 30);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    std::fs::write("test-finished", "done").unwrap();
}
"#,
    );
    let mut child = cargo(&p, "test --test sleeper --features first", "agent")
        .build_command()
        .spawn()
        .unwrap();
    wait_for_marker(&p, "started", &mut child);
    let old = units(&p, "agent");
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    cargo(&p, "check --features second", "agent").run();
    let new = units(&p, "agent");
    for dir in old.difference(&new) {
        assert!(p.root().join("target").join(dir).exists());
    }
    p.change_file("finished", "done");
    let start = std::time::Instant::now();
    loop {
        cargo(&p, "check --features second", "agent").run();
        if old
            .difference(&new)
            .all(|dir| !p.root().join("target").join(dir).exists())
        {
            break;
        }
        assert!(start.elapsed().as_secs() < 10);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(p.root().join("test-finished").exists());
}

#[cargo_test]
#[cfg(unix)]
fn run_keeps_lease_through_exec_and_configuration_change() {
    let p = fixture();
    p.change_file(
        "src/main.rs",
        r#"
fn main() {
    assert_eq!(foo::answer(), 42);
    #[cfg(feature="first")]
    {
        std::fs::write("started", "ready").unwrap();
        let start = std::time::Instant::now();
        while !std::path::Path::new("finished").exists() {
            assert!(start.elapsed().as_secs() < 30);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
"#,
    );
    let mut child = cargo(&p, "run --features first", "agent")
        .build_command()
        .spawn()
        .unwrap();
    wait_for_marker(&p, "started", &mut child);
    let old = units(&p, "agent");
    cargo(&p, "build --features second", "agent").run();
    let new = units(&p, "agent");
    for dir in old.difference(&new) {
        assert!(p.root().join("target").join(dir).exists());
    }
    p.change_file("finished", "done");
    assert!(child.wait().unwrap().success());
    fresh(&p, "build --features second", "agent");
    for dir in old.difference(&new) {
        assert!(!p.root().join("target").join(dir).exists());
    }
}
