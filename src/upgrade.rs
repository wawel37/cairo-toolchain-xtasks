//! Update toolchain crates properly.

use anyhow::{bail, Result};
use clap::{Parser, ValueEnum};
use semver::Version;
use std::mem;
use std::path::PathBuf;
use std::sync::OnceLock;
use toml_edit::{DocumentMut, InlineTable, Value};
use xshell::{cmd, Shell};

/// Update toolchain crates properly.
#[derive(Parser)]
pub struct Args {
    /// Name of toolchain dependency (group) to update.
    dep: DepName,

    #[command(flatten)]
    spec: Spec,

    /// Do not edit any files, just inform what would be done.
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

#[derive(ValueEnum, Copy, Clone, Debug)]
enum DepName {
    Cairo,
    #[value(name = "cairols")]
    CairoLS,
    #[value(name = "cairolint")]
    CairoLint,
}

#[derive(clap::Args, Clone, Default)]
#[group(required = true, multiple = true)]
struct Spec {
    /// Source the dependency from crates.io and use a specific version.
    version: Option<Version>,

    /// Source the dependency from the GitHub repository and use a specific commit/ref.
    #[arg(short, long, conflicts_with = "branch")]
    rev: Option<String>,

    /// Source the dependency from the GitHub repository and use a specific branch.
    #[arg(short, long)]
    branch: Option<String>,

    /// Source the dependency from a local filesystem.
    ///
    /// This is useful for local development, but avoid commiting this to the repository.
    #[arg(short, long, conflicts_with_all = ["rev", "branch"])]
    path: Option<PathBuf>,
}

pub fn main(args: Args) -> Result<()> {
    let sh = Shell::new()?;

    let mut cargo_toml = sh.read_file("Cargo.toml")?.parse::<DocumentMut>()?;

    edit_dependencies(&mut cargo_toml, "dependencies", &args);
    edit_dependencies(&mut cargo_toml, "dev-dependencies", &args);
    edit_dependencies(&mut cargo_toml, "workspace.dependencies", &args);
    edit_patch(&mut cargo_toml, &args);

    if !args.dry_run {
        sh.write_file("Cargo.toml", cargo_toml.to_string())?;

        cmd!(sh, "cargo fetch").run()?;

        purge_unused_patches(&mut cargo_toml)?;
        sh.write_file("Cargo.toml", cargo_toml.to_string())?;

        cmd!(sh, "cargo xtask sync-version").run()?;
    }

    Ok(())
}

fn edit_dependencies(cargo_toml: &mut DocumentMut, table_path: &str, args: &Args) {
    let Some(deps) = table_path
        .split('.')
        .try_fold(cargo_toml.as_item_mut(), |doc, key| doc.get_mut(key))
    else {
        return;
    };
    if deps.is_none() {
        return;
    }
    let deps = deps.as_table_mut().unwrap();

    for (_, dep) in deps.iter_mut().filter(|(key, _)| args.tool_owns_crate(key)) {
        let dep = dep.as_value_mut().unwrap();

        // Always use crates.io requirements so that we can reliably patch them with the
        // `[patch.crates-io]` table.
        let mut new_dep = InlineTable::from_iter([(
            "version",
            match &args.spec.version {
                Some(version) => Value::from(version.to_string()),
                None => Value::from("*"),
            },
        )]);

        copy_dependency_features(&mut new_dep, dep);

        *dep = new_dep.into();
        simplify_dependency_table(dep)
    }

    deps.fmt();
    deps.sort_values();

    eprintln!("[{table_path}]");
    for (key, dep) in deps.iter().filter(|(key, _)| args.tool_owns_crate(key)) {
        eprintln!("{key} = {dep}");
    }
}

fn edit_patch(cargo_toml: &mut DocumentMut, args: &Args) {
    let patch = cargo_toml["patch"].as_table_mut().unwrap()["crates-io"]
        .as_table_mut()
        .unwrap();

    // Clear any existing entries for this dependency.
    for crate_name in args.tool_crates() {
        patch.remove(crate_name);
    }

    // Leave this section as-if if we are requested to just use a specific version.
    if args.spec.rev.is_some() || args.spec.branch.is_some() || args.spec.path.is_some() {
        // Patch all Cairo crates that exist, even if this project does not directly depend on them,
        // to avoid any duplicates in transient dependencies.
        for &dep_name in args.tool_crates() {
            let mut dep = InlineTable::new();

            // Add a Git branch or revision reference if requested.
            if args.spec.rev.is_some() || args.spec.branch.is_some() {
                dep.insert("git", args.tool_repo().into());
            }

            if let Some(branch) = &args.spec.branch {
                dep.insert("branch", branch.as_str().into());
            }

            if let Some(rev) = &args.spec.rev {
                dep.insert("rev", rev.as_str().into());
            }

            // Add local path reference if requested.
            // For local path sources, Cargo is not looking for crates recursively therefore, we
            // need to manually provide full paths to Cairo workspace member crates.
            if let Some(path) = &args.spec.path {
                dep.insert(
                    "path",
                    path.join("crates")
                        .join(dep_name)
                        .to_string_lossy()
                        .into_owned()
                        .into(),
                );
            }

            patch.insert(dep_name, dep.into());
        }
    }

    patch.fmt();
    patch.sort_values();

    eprintln!("[patch.crates-io]");
    for (key, dep) in patch.iter() {
        eprintln!("{key} = {dep}");
    }
}

impl Args {
    fn tool_crates(&self) -> &'static [&'static str] {
        static CAIRO_CACHE: OnceLock<Vec<&str>> = OnceLock::new();
        match self.dep {
            DepName::Cairo => CAIRO_CACHE.get_or_init(|| {
                pull_cairo_packages_from_cairo_repository(&self.spec)
                    .unwrap()
                    .into_iter()
                    .map(|s| s.leak() as &str)
                    .collect()
            }),
            DepName::CairoLS => &["cairo-language-server"],
            DepName::CairoLint => &["cairo-lint-core"],
        }
    }

