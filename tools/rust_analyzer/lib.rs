use core::str;
use std::io::BufRead;
use std::process::Command;
use std::{collections::HashMap, str::FromStr};

use anyhow::{anyhow, Context};
use camino::{Utf8Path, Utf8PathBuf};
use runfiles::Runfiles;

mod aquery;
mod rust_project;

use rust_project::{DiscoverProject, RustProject};
use serde::Deserialize;

#[derive(PartialEq, Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RustAnalyzerArg {
    Path(Utf8PathBuf),
    Buildfile(Utf8PathBuf),
    Label(String),
}

impl RustAnalyzerArg {
    pub fn into_targets(
        self,
        bazel: &Utf8Path,
        workspace: &Utf8Path,
    ) -> anyhow::Result<Vec<String>> {
        match self {
            RustAnalyzerArg::Path(path) => Self::query_file_targets(bazel, workspace, path),
            RustAnalyzerArg::Buildfile(buildfile) => {
                Self::query_buildfile_targets(bazel, workspace, &buildfile)
            }
            RustAnalyzerArg::Label(s) => Ok(vec![s]),
        }
    }

    fn query_file_targets(
        bazel: &Utf8Path,
        workspace: &Utf8Path,
        path: Utf8PathBuf,
    ) -> anyhow::Result<Vec<String>> {
        let bytes = Command::new(bazel)
            .current_dir(workspace)
            .arg("query")
            .arg(path)
            .output()?
            .stdout;

        let path = str::from_utf8(&bytes)?
            .split(':')
            .next()
            .expect("string split must have one item");

        Ok(vec![format!("{path}:*")])
    }

    fn query_buildfile_targets(
        bazel: &Utf8Path,
        workspace: &Utf8Path,
        buildfile: &Utf8Path,
    ) -> anyhow::Result<Vec<String>> {
        let bytes = Command::new(bazel)
            .current_dir(workspace)
            .arg("query")
            .arg(buildfile)
            .output()?
            .stdout;

        let path = String::from_utf8(bytes)?;

        let targets = Command::new(bazel)
            .current_dir(workspace)
            .arg("query")
            .arg(format!(r#"'kind("rust_.* rule", siblings({path}))'"#))
            .output()?
            .stdout
            .lines()
            .collect::<Result<_, _>>()?;

        Ok(targets)
    }
}

impl FromStr for RustAnalyzerArg {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        serde_json::from_str(s).map_err(|e| anyhow::anyhow!("rust analyzer argument error: {e}"))
    }
}

pub fn generate_crate_info(
    bazel: impl AsRef<Utf8Path>,
    workspace: impl AsRef<Utf8Path>,
    rules_rust: impl AsRef<Utf8Path>,
    targets: &[String],
) -> anyhow::Result<()> {
    log::debug!("Building rust_analyzer_crate_spec files for {:?}", targets);

    let output = Command::new(bazel.as_ref())
        .current_dir(workspace.as_ref())
        .env_remove("BAZELISK_SKIP_WRAPPER")
        .env_remove("BUILD_WORKING_DIRECTORY")
        .env_remove("BUILD_WORKSPACE_DIRECTORY")
        .arg("build")
        .arg("--norun_validations")
        .arg(format!(
            "--aspects={}//rust:defs.bzl%rust_analyzer_aspect",
            rules_rust.as_ref()
        ))
        .arg("--output_groups=rust_analyzer_crate_spec,rust_generated_srcs")
        .args(targets)
        .output()?;

    if !output.status.success() {
        return Err(anyhow!(
            "bazel build failed:({})\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    Ok(())
}

pub fn generate_rust_project(
    bazel: impl AsRef<Utf8Path>,
    workspace: impl AsRef<Utf8Path>,
    rules_rust_name: impl AsRef<str>,
    targets: &[String],
    execution_root: impl AsRef<Utf8Path>,
) -> anyhow::Result<RustProject> {
    let crate_specs = aquery::get_crate_specs(
        bazel.as_ref(),
        workspace.as_ref(),
        execution_root.as_ref(),
        targets,
        rules_rust_name.as_ref(),
    )?;

    let path = runfiles::rlocation!(
        Runfiles::create()?,
        "rules_rust/rust/private/rust_analyzer_detect_sysroot.rust_analyzer_toolchain.json"
    )
    .with_context(|| "runfile failed")?;

    let toolchain_info: HashMap<String, String> =
        serde_json::from_str(&std::fs::read_to_string(path)?)?;

    let sysroot_src = &toolchain_info["sysroot_src"];
    let sysroot = &toolchain_info["sysroot"];

    let rust_project = rust_project::generate_rust_project(
        workspace.as_ref(),
        sysroot,
        sysroot_src,
        &crate_specs,
    )?;

    Ok(rust_project)
}

pub fn discover_project(
    bazel: impl AsRef<Utf8Path>,
    workspace: impl AsRef<Utf8Path>,
    rules_rust_name: impl AsRef<str>,
    targets: &[String],
    execution_root: impl AsRef<Utf8Path>,
) -> DiscoverProject {
    let res = generate_rust_project(bazel, workspace, rules_rust_name, targets, execution_root);
    match res {
        Ok(project) => DiscoverProject::Finished {
            buildfile: "/Users/bogdan/Coding/rules_rust/BUILD.bazel".into(),
            project,
        },
        Err(e) => DiscoverProject::Error {
            error: e.to_string(),
            source: None,
        },
    }
}

pub fn write_rust_project(
    bazel: impl AsRef<Utf8Path>,
    workspace: impl AsRef<Utf8Path>,
    rules_rust_name: impl AsRef<str>,
    targets: &[String],
    execution_root: impl AsRef<Utf8Path>,
    output_base: impl AsRef<Utf8Path>,
    rust_project_path: impl AsRef<Utf8Path>,
) -> anyhow::Result<()> {
    let rust_project = generate_rust_project(
        bazel.as_ref(),
        workspace.as_ref(),
        rules_rust_name.as_ref(),
        targets,
        execution_root.as_ref(),
    )?;

    rust_project::write_rust_project(
        rust_project_path.as_ref(),
        execution_root.as_ref(),
        output_base.as_ref(),
        &rust_project,
    )?;

    Ok(())
}
