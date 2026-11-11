use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use image::RgbImage;

use crate::cli::{Preset, Tile};
use crate::decode;
use crate::pixels::encode_rgb_png;
use crate::storage::Workspace;

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
// The pinned engine auto-selects from 32, 64, 100 and 200.
const AUTO_TILES: [i32; 2] = [0, 32];

pub fn install_interrupt_handlers() -> Result<()> {
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // The handler only stores into a lock-free atomic.
        unsafe {
            signal_hook_registry::register(signal, || INTERRUPTED.store(true, Ordering::Relaxed))?;
        }
    }
    Ok(())
}

pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::Relaxed)
}

pub fn check_interrupted() -> Result<()> {
    if interrupted() {
        bail!("interrupted");
    }
    Ok(())
}

#[derive(Debug)]
pub struct EngineError(pub String);

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EngineError {}

pub struct Engine {
    executable: PathBuf,
    models: PathBuf,
}

impl Engine {
    pub fn new(executable: PathBuf, models: PathBuf) -> Self {
        Self { executable, models }
    }

    pub fn infer(
        &self,
        workspace: &Workspace,
        rgb: &RgbImage,
        preset: Preset,
        tile: Tile,
    ) -> Result<RgbImage> {
        check_interrupted()?;
        let input = workspace.path().join("input.png");
        let output = workspace.path().join("inferred.png");
        encode_rgb_png(File::create(&input)?, rgb)?;
        let dimensions = (rgb.width() * 4, rgb.height() * 4);
        let tiles: &[i32] = match &tile {
            Tile::Auto => &AUTO_TILES,
            Tile::Fixed(size) => std::slice::from_ref(size),
        };

        for (index, &tile) in tiles.iter().enumerate() {
            match fs::remove_file(&output) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }

            let run = self.run(workspace, &input, &output, preset, tile)?;
            if let Some(message) = run.environment_error() {
                return Err(EngineError(message).into());
            }
            if run.out_of_memory() {
                if index + 1 < tiles.len() && rgb.width().max(rgb.height()) > 32 {
                    continue;
                }
                bail!("GPU ran out of memory with tile {tile}{}", run.details());
            }
            if !run.status.success() || run.gpu_error() {
                bail!("inference failed ({}){}", run.status, run.details());
            }
            return decode::inferred(&output, dimensions)
                .with_context(|| format!("invalid inference result{}", run.details()));
        }
        unreachable!("there is always at least one tile option")
    }

    fn run(
        &self,
        workspace: &Workspace,
        input: &Path,
        output: &Path,
        preset: Preset,
        tile: i32,
    ) -> Result<Run> {
        check_interrupted()?;
        let mut log = tempfile::tempfile_in(workspace.path())?;
        let lease = workspace.lease_fd();
        let mut command = Command::new(&self.executable);
        command
            .current_dir(workspace.path())
            .env("LC_ALL", "C")
            .arg("-i")
            .arg(input)
            .arg("-o")
            .arg(output)
            .arg("-m")
            .arg(&self.models)
            .args(["-n", preset.model(), "-s", "4", "-f", "png", "-t"])
            .arg(tile.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log.try_clone()?)
            .process_group(0);
        // Hold the workspace lease until the engine exits even if this process
        // is killed. Only the child inherits the open descriptor.
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(lease, libc::F_SETFD, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().map_err(|error| {
            EngineError(format!(
                "cannot start {}: {error}",
                self.executable.display()
            ))
        })?;
        let status = loop {
            if interrupted() {
                let _ = child.kill();
                child.wait().context("cannot reap interrupted engine")?;
                check_interrupted()?;
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => thread::sleep(Duration::from_millis(25)),
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error).context("cannot wait for inference engine");
                }
            }
        };
        let length = log.metadata()?.len();
        log.seek(SeekFrom::Start(length.saturating_sub(64 * 1024)))?;
        let mut bytes = Vec::new();
        log.read_to_end(&mut bytes)?;
        Ok(Run {
            status,
            log: String::from_utf8_lossy(&bytes).into_owned(),
        })
    }
}

struct Run {
    status: ExitStatus,
    log: String,
}

impl Run {
    fn environment_error(&self) -> Option<String> {
        if self.log.lines().any(|line| {
            line.contains("invalid gpu device")
                || line.contains("VK_ERROR_INCOMPATIBLE_DRIVER")
                || line.contains("VK_ERROR_INITIALIZATION_FAILED")
                || line.contains("VK_ERROR_DEVICE_LOST")
                || line.contains("vkCreateDevice failed")
                || line.contains("Library not loaded:")
                || (line.contains("vkCreateInstance failed")
                    && !line.ends_with(" -1")
                    && !line.ends_with(" -2"))
        }) {
            return Some(format!("Vulkan/MoltenVK GPU unavailable{}", self.details()));
        }
        None
    }

    fn out_of_memory(&self) -> bool {
        self.log.lines().any(|line| {
            line.contains("VK_ERROR_OUT_OF_DEVICE_MEMORY")
                || line.contains("VK_ERROR_OUT_OF_HOST_MEMORY")
                || ([
                    "vkAllocateMemory failed",
                    "vkCreateBuffer failed",
                    "vkAllocateDescriptorSets failed",
                ]
                .iter()
                .any(|marker| line.contains(marker))
                    && matches!(line.split_whitespace().last(), Some("-1" | "-2")))
        })
    }

    fn gpu_error(&self) -> bool {
        self.log.lines().any(|line| {
            line.contains("[mvk-error]")
                || line.contains("VK_ERROR_")
                || (line.starts_with("vk") && line.contains(" failed"))
        })
    }

    fn details(&self) -> String {
        let log = self.log.trim();
        if log.is_empty() {
            String::new()
        } else {
            format!("\n{log}")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn only_confirmed_memory_errors_on_auto_are_retried() {
        let root = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(root.path()).unwrap();
        let executable = root.path().join("test-engine");
        let engine = Engine::new(executable.clone(), root.path().to_path_buf());
        let input = RgbImage::new(40, 24);
        encode_rgb_png(
            File::create(workspace.path().join("reference.png")).unwrap(),
            &RgbImage::new(160, 96),
        )
        .unwrap();

        fs::write(
            &executable,
            r#"#!/bin/sh
out="$4"
shift 12
printf '%s\n' "$2" >> attempts
if [ "$2" != 32 ]; then
    echo VK_ERROR_OUT_OF_DEVICE_MEMORY >&2
    exit 1
fi
/bin/cp reference.png "$out"
"#,
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            engine
                .infer(&workspace, &input, Preset::General, Tile::Auto)
                .unwrap()
                .dimensions(),
            (160, 96)
        );
        let attempts = workspace.path().join("attempts");
        assert_eq!(fs::read_to_string(&attempts).unwrap(), "0\n32\n");

        fs::remove_file(&attempts).unwrap();
        assert!(
            engine
                .infer(&workspace, &input, Preset::General, Tile::Fixed(128))
                .is_err()
        );
        assert_eq!(fs::read_to_string(&attempts).unwrap(), "128\n");

        fs::write(&executable, "#!/bin/sh\necho called >> attempts\nexit 9\n").unwrap();
        fs::remove_file(&attempts).unwrap();
        assert!(
            engine
                .infer(&workspace, &input, Preset::General, Tile::Auto)
                .is_err()
        );
        assert_eq!(fs::read_to_string(&attempts).unwrap(), "called\n");

        fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        assert!(
            engine
                .infer(&workspace, &input, Preset::General, Tile::Auto)
                .is_err()
        );
    }
}
