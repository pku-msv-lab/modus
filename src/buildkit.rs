//! This module contains code which handles buildkit integration.
//!
//! To reduce friction, it is best that we do not require the user to install
//! buildkit themselves, which is actually another system service when running
//! on its own. Instead, we make use of the buildkit integration built into
//! `docker build` (search for `DOCKER_BUILDKIT` env) by creating our own little
//! "frontend" for buildkit.  In our case this would be a shim not intended to
//! be used directly by the user, but can be invoked simply by spawning `docker
//! build` in our code.
//!
//! When a user tries to build an image with our buildkit backend,
//! the following things will happen in order:
//!
//! 1. The outer modus program generates all required proofs and build plan for
//! the user's query, then encodes the build plan to json.
//!
//! 2. The outer program writes the json along with a special declaration line
//! `#syntax=...` to a temporary Dockerfile, and invokes `docker build`. The
//! `#syntax` declaration tells Docker to use our frontend instead of treating
//! it as a normal Dockerfile.
//!
//! 3. Our frontend (which runs inside a separate container created by docker
//! build) turns the json back into the build plan and generate all the required
//! buildkit calls to build all output images. If there are more than one output
//! images, it also generate a final empty image which depeneds on all the
//! output images user wants. This will effectively force buildkit to build
//! multiple output images in parallel, if that's what the user wants.
//!
//! 4. If there are multiple output images, invoke docker build again for each
//! output image, and instead of building the empty image, just build the
//! required output image, and also ignore all "no_cache". This should be
//! instant since everything is cached. We can also tell docker build to tag the
//! image with a name so that it can be referenced by the user.
//!
//! The easiest way to create this inner frontend is to build a separate binary.
//! Check out `buildkit_frontend.rs` for the main function of this inner
//! frontend.

use std::{
    fs::{File, OpenOptions},
    process::{Command, Stdio},
    sync::{atomic::AtomicBool, Arc},
};

use buildkit_llb::prelude::*;
use signal_hook::{
    consts::{SIGCHLD, SIGINT, SIGTERM},
    iterator::{exfiltrator::SignalOnly, SignalsInfo},
};

use crate::{
    imagegen::{self, BuildPlan},
    logic::Literal,
    modusfile::Modusfile,
};

use colored::Colorize;
use rand::distributions::{Distribution, Uniform};
use std::io::Write;

use thiserror::Error;

#[derive(Error, Debug)]
pub enum BuildError {
    #[error("Could not open a temporary file under the current directory: {0}. This is required to invoke docker build correctly.")]
    UnableToCreateTempFile(#[source] std::io::Error),
    #[error("Unable to run docker build.")]
    UnableToRunDockerBuild(#[source] std::io::Error),
    #[error("docker build exited with code {0}.")]
    DockerBuildFailed(i32),
    #[error("{0} contains invalid utf-8.")]
    FileHasInvalidUtf8(String),
    #[error("{0}")]
    IOError(
        #[from]
        #[source]
        std::io::Error,
    ),
    #[error("Interrupted by user.")]
    Interrupted,
}

use BuildError::*;

fn invoke_buildkit(
    dockerfile: &TmpFilename,
    tag: Option<String>,
    target: Option<String>,
    has_dockerignore: bool,
    iidfile: Option<&str>,
    signals: &mut SignalsInfo<SignalOnly>,
) -> Result<(), BuildError> {
    let mut args = Vec::new();
    args.push("build".to_string());
    args.push(".".to_string());
    args.push("-f".to_string());
    args.push(dockerfile.name().to_string());
    if let Some(tag) = tag {
        args.push("-t".to_string());
        args.push(tag);
    }
    let quiet = target.is_some();
    if let Some(target) = target {
        args.push("--target".to_string());
        args.push(target);
    }
    if quiet {
        args.push("--quiet".to_string());
    }
    args.push("--build-arg".to_string());
    if has_dockerignore {
        args.push("has_dockerignore=true".to_string());
    } else {
        args.push("has_dockerignore=false".to_string());
    }
    if let Some(iidfile) = iidfile {
        args.push("--iidfile".to_string());
        args.push(iidfile.to_string());
    }
    // args.push("--progress=plain".to_string());
    let mut cmd = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .stdout(if quiet {
            Stdio::null()
        } else {
            Stdio::inherit()
        })
        .stderr(Stdio::inherit())
        .env("DOCKER_BUILDKIT", "1")
        .spawn()
        .map_err(|e| UnableToRunDockerBuild(e))?;
    let exit_status = loop {
        let mut has_sigchild = false;
        let mut has_term = false;
        for sig in signals.wait() {
            if sig == SIGCHLD {
                has_sigchild = true;
            } else if sig == SIGINT || sig == SIGTERM {
                has_term = true;
            }
        }
        if has_term {
            std::thread::sleep(std::time::Duration::from_millis(200)); // Wait for child to terminate first. This gives better terminal output.
            return Err(Interrupted);
        }
        if has_sigchild {
            let wait_res = cmd.try_wait().map_err(|e| IOError(e))?;
            if let Some(wait_res) = wait_res {
                break wait_res;
            }
        }
    };
    if !exit_status.success() {
        return Err(DockerBuildFailed(exit_status.code().unwrap_or(-1)));
    }
    Ok(())
}

/// A holder for a file name that deletes the file when dropped.
struct TmpFilename(String);
pub const TMP_PREFIX: &str = "buildkit_temp_";
pub const TMP_PREFIX_IGNORE_PATTERN: &str = "buildkit_temp_*";

impl TmpFilename {
    /// Generate a name, but does not create the file.
    fn gen(suffix: &str) -> Self {
        const RANDOM_LEN: usize = 15;
        let mut rng = rand::thread_rng();
        let letters = Uniform::new_inclusive('a', 'z');
        let mut name = String::with_capacity(TMP_PREFIX.len() + RANDOM_LEN + suffix.len());
        name.push_str(TMP_PREFIX);
        for _ in 0..RANDOM_LEN {
            name.push(letters.sample(&mut rng));
        }
        name.push_str(".Dockerfile");
        Self(name)
    }

    fn name(&self) -> &str {
        &self.0
    }
}

impl Drop for TmpFilename {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(self.name()) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "Warning: unable to remove temporary file {}: {}",
                    self.name(),
                    e
                );
            }
        }
    }
}

