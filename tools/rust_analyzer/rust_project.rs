//! Library for generating rust_project.json files from a `Vec<CrateSpec>`
//! See official documentation of file format at https://rust-analyzer.github.io/manual.html

use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;

use anyhow::anyhow;
use camino::{Utf8Path, Utf8PathBuf};
use serde::Serialize;

use crate::aquery::CrateSpec;

/// The format that rust_analyzer expects as a response when automatically invoked.
#[derive(Debug, Serialize)]
#[serde(tag = "kind")]
#[serde(rename_all = "snake_case")]
pub enum DiscoverProject {
    Finished {
        buildfile: Utf8PathBuf,
        project: RustProject,
    },
    Error {
        error: String,
        source: Option<String>,
    },
    Progress {
        message: String,
    },
}

/// A `rust-project.json` workspace representation. See
/// [rust-analyzer documentation][rd] for a thorough description of this interface.
/// [rd]: https://rust-analyzer.github.io/manual.html#non-cargo-based-projects
#[derive(Debug, Serialize)]
pub struct RustProject {
    /// Path to the sysroot directory.
    ///
    /// The sysroot is where rustc looks for the
    /// crates that are built-in to rust, such as
    /// std.
    ///
    /// https://doc.rust-lang.org/rustc/command-line-arguments.html#--sysroot-override-the-system-root
    ///
    /// To see the current value of sysroot, you
    /// can query rustc:
    ///
    /// ```
    /// $ rustc --print sysroot
    /// /Users/yourname/.rustup/toolchains/stable-x86_64-apple-darwin
    /// ```
    sysroot: Option<Utf8PathBuf>,
    /// Path to the directory with *source code* of
    /// sysroot crates.
    ///
    /// By default, this is `lib/rustlib/src/rust/library`
    /// relative to the sysroot.
    ///
    /// It should point to the directory where std,
    /// core, and friends can be found:
    ///
    /// https://github.com/rust-lang/rust/tree/master/library.
    ///
    /// If provided, rust-analyzer automatically adds
    /// dependencies on sysroot crates. Conversely,
    /// if you omit this path, you can specify sysroot
    /// dependencies yourself and, for example, have
    /// several different "sysroots" in one graph of
    /// crates.
    sysroot_src: Option<Utf8PathBuf>,
    /// The set of crates comprising the current
    /// project. Must include all transitive
    /// dependencies as well as sysroot crate (libstd,
    /// libcore and such).
    crates: Vec<Crate>,

    pub(crate) runnables: Vec<Runnable>,
}

/// A `rust-project.json` crate representation. See
/// [rust-analyzer documentation][rd] for a thorough description of this interface.
/// [rd]: https://rust-analyzer.github.io/manual.html#non-cargo-based-projects
#[derive(Debug, Serialize)]
pub struct Crate {
    /// A name used in the package's project declaration
    #[serde(skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,

    /// Path to the root module of the crate.
    root_module: Utf8PathBuf,

    /// Edition of the crate.
    edition: String,

    /// Dependencies
    deps: Vec<Dependency>,

    /// The set of cfgs activated for a given crate, like
    /// `["unix", "feature=\"foo\"", "feature=\"bar\""]`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cfg: Vec<String>,

    /// Target triple for this Crate.
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,

    /// Environment variables, used for the `env!` macro
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,

    /// For proc-macro crates, path to compiled proc-macro (.so file).
    #[serde(skip_serializing_if = "Option::is_none")]
    proc_macro_dylib_path: Option<String>,

    /// Should this crate be treated as a member of current "workspace".
    #[serde(skip_serializing_if = "Option::is_none")]
    is_workspace_member: Option<bool>,

    /// Optionally specify the (super)set of `.rs` files comprising this crate.
    #[serde(skip_serializing_if = "Source::is_empty")]
    source: Source,

    /// Whether the crate is a proc-macro crate.
    is_proc_macro: bool,

