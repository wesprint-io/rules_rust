use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::process::Command;

use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use label::Label;
use serde::Deserialize;

use crate::rust_project::TargetKind;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AqueryOutput {
    artifacts: Vec<Artifact>,
    actions: Vec<Action>,
    path_fragments: Vec<PathFragment>,
    targets: Vec<Target>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Artifact {
    id: u32,
    path_fragment_id: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PathFragment {
    id: u32,
    label: String,
    parent_id: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Target {
    id: u32,
    label: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Action {
    output_ids: Vec<u32>,
    target_id: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrateSpec {
    pub aliases: BTreeMap<String, String>,
    pub crate_id: String,
    pub display_name: String,
    pub edition: String,
    pub root_module: String,
    pub is_workspace_member: bool,
    pub deps: BTreeSet<String>,
    pub proc_macro_dylib_path: Option<String>,
    pub source: Option<CrateSpecSource>,
    pub cfg: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub target: String,
    pub crate_type: CrateType,
    pub build_file: Option<Utf8PathBuf>,
    #[serde(default)]
    pub bazel_target: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CrateType {
    Bin,
    Rlib,
    Lib,
    Dylib,
    Cdylib,
    Staticlib,
    ProcMacro,
}

impl From<CrateType> for TargetKind {
    fn from(value: CrateType) -> Self {
        match value {
            CrateType::Bin => TargetKind::Bin,
            CrateType::Rlib
            | CrateType::Lib
            | CrateType::Dylib
            | CrateType::Cdylib
            | CrateType::Staticlib
            | CrateType::ProcMacro => TargetKind::Lib,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrateSpecSource {
    pub exclude_dirs: Vec<Utf8PathBuf>,
    pub include_dirs: Vec<Utf8PathBuf>,
}

pub fn get_crate_specs(
    bazel: &Utf8Path,
    workspace: &Utf8Path,
    execution_root: &Utf8Path,
    targets: &[String],
    rules_rust_name: &str,
) -> anyhow::Result<BTreeSet<CrateSpec>> {
    log::debug!("Get crate specs with targets: {:?}", targets);
    let target_pattern = format!("deps({})", targets.join("+"));

    let aquery_output = Command::new(bazel)
        .current_dir(workspace)
        .env_remove("BAZELISK_SKIP_WRAPPER")
        .env_remove("BUILD_WORKING_DIRECTORY")
        .env_remove("BUILD_WORKSPACE_DIRECTORY")
        .arg("aquery")
        .arg("--include_aspects")
        .arg("--include_artifacts")
        .arg(format!(
            "--aspects={rules_rust_name}//rust:defs.bzl%rust_analyzer_aspect"
        ))
        .arg("--output_groups=rust_analyzer_crate_spec")
        .arg(format!(
            r#"outputs(".*\.rust_analyzer_crate_spec\.json",{target_pattern})"#
        ))
        .arg("--output=jsonproto")
        .output()?;

    let crate_spec_files =
        parse_aquery_output_files(execution_root, &String::from_utf8(aquery_output.stdout)?)?;

    let mut crates = Vec::new();
    for (label, files) in crate_spec_files {
        for file in files {
            let f = File::open(&file).with_context(|| format!("Failed to open file: {file}"))?;
            let mut spec: CrateSpec = serde_json::from_reader(f)
                .with_context(|| format!("Failed to deserialize file: {file}"))?;

            spec.build_file = label_to_build_file(&label, workspace);
            crates.push(spec);
        }
    }

    consolidate_crate_specs(crates)
}

fn parse_aquery_output_files(
    execution_root: &Utf8Path,
    aquery_stdout: &str,
) -> anyhow::Result<BTreeMap<String, Vec<Utf8PathBuf>>> {
    let out: AqueryOutput = serde_json::from_str(aquery_stdout).map_err(|_| {
        // Parsing to `AqueryOutput` failed, try parsing into a `serde_json::Value`:
        match serde_json::from_str::<serde_json::Value>(aquery_stdout) {
            Ok(serde_json::Value::Object(_)) => {
                // If the JSON is an object, it's likely that the aquery command failed.
                anyhow::anyhow!("Aquery returned an empty result, are there any Rust targets in the specified paths?.")
            }
            _ => {
                anyhow::anyhow!("Failed to parse aquery output as JSON")
            }
        }
    })?;

    let artifacts = out
        .artifacts
        .iter()
        .map(|a| (a.id, a))
        .collect::<BTreeMap<_, _>>();
    let path_fragments = out
        .path_fragments
        .iter()
        .map(|pf| (pf.id, pf))
        .collect::<BTreeMap<_, _>>();
    let targets = out
        .targets
        .iter()
        .map(|t| (t.id, t.label.clone()))
        .collect::<BTreeMap<_, _>>();

    let mut output_files = BTreeMap::new();
    for action in out.actions {
        let target = targets
            .get(&action.target_id)
            .cloned()
            .expect("internal consistency error in bazel output: missing target");

        let entry: &mut Vec<_> = output_files.entry(target).or_default();

        for output_id in action.output_ids {
            let artifact = artifacts
                .get(&output_id)
                .expect("internal consistency error in bazel output: missing artifact");

            let path = path_from_fragments(artifact.path_fragment_id, &path_fragments)?;
            let path = execution_root.join(path);
            if path.exists() {
                entry.push(path);
            } else {
                log::warn!("Skipping missing crate_spec file: {:?}", path);
            }
        }
    }

    Ok(output_files)
}

fn path_from_fragments(
    id: u32,
    fragments: &BTreeMap<u32, &PathFragment>,
) -> anyhow::Result<Utf8PathBuf> {
    let path_fragment = fragments
        .get(&id)
        .expect("internal consistency error in bazel output");

    let buf = match path_fragment.parent_id {
        Some(parent_id) => path_from_fragments(parent_id, fragments)?
            .join(Utf8PathBuf::from(&path_fragment.label.clone())),
        None => Utf8PathBuf::from(&path_fragment.label.clone()),
    };

    Ok(buf)
}

/// Read all crate specs, deduplicating crates with the same ID. This happens when
/// a rust_test depends on a rust_library, for example.
fn consolidate_crate_specs(crate_specs: Vec<CrateSpec>) -> anyhow::Result<BTreeSet<CrateSpec>> {
    let mut consolidated_specs: BTreeMap<String, CrateSpec> = BTreeMap::new();
    for mut spec in crate_specs {
        log::debug!("{:?}", spec);
        if let Some(existing) = consolidated_specs.get_mut(&spec.crate_id) {
            existing.deps.extend(spec.deps);

            spec.cfg.retain(|cfg| !existing.cfg.contains(cfg));
            existing.cfg.extend(spec.cfg);

            // display_name should match the library's crate name because Rust Analyzer
            // seems to use display_name for matching crate entries in rust-project.json
            // against symbols in source files. For more details, see
            // https://github.com/bazelbuild/rules_rust/issues/1032
            if spec.crate_type == CrateType::Rlib {
                existing.display_name = spec.display_name;
                existing.crate_type = CrateType::Rlib;
            }

            // For proc-macro crates that exist within the workspace, there will be a
            // generated crate-spec in both the fastbuild and opt-exec configuration.
            // Prefer proc macro paths with an opt-exec component in the path.
            if let Some(dylib_path) = spec.proc_macro_dylib_path.as_ref() {
                const OPT_PATH_COMPONENT: &str = "-opt-exec-";
                if dylib_path.contains(OPT_PATH_COMPONENT) {
                    existing.proc_macro_dylib_path.replace(dylib_path.clone());
                }
            }
        } else {
            consolidated_specs.insert(spec.crate_id.clone(), spec);
        }
    }

    Ok(consolidated_specs.into_values().collect())
}

fn label_to_build_file(label: &str, workspace: &Utf8Path) -> Option<Utf8PathBuf> {
    let label = Label::analyze(label).ok()?;
    // External targets don't have a BUILD.bazel file in the repository.
    let package = label.repo().is_none().then(|| label.package()).flatten()?;

    let build_bazel_file: Utf8PathBuf = [workspace.as_str(), package, "BUILD.bazel"]
        .iter()
        .collect();

    if build_bazel_file.exists() {
        return Some(build_bazel_file);
    }

    let build_file: Utf8PathBuf = [workspace.as_str(), package, "BUILD"].iter().collect();

    if build_file.exists() {
        return Some(build_file);
    }

    None
}

#[cfg(test)]
mod test {
    use super::*;
    use itertools::Itertools;

    #[test]
    fn consolidate_lib_then_test_specs() {
        let crate_specs = vec![
            CrateSpec {
                aliases: BTreeMap::new(),
                crate_id: "ID-mylib.rs".into(),
                display_name: "mylib".into(),
                edition: "2018".into(),
                root_module: "mylib.rs".into(),
                is_workspace_member: true,
                deps: BTreeSet::from(["ID-lib_dep.rs".into()]),
                proc_macro_dylib_path: None,
                source: None,
                cfg: vec!["test".into(), "debug_assertions".into()],
                env: BTreeMap::new(),
                target: "x86_64-unknown-linux-gnu".into(),
                crate_type: "rlib".into(),
                build_file: None,
                bazel_target: "//tools/rust_analyzer:gen_rust_project_lib".to_owned(),
                build_file: None,
                bazel_target: "//tools/rust_analyzer:gen_rust_project_lib".to_owned(),
            },
            CrateSpec {
                aliases: BTreeMap::new(),
                crate_id: "ID-extra_test_dep.rs".into(),
                display_name: "extra_test_dep".into(),
                edition: "2018".into(),
                root_module: "extra_test_dep.rs".into(),
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
            },
            CrateSpec {
                aliases: BTreeMap::new(),
                crate_id: "ID-lib_dep.rs".into(),
                display_name: "lib_dep".into(),
                edition: "2018".into(),
                root_module: "lib_dep.rs".into(),
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
            },
            CrateSpec {
                aliases: BTreeMap::new(),
                crate_id: "ID-mylib.rs".into(),
                display_name: "mylib_test".into(),
                edition: "2018".into(),
                root_module: "mylib.rs".into(),
                is_workspace_member: true,
                deps: BTreeSet::from(["ID-extra_test_dep.rs".into()]),
                proc_macro_dylib_path: None,
                source: None,
                cfg: vec!["test".into(), "debug_assertions".into()],
                env: BTreeMap::new(),
                target: "x86_64-unknown-linux-gnu".into(),
                crate_type: "bin".into(),
                build_file: None,
                bazel_target: "//tools/rust_analyzer:gen_rust_project".to_owned(),
                build_file: None,
                bazel_target: "//tools/rust_analyzer:gen_rust_project".to_owned(),
            },
        ];

        assert_eq!(
            consolidate_crate_specs(crate_specs).unwrap(),
            BTreeSet::from([
                CrateSpec {
                    aliases: BTreeMap::new(),
                    crate_id: "ID-mylib.rs".into(),
                    display_name: "mylib".into(),
                    edition: "2018".into(),
                    root_module: "mylib.rs".into(),
                    is_workspace_member: true,
                    deps: BTreeSet::from(["ID-lib_dep.rs".into(), "ID-extra_test_dep.rs".into()]),
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
                    crate_id: "ID-extra_test_dep.rs".into(),
                    display_name: "extra_test_dep".into(),
                    edition: "2018".into(),
                    root_module: "extra_test_dep.rs".into(),
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
                },
                CrateSpec {
                    aliases: BTreeMap::new(),
                    crate_id: "ID-lib_dep.rs".into(),
                    display_name: "lib_dep".into(),
                    edition: "2018".into(),
                    root_module: "lib_dep.rs".into(),
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
                },
            ])
        );
    }

    #[test]
    fn consolidate_test_then_lib_specs() {
        let crate_specs = vec![
            CrateSpec {
                aliases: BTreeMap::new(),
                crate_id: "ID-mylib.rs".into(),
                display_name: "mylib_test".into(),
                edition: "2018".into(),
                root_module: "mylib.rs".into(),
                is_workspace_member: true,
                deps: BTreeSet::from(["ID-extra_test_dep.rs".into()]),
                proc_macro_dylib_path: None,
                source: None,
                cfg: vec!["test".into(), "debug_assertions".into()],
                env: BTreeMap::new(),
                target: "x86_64-unknown-linux-gnu".into(),
                crate_type: "bin".into(),
                build_file: None,
                bazel_target: "//tools/rust_analyzer:gen_rust_project".to_owned(),
                build_file: None,
                bazel_target: "//tools/rust_analyzer:gen_rust_project".to_owned(),
            },
            CrateSpec {
                aliases: BTreeMap::new(),
                crate_id: "ID-mylib.rs".into(),
                display_name: "mylib".into(),
                edition: "2018".into(),
                root_module: "mylib.rs".into(),
                is_workspace_member: true,
                deps: BTreeSet::from(["ID-lib_dep.rs".into()]),
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
                crate_id: "ID-extra_test_dep.rs".into(),
                display_name: "extra_test_dep".into(),
                edition: "2018".into(),
                root_module: "extra_test_dep.rs".into(),
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
            },
            CrateSpec {
                aliases: BTreeMap::new(),
                crate_id: "ID-lib_dep.rs".into(),
                display_name: "lib_dep".into(),
                edition: "2018".into(),
                root_module: "lib_dep.rs".into(),
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
            },
        ];

        assert_eq!(
            consolidate_crate_specs(crate_specs).unwrap(),
            BTreeSet::from([
                CrateSpec {
                    aliases: BTreeMap::new(),
                    crate_id: "ID-mylib.rs".into(),
                    display_name: "mylib".into(),
                    edition: "2018".into(),
                    root_module: "mylib.rs".into(),
                    is_workspace_member: true,
                    deps: BTreeSet::from(["ID-lib_dep.rs".into(), "ID-extra_test_dep.rs".into()]),
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
                    crate_id: "ID-extra_test_dep.rs".into(),
                    display_name: "extra_test_dep".into(),
                    edition: "2018".into(),
                    root_module: "extra_test_dep.rs".into(),
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
                },
                CrateSpec {
                    aliases: BTreeMap::new(),
                    crate_id: "ID-lib_dep.rs".into(),
                    display_name: "lib_dep".into(),
                    edition: "2018".into(),
                    root_module: "lib_dep.rs".into(),
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
                },
            ])
        );
    }

    #[test]
    fn consolidate_lib_test_main_specs() {
        // mylib.rs is a library but has tests and an entry point, and mylib2.rs
        // depends on mylib.rs. The display_name of the library target mylib.rs
        // should be "mylib" no matter what order the crate specs is in.
        // Otherwise Rust Analyzer will not be able to resolve references to
        // mylib in mylib2.rs.
        let crate_specs = vec![
            CrateSpec {
                aliases: BTreeMap::new(),
                crate_id: "ID-mylib.rs".into(),
                display_name: "mylib".into(),
                edition: "2018".into(),
                root_module: "mylib.rs".into(),
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
            },
            CrateSpec {
                aliases: BTreeMap::new(),
                crate_id: "ID-mylib.rs".into(),
                display_name: "mylib_test".into(),
                edition: "2018".into(),
                root_module: "mylib.rs".into(),
                is_workspace_member: true,
                deps: BTreeSet::new(),
                proc_macro_dylib_path: None,
                source: None,
                cfg: vec!["test".into(), "debug_assertions".into()],
                env: BTreeMap::new(),
                target: "x86_64-unknown-linux-gnu".into(),
                crate_type: "bin".into(),
                build_file: None,
                bazel_target: "//tools/rust_analyzer:gen_rust_project".to_owned(),
                build_file: None,
                bazel_target: "//tools/rust_analyzer:gen_rust_project".to_owned(),
            },
            CrateSpec {
                aliases: BTreeMap::new(),
                crate_id: "ID-mylib.rs".into(),
                display_name: "mylib_main".into(),
                edition: "2018".into(),
                root_module: "mylib.rs".into(),
                is_workspace_member: true,
                deps: BTreeSet::new(),
                proc_macro_dylib_path: None,
                source: None,
                cfg: vec!["test".into(), "debug_assertions".into()],
                env: BTreeMap::new(),
                target: "x86_64-unknown-linux-gnu".into(),
                crate_type: "bin".into(),
                build_file: None,
                bazel_target: "//tools/rust_analyzer:gen_rust_project".to_owned(),
            },
            CrateSpec {
                aliases: BTreeMap::new(),
                crate_id: "ID-mylib2.rs".into(),
                display_name: "mylib2".into(),
                edition: "2018".into(),
                root_module: "mylib2.rs".into(),
                is_workspace_member: true,
                deps: BTreeSet::from(["ID-mylib.rs".into()]),
                proc_macro_dylib_path: None,
                source: None,
                cfg: vec!["test".into(), "debug_assertions".into()],
                env: BTreeMap::new(),
                target: "x86_64-unknown-linux-gnu".into(),
                crate_type: "rlib".into(),
                build_file: None,
                bazel_target: "//tools/rust_analyzer:gen_rust_project_lib".to_owned(),
            },
        ];

        for perm in crate_specs.into_iter().permutations(4) {
            assert_eq!(
                consolidate_crate_specs(perm).unwrap(),
                BTreeSet::from([
                    CrateSpec {
                        aliases: BTreeMap::new(),
                        crate_id: "ID-mylib.rs".into(),
                        display_name: "mylib".into(),
                        edition: "2018".into(),
                        root_module: "mylib.rs".into(),
                        is_workspace_member: true,
                        deps: BTreeSet::from([]),
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
                        crate_id: "ID-mylib2.rs".into(),
                        display_name: "mylib2".into(),
                        edition: "2018".into(),
                        root_module: "mylib2.rs".into(),
                        is_workspace_member: true,
                        deps: BTreeSet::from(["ID-mylib.rs".into()]),
                        proc_macro_dylib_path: None,
                        source: None,
                        cfg: vec!["test".into(), "debug_assertions".into()],
                        env: BTreeMap::new(),
                        target: "x86_64-unknown-linux-gnu".into(),
                        crate_type: "rlib".into(),
                        build_file: None,
                        bazel_target: "//tools/rust_analyzer:gen_rust_project_lib".to_owned(),
                    },
                ])
            );
        }
    }

    #[test]
    fn consolidate_proc_macro_prefer_exec() {
        // proc macro crates should prefer the -opt-exec- path which is always generated
        // during builds where it is used, while the fastbuild version would only be built
        // when explicitly building that target.
        let crate_specs = vec![
            CrateSpec {
                aliases: BTreeMap::new(),
                crate_id: "ID-myproc_macro.rs".into(),
                display_name: "myproc_macro".into(),
                edition: "2018".into(),
                root_module: "myproc_macro.rs".into(),
                is_workspace_member: true,
                deps: BTreeSet::new(),
                proc_macro_dylib_path: Some(
                    "bazel-out/k8-opt-exec-F005BA11/bin/myproc_macro/libmyproc_macro-12345.so"
                        .into(),
                ),
                source: None,
                cfg: vec!["test".into(), "debug_assertions".into()],
                env: BTreeMap::new(),
                target: "x86_64-unknown-linux-gnu".into(),
                crate_type: "proc_macro".into(),
                build_file: None,
                bazel_target: "//tools/rust_analyzer:gen_rust_project_proc_macro".to_owned(),
            },
            CrateSpec {
                aliases: BTreeMap::new(),
                crate_id: "ID-myproc_macro.rs".into(),
                display_name: "myproc_macro".into(),
                edition: "2018".into(),
                root_module: "myproc_macro.rs".into(),
                is_workspace_member: true,
                deps: BTreeSet::new(),
                proc_macro_dylib_path: Some(
                    "bazel-out/k8-fastbuild/bin/myproc_macro/libmyproc_macro-12345.so".into(),
                ),
                source: None,
                cfg: vec!["test".into(), "debug_assertions".into()],
                env: BTreeMap::new(),
                target: "x86_64-unknown-linux-gnu".into(),
                crate_type: "proc_macro".into(),
                build_file: None,
                bazel_target: "//tools/rust_analyzer:gen_rust_project_proc_macro".to_owned(),
            },
        ];

        for perm in crate_specs.into_iter().permutations(2) {
            assert_eq!(
                consolidate_crate_specs(perm).unwrap(),
                BTreeSet::from([CrateSpec {
                    aliases: BTreeMap::new(),
                    crate_id: "ID-myproc_macro.rs".into(),
                    display_name: "myproc_macro".into(),
                    edition: "2018".into(),
                    root_module: "myproc_macro.rs".into(),
                    is_workspace_member: true,
                    deps: BTreeSet::new(),
                    proc_macro_dylib_path: Some(
                        "bazel-out/k8-opt-exec-F005BA11/bin/myproc_macro/libmyproc_macro-12345.so"
                            .into()
                    ),
                    source: None,
                    cfg: vec!["test".into(), "debug_assertions".into()],
                    env: BTreeMap::new(),
                    target: "x86_64-unknown-linux-gnu".into(),
                    crate_type: "proc_macro".into(),
                    build_file: None,
                    bazel_target: "//tools/rust_analyzer:gen_rust_project_proc_macro".to_owned(),
                },])
            );
        }
    }
}
