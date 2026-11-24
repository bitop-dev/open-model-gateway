//! What the egress rule reads (#606). Clippy applies `apps/gateway/clippy.toml`
//! to this package only, including its build script, and the source scan
//! reads `src/` and `build.rs`. Code linked in from another crate is outside
//! both, so these tests hold that code to a reviewed list:
//!
//! - every crate built from a local path that the gateway's dependency graph
//!   reaches (`Cargo.lock` packages without a `source`) is listed with the
//!   reason it may be there; a new path crate linked into the gateway fails
//!   until it is reviewed;
//! - the in-repository path crates' own dependencies are pinned, so one of
//!   them cannot gain a network client unnoticed;
//! - `build.rs`, which runs on the build machine, is held to its reviewed
//!   statements and the package has no build dependencies.
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// One `Cargo.lock` package: its name, version, whether it has a `source`
/// (a registry or git) and its dependencies as written (`name` or
/// `name version`).
struct Package {
    name: String,
    version: String,
    local: bool,
    dependencies: Vec<String>,
}

fn lock_packages(lock: &str) -> Vec<Package> {
    let mut packages = Vec::new();
    for block in lock.split("[[package]]").skip(1) {
        let field = |key: &str| {
            block
                .lines()
                .find_map(|l| l.strip_prefix(&format!("{key} = ")))
                .map(|v| v.trim_matches('"').to_owned())
        };
        let dependencies = block
            .split_once("dependencies = [")
            .map(|(_, rest)| {
                rest.split_once(']')
                    .map_or(rest, |(list, _)| list)
                    .lines()
                    .map(|l| l.trim().trim_end_matches(',').trim_matches('"').to_owned())
                    .filter(|l| !l.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        packages.push(Package {
            name: field("name").expect("a package name"),
            version: field("version").expect("a package version"),
            local: field("source").is_none(),
            dependencies,
        });
    }
    packages
}

/// The packages reachable from `start` (normal, build and dev dependencies:
/// `Cargo.lock` does not tell them apart).
fn closure<'a>(packages: &'a [Package], start: &str) -> Vec<&'a Package> {
    let find = |dependency: &str| {
        let mut parts = dependency.split(' ');
        let name = parts.next().unwrap_or_default();
        let version = parts.next();
        let matching: Vec<_> = packages
            .iter()
            .filter(|p| p.name == name && version.is_none_or(|v| p.version == v))
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "Cargo.lock: `{dependency}` is ambiguous or missing"
        );
        matching[0]
    };
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let mut queue = vec![find(start)];
    while let Some(package) = queue.pop() {
        if !seen.insert((package.name.clone(), package.version.clone())) {
            continue;
        }
        out.push(package);
        queue.extend(package.dependencies.iter().map(|d| find(d)));
    }
    out
}

/// Every local-path crate in the gateway's graph, with the reason it is
/// allowed there. Change this list only in a reviewed change: a crate here is
/// compiled into the gateway (or its tests) without `clippy.toml` or the
/// source scan.
const PATH_CRATES: &[(&str, &str)] = &[
    (
        "open-model-gateway",
        "the gateway itself: clippy.toml and the source scan cover it",
    ),
    (
        "gateway-datetime-compat",
        "crates/datetime: date and time carriers, no I/O; its dependencies are pinned below",
    ),
    (
        "dbtest",
        "crates/dbtest: the `#[dbtest::test]` proc macro, a dev-dependency only; its dependencies are pinned below",
    ),
    (
        "oauth2",
        "vendor/oauth2: the patched upstream SDK, reviewed like a dependency (docs/verification.md, supply chain); the gateway drives it with its own egress client",
    ),
    (
        "openidconnect",
        "vendor/openidconnect: the patched upstream SDK, reviewed like a dependency; the gateway drives it with its own egress client",
    ),
];

