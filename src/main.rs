mod bundle;
mod cli;
mod decode;
mod engine;
mod pixels;
mod storage;

use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;

use crate::bundle::Bundle;
use crate::cli::{Cli, ModelCommand, Operation, Options, Preset};
use crate::engine::{EngineError, check_interrupted};
use crate::storage::{Job, Plan, Workspace};

const FAILURE: u8 = 1;
const USAGE: u8 = 2;
const RUNTIME: u8 = 3;
const INTERRUPTED: u8 = 130;

struct Failure {
    code: u8,
    error: anyhow::Error,
}

fn main() -> ExitCode {
    let result = run(Cli::parse());
    if let Err(failure) = result {
        if failure.code != FAILURE || !failure.error.is::<Reported>() {
            eprintln!("upscale: {:#}", failure.error);
        }
        return ExitCode::from(failure.code);
    }
    ExitCode::SUCCESS
}

#[derive(Debug)]
struct Reported;

impl std::fmt::Display for Reported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("one or more images failed")
    }
}

impl std::error::Error for Reported {}

fn run(cli: Cli) -> Result<(), Failure> {
    match cli.operation {
        Some(Operation::Models {
            command: ModelCommand::List,
        }) => {
            for preset in Preset::ALL {
                println!("{preset}\t{}\t4x", preset.model());
            }
            Ok(())
        }
        Some(Operation::Doctor) => {
            engine::install_interrupt_handlers().map_err(runtime_failure)?;
            doctor().map_err(runtime_failure)
        }
        None => {
            let options = cli
                .options()
                .map_err(|error| Failure { code: USAGE, error })?;
            engine::install_interrupt_handlers().map_err(runtime_failure)?;
            upscale(&options)
        }
    }
}

fn doctor() -> Result<()> {
    let bundle = Bundle::open()?;
    eprintln!("Platform: macOS arm64");
    eprintln!("Bundle: {} (SHA-256 verified)", bundle.engine_version());
    let workspace = Workspace::new(&std::env::temp_dir())?;
    bundle.diagnose(&workspace)?;
    check_interrupted()?;
    eprintln!("Doctor: Vulkan/MoltenVK inference passed for both presets");
    Ok(())
}

fn upscale(options: &Options) -> Result<(), Failure> {
    let plan = Plan::new(options).map_err(|error| Failure { code: USAGE, error })?;
    let workspace = Workspace::new(&plan.parent).map_err(image_failure)?;
    if options.output.is_some() {
        plan.check_collisions(&workspace)
            .map_err(|error| Failure { code: USAGE, error })?;
    }

    let mut bundle = None;
    let mut succeeded = 0;
    let mut failed = 0;
    for (index, job) in plan.jobs.iter().enumerate() {
        check_interrupted().map_err(image_failure)?;
        if !options.quiet {
            eprintln!(
                "[{}/{}] {}",
                index + 1,
                plan.jobs.len(),
                job.input.display()
            );
        }
        let result = process(job, options, &workspace, &mut bundle);
        check_interrupted().map_err(image_failure)?;
        match result {
            Ok(destination) => {
                succeeded += 1;
                if !options.quiet {
                    eprintln!("Saved {}", destination.display());
                }
            }
            Err(failure)
                if !plan.batch || failure.code == RUNTIME || failure.code == INTERRUPTED =>
            {
                return Err(failure);
            }
            Err(failure) => {
                failed += 1;
                eprintln!("upscale: {:#}", failure.error);
            }
        }
    }
    if plan.batch && (!options.quiet || failed > 0) {
        eprintln!("Batch complete: {succeeded} succeeded, {failed} failed");
    }
    if failed > 0 {
        return Err(image_failure(Reported.into()));
    }
    Ok(())
}

fn process(
    job: &Job,
    options: &Options,
    workspace: &Workspace,
    bundle: &mut Option<Bundle>,
) -> Result<PathBuf, Failure> {
    let with_input = |error: anyhow::Error| error.context(job.input.display().to_string());
    let mut input = decode::input(&job.input).map_err(|error| {
        let code = if error.is::<decode::Unsupported>() {
            USAGE
        } else {
            FAILURE
        };
        Failure {
            code,
            error: with_input(error),
        }
    })?;
    let bundle = match bundle {
        Some(bundle) => bundle,
        slot => slot.insert(Bundle::open().map_err(runtime_failure)?),
    };
    let result = (|| {
        input.fill_transparent()?;
        let rgb = bundle
            .engine()
            .infer(workspace, &input.rgb, options.preset, options.tile)?;
        let output = input.finish(rgb, options.scale)?;
        save(workspace, &job.output, &output, options.output.is_none())
    })();
    result.map_err(|error| {
        let mut failure = image_failure(with_input(error));
        if failure.error.is::<EngineError>() {
            failure.code = RUNTIME;
        }
        failure
    })
}

fn save(
    workspace: &Workspace,
    destination: &Path,
    output: &pixels::Output,
    automatic: bool,
) -> Result<PathBuf> {
    let extension = destination
        .extension()
        .context("missing output extension")?;
    let stem = destination.file_stem().context("missing output filename")?;
    let mut temporary = tempfile::NamedTempFile::new_in(workspace.path())?;
    pixels::encode_png(temporary.as_file_mut(), output)?;
    temporary.as_file().sync_all()?;

    let mut path = destination.to_path_buf();
    let mut suffix = 1;
    loop {
        check_interrupted()?;
        match temporary.persist_noclobber(&path) {
            Ok(_) => return Ok(path),
            Err(error) if automatic && error.error.kind() == io::ErrorKind::AlreadyExists => {
                temporary = error.file;
                let mut name = stem.to_os_string();
                name.push(format!("({suffix})."));
                name.push(extension);
                path.set_file_name(name);
                suffix += 1;
            }
            Err(error) => {
                return Err(error.error).with_context(|| format!("cannot save {}", path.display()));
            }
        }
    }
}

fn image_failure(error: anyhow::Error) -> Failure {
    let code = if engine::interrupted() {
        INTERRUPTED
    } else {
        FAILURE
    };
    Failure { code, error }
}

fn runtime_failure(error: anyhow::Error) -> Failure {
    let code = if engine::interrupted() {
        INTERRUPTED
    } else {
        RUNTIME
    };
    Failure { code, error }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn automatic_save_retries_occupied_paths_without_overwriting_them() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        let destination = root.path().join("sample.photo_x4.PNG");
        let link = root.path().join("sample.photo_x4(1).PNG");
        let directory = root.path().join("sample.photo_x4(2).PNG");
        fs::write(&destination, b"existing").unwrap();
        symlink("missing", &link).unwrap();
        fs::create_dir(&directory).unwrap();

        let output = pixels::Output::Rgb(image::RgbImage::new(1, 1));
        let saved = save(&workspace, &destination, &output, true).unwrap();

        assert_eq!(saved, root.path().join("sample.photo_x4(3).PNG"));
        assert_eq!(decode::input(&saved).unwrap().rgb.dimensions(), (1, 1));
        assert_eq!(fs::read(&destination).unwrap(), b"existing");
        assert_eq!(fs::read_link(link).unwrap(), Path::new("missing"));
        assert!(directory.is_dir());
    }
}