    fn tool_owns_crate(&self, crate_name: &str) -> bool {
        self.tool_crates().contains(&crate_name)
    }

    fn tool_repo(&self) -> &'static str {
        match self.dep {
            DepName::Cairo => "https://github.com/starkware-libs/cairo",
            DepName::CairoLS => "https://github.com/software-mansion/cairols",
            DepName::CairoLint => "https://github.com/software-mansion/cairo-lint",
        }
    }
}

/// Copies features from source dependency spec to new dependency table, if exists.
fn copy_dependency_features(dest: &mut InlineTable, src: &Value) {
    if let Some(dep) = src.as_inline_table() {
        if let Some(features) = dep.get("features") {
            dest.insert("features", features.clone());
        }
    }
}

/// Simplifies a `{ version = "V" }` dependency spec to shorthand `"V"` if possible.
fn simplify_dependency_table(dep: &mut Value) {
    *dep = match mem::replace(dep, false.into()) {
        Value::InlineTable(mut table) => {
            if table.len() == 1 {
                table.remove("version").unwrap_or_else(|| table.into())
            } else {
                table.into()
            }
        }

        dep => dep,
    }
}

/// Remove any unused patches from the `[patch.crates-io]` table.
///
/// We are adding patch entries for **all** Cairo crates existing, and some may end up being unused.
/// Cargo is emitting warnings about unused patches and keeps a record of them in the `Cargo.lock`.
/// The goal of this function is to resolve these warnings.
fn purge_unused_patches(cargo_toml: &mut DocumentMut) -> Result<()> {
    let sh = Shell::new()?;
    let cargo_lock = sh.read_file("Cargo.lock")?.parse::<DocumentMut>()?;

    if let Some(unused_patches) = find_unused_patches(&cargo_lock) {
        let patch = cargo_toml["patch"].as_table_mut().unwrap()["crates-io"]
            .as_table_mut()
            .unwrap();

        // Remove any patches that are not for Cairo crates.
        patch.retain(|key, _| !unused_patches.contains(&key.to_owned()));
    }

    Ok(())
}

/// Extracts names of unused patches from the `[[patch.unused]]` array from the `Cargo.lock` file.
fn find_unused_patches(cargo_lock: &DocumentMut) -> Option<Vec<String>> {
    Some(
        cargo_lock
            .get("patch")?
            .get("unused")?
            .as_array_of_tables()?
            .iter()
            .flat_map(|table| Some(table.get("name")?.as_str()?.to_owned()))
            .collect(),
    )
}

/// Pulls names of crates published from the `starkware-libs/cairo` repository.
///
/// The list is obtained by parsing the `scripts/release_crates.sh` script in that repo.
/// The resulting vector is sorted alphabetically.
fn pull_cairo_packages_from_cairo_repository(spec: &Spec) -> Result<Vec<String>> {
    let sh = Shell::new()?;

    let release_crates_sh = if let Some(path) = &spec.path {
        sh.read_file(path.join("scripts").join("release_crates.sh"))?
    } else {
        let rev = if let Some(version) = &spec.version {
            format!("refs/tags/v{version}")
        } else if let Some(rev) = &spec.rev {
            rev.to_string()
        } else if let Some(branch) = &spec.branch {
            format!("refs/heads/{branch}")
        } else {
            "refs/heads/main".to_string()
        };
        let url = format!("https://raw.githubusercontent.com/starkware-libs/cairo/{rev}/scripts/release_crates.sh");
        cmd!(sh, "curl -sSfL {url}").read()?
    };

    let Some((_, source_list)) = release_crates_sh.split_once("CRATES_TO_PUBLISH=(") else {
        bail!("failed to extract start of `CRATES_TO_PUBLISH` from `scripts/release_crates.sh`");
    };
    let Some((source_list, _)) = source_list.split_once(")") else {
        bail!("failed to extract end of `CRATES_TO_PUBLISH` from `scripts/release_crates.sh`");
    };

    let mut crates: Vec<String> = source_list
        .split_whitespace()
        .filter(|s| s.starts_with("cairo-lang-"))
        .map(|s| s.into())
        .collect();
    crates.sort();
    Ok(crates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pull_cairo_packages_from_cairo_repository() {
        let list = pull_cairo_packages_from_cairo_repository(&Spec::default()).unwrap();
        assert!(!list.is_empty());
        assert!(list.contains(&"cairo-lang-compiler".to_owned()));
        assert!(!list.contains(&"cairo-test".to_owned()));
        assert!(list.is_sorted());
    }
}