fn write_tmp_dockerfile(content: &str) -> Result<TmpFilename, std::io::Error> {
    let tmp_file = TmpFilename::gen(".Dockerfile");
    let mut f = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(tmp_file.name())?;
    f.write_all(content.as_bytes())?;
    Ok(tmp_file)
}

/// Returns the image IDs on success, following the order in build_plan.outputs.
pub fn build(build_plan: &BuildPlan) -> Result<Vec<String>, BuildError> {
    let mut signals = SignalsInfo::with_exfiltrator(&[SIGINT, SIGTERM, SIGCHLD], SignalOnly)
        .expect("Failed to create signal handler.");

    fn check_terminate(signals: &mut SignalsInfo<SignalOnly>) -> bool {
        signals.pending().any(|s| s == SIGINT || s == SIGTERM)
    }
    if check_terminate(&mut signals) {
        return Err(Interrupted);
    }
    let has_dockerignore = check_dockerignore()?;
    let mut content = String::new();
    content.push_str("#syntax=europe-docker.pkg.dev/maowtm/modus-test/modus-bk-frontend"); // TODO
    content.push('\n');
    content.push_str(&serde_json::to_string(build_plan).expect("Unable to serialize build plan"));
    if check_terminate(&mut signals) {
        return Err(Interrupted);
    }
    let dockerfile = write_tmp_dockerfile(&content).map_err(|e| UnableToCreateTempFile(e))?;
    match build_plan.outputs.len() {
        0 => unreachable!(), // not possible because if there is no solution to the initial query, there will be an SLD failure.
        1 => {
            println!("{}", "Running docker build...".blue());
            let iidfile = TmpFilename::gen(".iid");
            invoke_buildkit(
                &dockerfile,
                None,
                None,
                has_dockerignore,
                Some(iidfile.name()),
                &mut signals,
            )?;
            if check_terminate(&mut signals) {
                return Err(Interrupted);
            }
            let iid = std::fs::read_to_string(iidfile.name())?;
            Ok(vec![iid])
        }
        nb_outputs => {
            println!("{}", "Running docker build...".blue());
            if check_terminate(&mut signals) {
                return Err(Interrupted);
            }
            invoke_buildkit(
                &dockerfile,
                None,
                None,
                has_dockerignore,
                None,
                &mut signals,
            )?;
            let mut res = Vec::with_capacity(nb_outputs);
            let stderr = std::io::stderr();
            // TODO: check isatty
            let mut stderr = stderr.lock();
            write!(stderr, "\x1b[1A\x1b[2K\r=== Build success, exporting individual images ===\n")?;
            for i in 0..nb_outputs {
                let target_str = format!("{}", i);
                write!(
                    stderr,
                    "{}",
                    format!(
                        "Exporting {}/{}: {}",
                        i + 1,
                        nb_outputs,
                        build_plan.outputs[i]
                            .source_literal
                            .as_ref()
                            .expect("Expected source_literal to present in build plan")
                            .to_string()
                    )
                    .blue()
                )?;
                stderr.flush()?;
                let iidfile = TmpFilename::gen(".iid");
                invoke_buildkit(
                    &dockerfile,
                    None,
                    Some(target_str),
                    has_dockerignore,
                    Some(iidfile.name()),
                    &mut signals,
                )?;
                let iid = std::fs::read_to_string(iidfile.name())?;
                write!(stderr, "{}", format!(" -> {}\n", &iid).blue())?;
                stderr.flush()?;
                res.push(iid);
            }
            Ok(res)
        }
    }
}

pub fn check_dockerignore() -> Result<bool, BuildError> {
    match std::fs::read(".dockerignore") {
        Ok(content) => {
            if std::str::from_utf8(&content).is_err() {
                Err(FileHasInvalidUtf8(".dockerignore".to_string()))
            } else {
                Ok(true)
            }
        }
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                Err(e.into())
            } else {
                Ok(false)
            }
        }
    }
}