/// The dependencies of the in-repository path crates, as `Cargo.lock` lists
/// them. A network crate added to one of them fails here.
const IN_REPOSITORY_DEPENDENCIES: &[(&str, &[&str])] = &[
    (
        "gateway-datetime-compat",
        &["serde", "serde_json", "sqlx", "time", "tokio"],
    ),
    ("dbtest", &["proc-macro2", "quote", "syn 3.0.10"]),
];

#[test]
fn the_gateways_path_dependencies_are_the_reviewed_list() {
    let lock = std::fs::read_to_string(root().join("../../Cargo.lock")).unwrap();
    let packages = lock_packages(&lock);
    assert!(
        packages.len() > 300,
        "read {} packages; did the parser break?",
        packages.len()
    );
    let local: BTreeSet<&str> = closure(&packages, "open-model-gateway")
        .into_iter()
        .filter(|p| p.local)
        .map(|p| p.name.as_str())
        .collect();
    let reviewed: BTreeSet<&str> = PATH_CRATES.iter().map(|(name, _)| *name).collect();
    assert_eq!(
        local, reviewed,
        "a local-path crate is linked into the gateway without review (#606): it is outside clippy.toml and the source scan"
    );
    let by_name: BTreeMap<&str, &Package> = packages
        .iter()
        .filter(|p| p.local)
        .map(|p| (p.name.as_str(), p))
        .collect();
    for (name, dependencies) in IN_REPOSITORY_DEPENDENCIES {
        assert_eq!(
            by_name[name].dependencies,
            dependencies
                .iter()
                .map(|d| (*d).to_owned())
                .collect::<Vec<_>>(),
            "{name}: its dependencies changed; review them for network clients (#606)"
        );
    }
}

/// The parser and the closure see a path crate added anywhere in the graph,
/// directly or through another crate.
#[test]
fn a_new_path_crate_anywhere_in_the_graph_is_seen() {
    let lock = r#"
[[package]]
name = "open-model-gateway"
version = "0.1.0"
dependencies = [
 "registry-crate",
]

[[package]]
name = "registry-crate"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
dependencies = [
 "hidden-path-crate",
]

[[package]]
name = "hidden-path-crate"
version = "0.1.0"

[[package]]
name = "unlinked-path-crate"
version = "0.1.0"
"#;
    let packages = lock_packages(lock);
    let local: Vec<&str> = closure(&packages, "open-model-gateway")
        .into_iter()
        .filter(|p| p.local)
        .map(|p| p.name.as_str())
        .collect();
    assert_eq!(local, ["open-model-gateway", "hidden-path-crate"]);
}

/// The build script's statements, as written (rustfmt layout). It runs on
/// the build machine, so it is held to this list like the egress client
/// module: anything else in it fails.
const BUILD_SCRIPT: &str = r#"fn main() {
    // SQLx embeds migrations in both the production migrator and test binaries.
    // Adding/changing a migration must invalidate both, including cached builds.
    println!("cargo:rerun-if-changed=enterprise_migrations");
    println!("cargo:rerun-if-changed=migrations");
}
"#;

#[test]
fn the_build_script_is_the_reviewed_one_and_has_no_dependencies() {
    let script = std::fs::read_to_string(root().join("build.rs")).unwrap();
    assert_eq!(
        script, BUILD_SCRIPT,
        "build.rs changed: it runs on the build machine outside the egress rule; review it and update BUILD_SCRIPT (#606)"
    );
    let manifest = std::fs::read_to_string(root().join("Cargo.toml")).unwrap();
    assert!(
        !manifest
            .lines()
            .any(|l| l.trim_start().starts_with("[build-dependencies]")
                || l.trim_start().starts_with("[target.") && l.contains("build-dependencies")),
        "the gateway has build dependencies: they run on the build machine outside the egress rule (#606)"
    );
    // Cargo runs the file named by `package.build`, or `build.rs`.
    assert!(
        !manifest
            .lines()
            .any(|l| l.trim_start().starts_with("build =")),
        "package.build names another build script (#606)"
    );
}
