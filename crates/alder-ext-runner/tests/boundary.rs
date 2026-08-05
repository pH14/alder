//! boundary-held: the check this crate's extraction is named for.
//!
//! `alder-ext-runner` exists because execution used to be welded to alderd —
//! the fundamental (run a command, get a handle) to the personal (how one
//! project's sessions run). The extraction is only worth having if the
//! boundary actually holds, in both directions, forever:
//!
//! 1. **The runner imports no workspace crate.** It never reads or writes the
//!    alder log, and it stamps nothing of alder's into the sessions it
//!    creates. If the runner needs a marker it uses its own name.
//! 2. **No workspace crate depends on the runner.** Alder never needs to know
//!    the runner exists; whatever drives the runner is glue outside both.
//!
//! Both directions are asserted here against `cargo metadata` — the resolved
//! manifest truth, not a grep of import lines — and against **every** other
//! workspace member the metadata reports, so a crate added to the workspace
//! tomorrow is covered without editing this file. A dependency added in
//! either direction fails this test by name before it ever compiles into a
//! coupling. A third check sweeps this crate's files for alder's log ref
//! namespaces, because reaching the log through a subprocess would evade the
//! manifest graph while breaking the same boundary.
//!
//! The check must never pass by not looking. The runner and every expected
//! counterpart are asserted to be *found* in the metadata before any edge is
//! examined, so a renamed or missing package fails loudly instead of turning
//! the assertions vacuous. When this crate moves to its own repository, that
//! failure is the prompt to delete the counterpart list along with the
//! workspace — a conscious edit, not a silent pass.

use std::{collections::BTreeMap, path::Path, process::Command};

use serde_json::Value;

const RUNNER: &str = "alder-ext-runner";

/// Every workspace package expected on the alder side of the boundary right
/// now. The edge checks below run against *all* workspace members regardless;
/// this list only guarantees none of the known ones can quietly vanish from
/// coverage by rename or removal.
const EXPECTED_COUNTERPARTS: [&str; 6] = [
    "alder",
    "alder-log",
    "alder-model",
    "alder-observation",
    "alder-work",
    "alderd",
];

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

fn packages(metadata: &Value) -> &Vec<Value> {
    metadata["packages"]
        .as_array()
        .expect("packages is an array")
}

/// The name of every workspace member, resolved through the package list so a
/// member whose package entry is missing fails here by ID rather than being
/// silently skipped.
fn workspace_member_names(metadata: &Value) -> Vec<String> {
    let by_id: BTreeMap<&str, &str> = packages(metadata)
        .iter()
        .map(|package| {
            (
                package["id"].as_str().expect("a package has an id"),
                package["name"].as_str().expect("a package has a name"),
            )
        })
        .collect();
    metadata["workspace_members"]
        .as_array()
        .expect("workspace_members is an array")
        .iter()
        .map(|id| id.as_str().expect("a workspace member id is a string"))
        .map(|id| {
            by_id
                .get(id)
                .unwrap_or_else(|| panic!("workspace member `{id}` has no package entry"))
                .to_string()
        })
        .collect()
}

/// Every dependency declaration of one package, whatever its kind — normal,
/// dev, or build. Dev-dependencies count on purpose: a runner test that
/// imported an alder crate would tie the crates' releases together exactly
/// the way the extraction forbids. The package must actually be found: an
/// absent package would otherwise report no dependencies and pass every
/// edge check without vouching for anything.
fn dependencies_of<'a>(metadata: &'a Value, package: &str) -> Vec<&'a str> {
    let entries: Vec<&Value> = packages(metadata)
        .iter()
        .filter(|entry| entry["name"] == package)
        .collect();
    assert!(
        !entries.is_empty(),
        "package `{package}` was not found in cargo metadata; the boundary \
         check cannot vouch for a crate it cannot see"
    );
    entries
        .iter()
        .flat_map(|entry| entry["dependencies"].as_array().into_iter().flatten())
        .filter_map(|dependency| dependency["name"].as_str())
        .collect()
}

#[test]
fn boundary_held_zero_dependency_edges_with_every_workspace_member() {
    let metadata = metadata();
    let members = workspace_member_names(&metadata);

    // Found before checked: the runner and every expected counterpart must be
    // present by name, so this test can never pass by failing to look.
    for expected in std::iter::once(&RUNNER).chain(EXPECTED_COUNTERPARTS.iter()) {
        assert!(
            members.iter().any(|member| member == expected),
            "`{expected}` is not a workspace member per cargo metadata. If it \
             was renamed or moved, update this test so the boundary check \
             keeps covering it; it must never go silently unchecked. \
             Members found: {members:?}"
        );
    }

    let runner_dependencies = dependencies_of(&metadata, RUNNER);
    for member in members.iter().filter(|member| *member != RUNNER) {
        // Direction one: the runner imports no workspace crate.
        assert!(
            !runner_dependencies.contains(&member.as_str()),
            "BOUNDARY BROKEN: {RUNNER} depends on `{member}`. The runner is \
             generically useful exactly because it knows nothing about alder; \
             whatever needed this edge belongs in glue outside both, not here."
        );
        // Direction two: no workspace crate depends on the runner.
        let dependencies = dependencies_of(&metadata, member);
        assert!(
            !dependencies.contains(&RUNNER),
            "BOUNDARY BROKEN: `{member}` depends on {RUNNER}. Alder never \
             needs to know the runner exists; whatever needed this edge \
             belongs in glue outside both, not in an alder crate."
        );
    }
}

#[test]
fn boundary_held_no_file_in_this_crate_names_an_alder_log_namespace() {
    // The manifest graph cannot see a subprocess. A runner that shelled out
    // to git against alder's log refs would read the log while importing
    // nothing, so every file in the crate — sources, tests, scripts, build
    // and manifest files — is swept for both log ref namespaces. The needles
    // are spelled split so this file does not sweep itself up.
    let needles = [
        concat!("refs/heads/", "alder"),
        concat!("refs/", "alder-log"),
    ];
    let mut swept = 0;
    sweep(Path::new(env!("CARGO_MANIFEST_DIR")), &needles, &mut swept);
    assert!(
        swept >= 15,
        "only {swept} files were swept; the sweep no longer covers the crate"
    );
}

fn sweep(directory: &Path, needles: &[&str], swept: &mut usize) {
    for entry in std::fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("{} is readable: {error}", directory.display()))
    {
        let path = entry.expect("a directory entry").path();
        if path.is_dir() {
            // Build output is not source; everything else recurses.
            if path.file_name().and_then(|name| name.to_str()) == Some("target") {
                continue;
            }
            sweep(&path, needles, swept);
            continue;
        }
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|error| panic!("{} reads: {error}", path.display()));
        let content = String::from_utf8_lossy(&bytes);
        for needle in needles {
            assert!(
                !content.contains(needle),
                "BOUNDARY BROKEN: {} names alder's log namespace `{needle}`. \
                 The runner never reads or writes the alder log, through any \
                 tool.",
                path.display()
            );
        }
        *swept += 1;
    }
}