    #[serde(skip_serializing_if = "Option::is_none")]
    build: Option<Build>,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TargetKind {
    Bin,
    /// Any kind of Cargo lib crate-type (dylib, rlib, proc-macro, ...).
    Lib,
    // Test,
}

/// A template-like structure for describing runnables.
///
/// These are used for running and debugging binaries and tests without encoding
/// build system-specific knowledge into rust-analyzer.
///
/// # Example
///
/// Below is an example of a test runnable. `{label}` and `{test_id}`
/// are explained in [`Runnable::args`]'s documentation.
///
/// ```json
/// {
///     "program": "buck",
///     "args": [
///         "test",
///          "{label}",
///          "--",
///          "{test_id}",
///          "--print-passing-details"
///     ],
///     "cwd": "/home/user/repo-root/",
///     "kind": "testOne"
/// }
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct Runnable {
    /// The program invoked by the runnable.
    ///
    /// For example, this might be `cargo`, `buck`, or `bazel`.
    pub program: String,
    /// The arguments passed to [`Runnable::program`].
    ///
    /// The args can contain two template strings: `{label}` and `{test_id}`.
    /// rust-analyzer will find and replace `{label}` with [`Build::label`] and
    /// `{test_id}` with the test name.
    pub args: Vec<String>,
    /// The current working directory of the runnable.
    pub cwd: Utf8PathBuf,
    pub kind: RunnableKind,
}

/// The kind of runnable.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RunnableKind {
    Check,

    // /// Can run a binary.
    // Run,
    /// Run a single test.
    TestOne,
}

#[derive(Debug, Serialize)]
pub struct Build {
    /// The name associated with this crate.
    ///
    /// This is determined by the build system that produced
    /// the `rust-project.json` in question. For instance, if buck were used,
    /// the label might be something like `//ide/rust/rust-analyzer:rust-analyzer`.
    ///
    /// Do not attempt to parse the contents of this string; it is a build system-specific
    /// identifier similar to [`Crate::display_name`].
    pub label: String,
    /// Path corresponding to the build system-specific file defining the crate.
    ///
    /// It is roughly analogous to [`ManifestPath`], but it should *not* be used with
    /// [`crate::ProjectManifest::from_manifest_file`], as the build file may not be
    /// be in the `rust-project.json`.
    pub build_file: Utf8PathBuf,
    /// The kind of target.
    ///
    /// Examples (non-exhaustively) include [`TargetKind::Bin`], [`TargetKind::Lib`],
    /// and [`TargetKind::Test`]. This information is used to determine what sort
    /// of runnable codelens to provide, if any.
    pub target_kind: TargetKind,
}

#[derive(Debug, Default, Serialize)]
pub struct Source {
    include_dirs: Vec<Utf8PathBuf>,
    exclude_dirs: Vec<Utf8PathBuf>,
}

impl Source {
    /// Returns true if no include information has been added.
    fn is_empty(&self) -> bool {
        self.include_dirs.is_empty() && self.exclude_dirs.is_empty()
    }
}

#[derive(Debug, Serialize)]
pub struct Dependency {
    /// Index of a crate in the `crates` array.
    #[serde(rename = "crate")]
    crate_index: usize,

    /// The display name of the crate.
    name: String,
}

