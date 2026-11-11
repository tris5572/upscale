mod bundle;
mod cli;
mod decode;
mod engine;
mod pixels;
mod storage;

use std::path::Path;
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
    plan.check_collisions(&workspace)
        .map_err(|error| Failure { code: USAGE, error })?;

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
            Ok(()) => {
                succeeded += 1;
                if !options.quiet {
                    eprintln!("Saved {}", job.output.display());
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
) -> Result<(), Failure> {
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
        save(workspace, &job.output, &output)
    })();
    result.map_err(|error| {
        let mut failure = image_failure(with_input(error));
        if failure.error.is::<EngineError>() {
            failure.code = RUNTIME;
        }
        failure
    })
}

fn save(workspace: &Workspace, destination: &Path, output: &pixels::Output) -> Result<()> {
    let mut temporary = tempfile::NamedTempFile::new_in(workspace.path())?;
    pixels::encode_png(temporary.as_file_mut(), output)?;
    temporary.as_file().sync_all()?;
    check_interrupted()?;
    temporary
        .persist_noclobber(destination)
        .map_err(|error| error.error)
        .with_context(|| format!("cannot save {}", destination.display()))?;
    Ok(())
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
