//! boundary-held: the check this crate's extraction is named for.
//!
//! `alder-ext-runner` exists because execution used to be welded to alderd —
//! the fundamental (run a command, get a handle) to the personal (how one
//! project's sessions run). The extraction is only worth having if the
//! boundary actually holds, in both directions, forever:
//!
//! 1. **The runner imports no alder crate.** It never reads or writes the
//!    alder log, and it stamps nothing of alder's into the sessions it
//!    creates. If the runner needs a marker it uses its own name.
//! 2. **No alder crate depends on the runner.** Alder never needs to know
//!    the runner exists; whatever drives the runner is glue outside both.
//!
//! Both directions are asserted here against `cargo metadata` — the resolved
//! manifest truth, not a grep of import lines — so a dependency added in
//! either direction fails this test by name before it ever compiles into a
//! coupling. A third check greps this crate's sources for alder's log ref
//! path, because reaching the log through a subprocess would evade the
//! manifest graph while breaking the same boundary.
//!
//! This is also the movability claim: a crate with zero edges either way can
//! be copied into its own repository whole. When that happens the alder
//! packages simply stop appearing in the metadata and every assertion below
//! holds vacuously, which is the correct answer.

use std::{path::Path, process::Command};

use serde_json::Value;

/// The alder side of the boundary: every crate the runner must know nothing
/// about, and that must know nothing about the runner.
const ALDER_CRATES: [&str; 5] = [
    "alder",
    "alder-log",
    "alder-work",
    "alder-observation",
    "alderd",
];

const RUNNER: &str = "alder-ext-runner";

fn metadata() -> Value {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args(["metadata", "--format-version", "1"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo metadata runs");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("cargo metadata prints JSON")
}

/// Every dependency declaration of one package, whatever its kind — normal,
/// dev, or build. Dev-dependencies count on purpose: a runner test that
/// imported an alder crate would tie the crates' releases together exactly
/// the way the extraction forbids.
fn dependencies_of<'a>(metadata: &'a Value, package: &str) -> Vec<&'a str> {
    metadata["packages"]
        .as_array()
        .expect("packages is an array")
        .iter()
        .filter(|entry| entry["name"] == package)
        .flat_map(|entry| entry["dependencies"].as_array().into_iter().flatten())
        .filter_map(|dependency| dependency["name"].as_str())
        .collect()
}

#[test]
fn boundary_held_zero_dependency_edges_in_either_direction() {
    let metadata = metadata();

    // Direction one: the runner imports no alder crate.
    let runner_dependencies = dependencies_of(&metadata, RUNNER);
    assert!(
        !runner_dependencies.is_empty() || metadata["packages"].as_array().is_some(),
        "cargo metadata reported no packages at all"
    );
    for forbidden in ALDER_CRATES {
        assert!(
            !runner_dependencies.contains(&forbidden),
            "BOUNDARY BROKEN: {RUNNER} depends on `{forbidden}`. The runner is \
             generically useful exactly because it knows nothing about alder; \
             whatever needed this edge belongs in glue outside both, not here."
        );
    }

    // Direction two: no alder crate depends on the runner.
    for alder_crate in ALDER_CRATES {
        let dependencies = dependencies_of(&metadata, alder_crate);
        assert!(
            !dependencies.contains(&RUNNER),
            "BOUNDARY BROKEN: `{alder_crate}` depends on {RUNNER}. Alder never \
             needs to know the runner exists; whatever needed this edge belongs \
             in glue outside both, not in an alder crate."
        );
    }
}

#[test]
fn boundary_held_the_runner_never_names_the_alder_log_ref() {
    // The manifest graph cannot see a subprocess. A runner that shelled out
    // to git against `refs/heads/alder` would read the log while importing
    // nothing, so the sources are swept for the ref path itself.
    let sources = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut swept = 0;
    for entry in std::fs::read_dir(&sources).expect("src is readable") {
        let path = entry.expect("a directory entry").path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("a source file reads");
        assert!(
            !source.contains("refs/heads/alder"),
            "BOUNDARY BROKEN: {} names alder's log ref. The runner never reads \
             or writes the alder log, through any tool.",
            path.display()
        );
        swept += 1;
    }
    assert!(
        swept >= 5,
        "only {swept} source files were swept; the sweep no longer covers src/"
    );
}