pub fn generate_rust_project(
    workspace: &Utf8Path,
    sysroot: &str,
    sysroot_src: &str,
    crates: &BTreeSet<CrateSpec>,
) -> anyhow::Result<RustProject> {
    let mut project = RustProject {
        sysroot: Some(sysroot.into()),
        sysroot_src: Some(sysroot_src.into()),
        crates: Vec::new(),
        runnables: vec![
            Runnable {
                program: "bazel".to_owned(),
                args: vec!["build".to_owned(), "{label}".to_owned()],
                cwd: workspace.to_owned(),
                kind: RunnableKind::Check,
            },
            Runnable {
                program: "bazel".to_owned(),
                args: vec![
                    "test".to_owned(),
                    "{label}".to_owned(),
                    "--".to_owned(),
                    "{test_id}".to_owned(),
                ],
                cwd: workspace.to_owned(),
                kind: RunnableKind::TestOne,
            },
        ],
    };

    let mut unmerged_crates: Vec<&CrateSpec> = crates.iter().collect();
    let mut skipped_crates: Vec<&CrateSpec> = Vec::new();
    let mut merged_crates_index: BTreeMap<String, usize> = BTreeMap::new();

    while !unmerged_crates.is_empty() {
        for c in unmerged_crates.iter() {
            if c.deps
                .iter()
                .any(|dep| !merged_crates_index.contains_key(dep))
            {
                log::trace!(
                    "Skipped crate {} because missing deps: {:?}",
                    &c.crate_id,
                    c.deps
                        .iter()
                        .filter(|dep| !merged_crates_index.contains_key(*dep))
                        .cloned()
                        .collect::<Vec<_>>()
                );
                skipped_crates.push(c);
            } else {
                log::trace!("Merging crate {}", &c.crate_id);
                merged_crates_index.insert(c.crate_id.clone(), project.crates.len());
                project.crates.push(Crate {
                    display_name: Some(c.display_name.clone()),
                    root_module: c.root_module.clone().into(),
                    edition: c.edition.clone(),
                    deps: c
                        .deps
                        .iter()
                        .map(|dep| {
                            let crate_index = *merged_crates_index
                                .get(dep)
                                .expect("failed to find dependency on second lookup");
                            let dep_crate = &project.crates[crate_index];
                            let name = if let Some(alias) = c.aliases.get(dep) {
                                alias.clone()
                            } else {
                                dep_crate
                                    .display_name
                                    .as_ref()
                                    .expect("all crates should have display_name")
                                    .clone()
                            };
                            Dependency { crate_index, name }
                        })
                        .collect(),
                    is_workspace_member: Some(c.is_workspace_member),
                    source: match &c.source {
                        Some(s) => Source {
                            exclude_dirs: s.exclude_dirs.clone(),
                            include_dirs: s.include_dirs.clone(),
                        },
                        None => Source::default(),
                    },
                    cfg: c.cfg.clone(),
                    target: Some(c.target.clone()),
                    env: c.env.clone(),
                    is_proc_macro: c.proc_macro_dylib_path.is_some(),
                    proc_macro_dylib_path: c.proc_macro_dylib_path.clone(),
                    build: c.build_file.as_ref().map(|build_file| Build {
                        label: c.bazel_target.clone(),
                        build_file: build_file.to_owned(),
                        target_kind: c.crate_type.into(),
                    }),
                });
            }
        }

        // This should not happen, but if it does exit to prevent infinite loop.
        if unmerged_crates.len() == skipped_crates.len() {
            log::debug!(
                "Did not make progress on {} unmerged crates. Crates: {:?}",
                skipped_crates.len(),
                skipped_crates
            );
            let crate_map: BTreeMap<String, &CrateSpec> = unmerged_crates
                .iter()
                .map(|c| (c.crate_id.to_string(), *c))
                .collect();

            for unmerged_crate in &unmerged_crates {
                let mut path = vec![];
                if let Some(cycle) = detect_cycle(unmerged_crate, &crate_map, &mut path) {
                    log::warn!(
                        "Cycle detected: {:?}",
                        cycle
                            .iter()
                            .map(|c| c.crate_id.to_string())
                            .collect::<Vec<String>>()
                    );
                }
            }
            return Err(anyhow!(
                "Failed to make progress on building crate dependency graph"
            ));
        }
        std::mem::swap(&mut unmerged_crates, &mut skipped_crates);
        skipped_crates.clear();
    }

    Ok(project)
}

fn detect_cycle<'a>(
    current_crate: &'a CrateSpec,
    all_crates: &'a BTreeMap<String, &'a CrateSpec>,
    path: &mut Vec<&'a CrateSpec>,
) -> Option<Vec<&'a CrateSpec>> {
    if path
        .iter()
        .any(|dependent_crate| dependent_crate.crate_id == current_crate.crate_id)
    {
        let mut cycle_path = path.clone();
        cycle_path.push(current_crate);
        return Some(cycle_path);
    }

    path.push(current_crate);

    for dep in &current_crate.deps {
        match all_crates.get(dep) {
            Some(dep_crate) => {
                if let Some(cycle) = detect_cycle(dep_crate, all_crates, path) {
                    return Some(cycle);
                }
            }
            None => log::debug!("dep {dep} not found in unmerged crate map"),
        }
    }

    path.pop();

    None
}

