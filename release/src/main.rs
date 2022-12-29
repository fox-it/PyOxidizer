// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use {
    anyhow::{anyhow, Context, Result},
    clap::{ArgMatches, Command},
    duct::cmd,
    git2::Repository,
    std::{
        ffi::OsString,
        io::{BufRead, BufReader},
        path::{Path, PathBuf},
    },
};

pub mod documentation;

/// Obtain the package version string from a Cargo.toml file.
fn cargo_toml_package_version(path: &Path) -> Result<String> {
    let manifest = cargo_toml::Manifest::from_path(path)?;

    Ok(manifest
        .package
        .ok_or_else(|| anyhow!("no [package]"))?
        .version()
        .to_string())
}

pub fn run_cmd<S>(
    package: &str,
    dir: &Path,
    program: &str,
    args: S,
    ignore_errors: Vec<String>,
) -> Result<i32>
where
    S: IntoIterator,
    S::Item: Into<OsString>,
{
    let mut found_ignore_string = false;

    let command = cmd(program, args)
        .dir(dir)
        .stderr_to_stdout()
        .unchecked()
        .reader()
        .context("launching command")?;
    {
        let reader = BufReader::new(&command);
        for line in reader.lines() {
            let line = line?;

            for s in ignore_errors.iter() {
                if line.contains(s) {
                    found_ignore_string = true;
                }
            }
            println!("{}: {}", package, line);
        }
    }
    let output = command
        .try_wait()
        .context("waiting on process")?
        .ok_or_else(|| anyhow!("unable to wait on command"))?;

    let code = output.status.code().unwrap_or(1);

    if output.status.success() || found_ignore_string {
        Ok(code)
    } else {
        Err(anyhow!(
            "command exited {}",
            output.status.code().unwrap_or(1)
        ))
    }
}

fn generate_new_project_cargo_lock(repo_root: &Path, pyembed_force_path: bool) -> Result<String> {
    // The lock file is derived from a new Rust project, similarly to the one that
    // `pyoxidizer init-rust-project` generates. Ideally we'd actually call that command.
    // However, there's a bit of a chicken and egg problem, especially as we call this
    // function as part of the release. So/ we emulate what the autogenerated Cargo.toml
    // would resemble. We don't need it to match exactly: we just need to ensure the
    // dependency set is complete.

    const PACKAGE_NAME: &str = "placeholder_project";

    let temp_dir = tempfile::TempDir::new()?;
    let project_path = temp_dir.path().join(PACKAGE_NAME);
    let cargo_toml_path = project_path.join("Cargo.toml");

    let pyembed_version =
        cargo_toml_package_version(&repo_root.join("pyembed").join("Cargo.toml"))?;

    let pyembed_entry = format!(
        "[dependencies.pyembed]\nversion = \"{}\"\ndefault-features = false\n",
        pyembed_version
    );

    // For pre-releases, refer to pyembed by its repo path, as pre-releases aren't
    // published. Otherwise, leave as-is: Cargo.lock should pick up the version published
    // on the registry and embed that metadata.
    let pyembed_entry = if pyembed_version.ends_with("-pre") || pyembed_force_path {
        format!(
            "{}path = \"{}\"\n",
            pyembed_entry,
            repo_root.join("pyembed").display()
        )
    } else {
        pyembed_entry
    };

    cmd(
        "cargo",
        vec![
            "init".to_string(),
            "--bin".to_string(),
            format!("{}", project_path.display()),
        ],
    )
    .stdout_to_stderr()
    .run()?;

    let extra_toml_path = repo_root
        .join("pyoxidizer")
        .join("src")
        .join("templates")
        .join("cargo-extra.toml.hbs");

    let mut manifest_data = std::fs::read_to_string(&cargo_toml_path)?;
    manifest_data.push_str(&pyembed_entry);

    // This is a handlebars template but it has nothing special. So just read as
    // a regualar file.
    manifest_data.push_str(&std::fs::read_to_string(extra_toml_path)?);

    std::fs::write(&cargo_toml_path, manifest_data.as_bytes())?;

    cmd("cargo", vec!["generate-lockfile", "--offline"])
        .dir(&project_path)
        .stdout_to_stderr()
        .run()?;

    let cargo_lock_path = project_path.join("Cargo.lock");

    // Filter out our placeholder package because the value will be different for
    // generated projects.
    let mut lock_file = cargo_lock::Lockfile::load(cargo_lock_path)?;

    lock_file.packages = lock_file
        .packages
        .drain(..)
        .filter(|package| package.name.as_str() != PACKAGE_NAME)
        .collect::<Vec<_>>();

    Ok(lock_file.to_string())
}

fn command_generate_new_project_cargo_lock(repo_root: &Path, _args: &ArgMatches) -> Result<()> {
    print!("{}", generate_new_project_cargo_lock(repo_root, false)?);

    Ok(())
}

fn command_synchronize_generated_files(repo_root: &Path) -> Result<()> {
    let cargo_lock = generate_new_project_cargo_lock(repo_root, false)?;
    documentation::generate_sphinx_files(repo_root)?;

    let pyoxidizer_src_path = repo_root.join("pyoxidizer").join("src");
    let lock_path = pyoxidizer_src_path.join("new-project-cargo.lock");

    println!("writing {}", lock_path.display());
    std::fs::write(&lock_path, cargo_lock.as_bytes())?;

    Ok(())
}

fn main_impl() -> Result<()> {
    let cwd = std::env::current_dir()?;

    let repo_root = if let Ok(repo) = Repository::discover(&cwd) {
        repo.workdir()
            .ok_or_else(|| anyhow!("unable to resolve working directory"))?
            .to_path_buf()
    } else if let Ok(output) = std::process::Command::new("sl")
        .arg("root")
        .current_dir(&cwd)
        .output()
    {
        if output.status.success() {
            let root = String::from_utf8(output.stdout)
                .expect("sl root should print UTF-8")
                .trim()
                .to_string();

            PathBuf::from(root)
        } else {
            return Err(anyhow!("could not find VCS root"));
        }
    } else {
        return Err(anyhow!("could not find VCS root"));
    };

    let matches = Command::new("PyOxidizer Releaser")
        .version("0.1")
        .author("Gregory Szorc <gregory.szorc@gmail.com>")
        .about("Perform releases from the PyOxidizer repository")
        .arg_required_else_help(true)
        .subcommand(
            Command::new("generate-new-project-cargo-lock")
                .about("Emit a Cargo.lock file for the pyembed crate"),
        )
        .subcommand(Command::new("synchronize-generated-files").about("Write out generated files"))
        .get_matches();

    match matches.subcommand() {
        Some(("generate-new-project-cargo-lock", args)) => {
            command_generate_new_project_cargo_lock(&repo_root, args)
        }
        Some(("synchronize-generated-files", _)) => command_synchronize_generated_files(&repo_root),
        _ => Err(anyhow!("invalid sub-command")),
    }
}

fn main() {
    let exit_code = match main_impl() {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("Error: {:?}", err);
            1
        }
    };

    std::process::exit(exit_code);
}