pub fn write_rust_project(
    rust_project_path: &Utf8Path,
    execution_root: &Utf8Path,
    output_base: &Utf8Path,
    rust_project: &RustProject,
) -> anyhow::Result<()> {
    // Try to remove the existing rust-project.json. It's OK if the file doesn't exist.
    match std::fs::remove_file(rust_project_path) {
        Ok(_) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => {
            return Err(anyhow!(
                "Unexpected error removing old rust-project.json: {}",
                err
            ))
        }
    }

    // Render the `rust-project.json` file and replace the exec root
    // placeholders with the path to the local exec root.
    let rust_project_content = serde_json::to_string_pretty(rust_project)?
        .replace("${pwd}", execution_root.as_str())
        .replace("__EXEC_ROOT__", execution_root.as_str())
        .replace("__OUTPUT_BASE__", output_base.as_str());

    // Write the new rust-project.json file.
    std::fs::write(rust_project_path, rust_project_content)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A simple example with a single crate and no dependencies.
    #[test]
    fn generate_rust_project_single() {
        let project = generate_rust_project(
            "",
            "sysroot",
            "sysroot_src",
            &BTreeSet::from([CrateSpec {
                aliases: BTreeMap::new(),
                crate_id: "ID-example".into(),
                display_name: "example".into(),
                edition: "2018".into(),
                root_module: "example/lib.rs".into(),
                is_workspace_member: true,
                deps: BTreeSet::new(),
                proc_macro_dylib_path: None,
                source: None,
                cfg: vec!["test".into(), "debug_assertions".into()],
                env: BTreeMap::new(),
                target: "x86_64-unknown-linux-gnu".into(),
                crate_type: "rlib".into(),
                build_file: None,
                bazel_target: "//tools/rust_analyzer:gen_rust_project_lib".to_owned(),
            }]),
        )
        .expect("expect success");

        assert_eq!(project.crates.len(), 1);
        let c = &project.crates[0];
        assert_eq!(c.display_name, Some("example".into()));
        assert_eq!(c.root_module, "example/lib.rs");
        assert_eq!(c.deps.len(), 0);
    }

    /// An example with a one crate having two dependencies.
    #[test]
    fn generate_rust_project_with_deps() {
        let project = generate_rust_project(
            "",
            "sysroot",
            "sysroot_src",
            &BTreeSet::from([
                CrateSpec {
                    aliases: BTreeMap::new(),
                    crate_id: "ID-example".into(),
                    display_name: "example".into(),
                    edition: "2018".into(),
                    root_module: "example/lib.rs".into(),
                    is_workspace_member: true,
                    deps: BTreeSet::from(["ID-dep_a".into(), "ID-dep_b".into()]),
                    proc_macro_dylib_path: None,
                    source: None,
                    cfg: vec!["test".into(), "debug_assertions".into()],
                    env: BTreeMap::new(),
                    target: "x86_64-unknown-linux-gnu".into(),
                    crate_type: "rlib".into(),
                    build_file: None,
                    bazel_target: "//tools/rust_analyzer:gen_rust_project_lib".to_owned(),
                },
                CrateSpec {
                    aliases: BTreeMap::new(),
                    crate_id: "ID-dep_a".into(),
                    display_name: "dep_a".into(),
                    edition: "2018".into(),
                    root_module: "dep_a/lib.rs".into(),
                    is_workspace_member: false,
                    deps: BTreeSet::new(),
                    proc_macro_dylib_path: None,
                    source: None,
                    cfg: vec!["test".into(), "debug_assertions".into()],
                    env: BTreeMap::new(),
                    target: "x86_64-unknown-linux-gnu".into(),
                    crate_type: "rlib".into(),
                    build_file: None,
                    bazel_target: "//tools/rust_analyzer:gen_rust_project_lib".to_owned(),
                },
                CrateSpec {
                    aliases: BTreeMap::new(),
                    crate_id: "ID-dep_b".into(),
                    display_name: "dep_b".into(),
                    edition: "2018".into(),
                    root_module: "dep_b/lib.rs".into(),
                    is_workspace_member: false,
                    deps: BTreeSet::new(),
                    proc_macro_dylib_path: None,
                    source: None,
                    cfg: vec!["test".into(), "debug_assertions".into()],
                    env: BTreeMap::new(),
                    target: "x86_64-unknown-linux-gnu".into(),
                    crate_type: "rlib".into(),
                    build_file: None,
                    bazel_target: "//tools/rust_analyzer:gen_rust_project_lib".to_owned(),
                },
            ]),
        )
        .expect("expect success");

        assert_eq!(project.crates.len(), 3);
        // Both dep_a and dep_b should be one of the first two crates.
        assert!(
            Some("dep_a".into()) == project.crates[0].display_name
                || Some("dep_a".into()) == project.crates[1].display_name
        );
        assert!(
            Some("dep_b".into()) == project.crates[0].display_name
                || Some("dep_b".into()) == project.crates[1].display_name
        );
        let c = &project.crates[2];
        assert_eq!(c.display_name, Some("example".into()));
    }
}
