use std::ffi::OsString;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tempfile::TempDir;

use crate::cli::Options;

const PREFIX: &str = ".upscale-work-";
const RANDOM_LEN: usize = 16;
const MARKER: &str = ".lease";
const MARKER_CONTENT: &[u8] = b"upscale workspace v1\n";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Png,
    Jpeg,
}

impl Format {
    pub fn from_extension(path: &Path) -> Option<Self> {
        let extension = path.extension()?.as_encoded_bytes();
        if extension.eq_ignore_ascii_case(b"png") {
            Some(Self::Png)
        } else if extension.eq_ignore_ascii_case(b"jpg") || extension.eq_ignore_ascii_case(b"jpeg")
        {
            Some(Self::Jpeg)
        } else {
            None
        }
    }
}

pub struct Job {
    pub input: PathBuf,
    pub output: PathBuf,
}

pub struct Plan {
    pub parent: PathBuf,
    pub batch: bool,
    pub jobs: Vec<Job>,
}

impl Plan {
    pub fn new(options: &Options) -> Result<Self> {
        let input = fs::metadata(&options.input)
            .with_context(|| format!("cannot access input {}", options.input.display()))?;
        if input.is_dir() {
            return Self::batch(options, &input);
        }
        if !input.is_file() || Format::from_extension(&options.input).is_none() {
            bail!("input must be a PNG or JPEG file, or a directory");
        }
        let output = if let Some(output) = &options.output {
            if Format::from_extension(output) != Some(Format::Png) {
                bail!("output filename must end in .png");
            }
            output.clone()
        } else {
            options
                .input
                .with_file_name(scaled_output_name(&options.input, options.scale))
        };
        let parent = output
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        if !parent.is_dir() {
            bail!("output parent is not a directory: {}", parent.display());
        }
        if options.output.is_some() {
            ensure_new_output(&output)?;
        }
        Ok(Self {
            parent: parent.canonicalize()?,
            batch: false,
            jobs: vec![Job {
                input: options.input.clone(),
                output,
            }],
        })
    }

    fn batch(options: &Options, input: &fs::Metadata) -> Result<Self> {
        let mut paths = Vec::new();
        for entry in fs::read_dir(&options.input)? {
            let path = entry?.path();
            if Format::from_extension(&path).is_some() && path.is_file() {
                paths.push(path);
            }
        }
        paths.sort_unstable_by(|left, right| left.file_name().cmp(&right.file_name()));
        if paths.is_empty() {
            bail!("no PNG or JPEG files in {}", options.input.display());
        }

        let parent = if let Some(output) = &options.output {
            fs::create_dir_all(output)
                .with_context(|| format!("cannot create output directory {}", output.display()))?;
            let parent = output.canonicalize()?;
            if same_file(input, &parent.metadata()?) {
                bail!("input and output directories refer to the same location");
            }
            parent
        } else {
            options.input.canonicalize()?
        };

        let jobs = paths
            .into_iter()
            .map(|path| {
                let name = scaled_output_name(&path, options.scale);
                Job {
                    input: path,
                    output: parent.join(name),
                }
            })
            .collect();
        Ok(Self {
            parent,
            batch: true,
            jobs,
        })
    }

    pub fn check_collisions(&self, workspace: &Workspace) -> Result<()> {
        if !self.batch {
            return Ok(());
        }
        // Let the destination filesystem decide whether names alias by case or normalization.
        let names = workspace.path().join("names");
        fs::create_dir(&names)?;
        for job in &self.jobs {
            ensure_new_output(&job.output)?;
            let name = job.output.file_name().context("missing output filename")?;
            match File::create_new(names.join(name)) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    bail!("multiple inputs map to {}", job.output.display());
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("invalid output filename {name:?}"));
                }
            }
        }
        Ok(())
    }
}

fn scaled_output_name(input: &Path, scale: u32) -> OsString {
    let mut name = input.file_stem().unwrap_or_default().to_os_string();
    name.push(format!("_x{scale}.png"));
    name
}

fn ensure_new_output(path: &Path) -> Result<()> {
    match path.symlink_metadata() {
        Ok(_) => bail!("output already exists: {}", path.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("cannot inspect {}", path.display())),
    }
}

fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

pub struct Workspace {
    directory: TempDir,
    lease: File,
}

impl Workspace {
    pub fn new(parent: &Path) -> Result<Self> {
        reclaim(parent)?;
        for _ in 0..3 {
            let directory = tempfile::Builder::new()
                .prefix(PREFIX)
                .rand_bytes(RANDOM_LEN)
                .permissions(fs::Permissions::from_mode(0o700))
                .tempdir_in(parent)?;
            match fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            }
            if let Some(lease) = lease(directory.path())? {
                fs::write(directory.path().join(MARKER), MARKER_CONTENT)?;
                return Ok(Self { directory, lease });
            }
        }
        bail!("cannot acquire a private workspace in {}", parent.display());
    }

    pub fn path(&self) -> &Path {
        self.directory.path()
    }

    pub fn lease_fd(&self) -> RawFd {
        self.lease.as_raw_fd()
    }
}

fn reclaim(parent: &Path) -> Result<()> {
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(suffix) = name.as_encoded_bytes().strip_prefix(PREFIX.as_bytes()) else {
            continue;
        };
        if suffix.len() != RANDOM_LEN || !suffix.iter().all(u8::is_ascii_alphanumeric) {
            continue;
        }
        let path = entry.path();
        if let Some(_lease) = lease(&path)? {
            let marker = path.join(MARKER);
            let owned = marker.symlink_metadata().is_ok_and(|metadata| {
                metadata.is_file() && metadata.len() == MARKER_CONTENT.len() as u64
            }) && fs::read(marker).is_ok_and(|contents| contents == MARKER_CONTENT);
            let removal = if owned {
                fs::remove_dir_all(path)
            } else {
                // A process killed just after mkdir can leave an empty directory.
                fs::remove_dir(path)
            };
            match removal {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
                    ) => {}
                Err(error) => {
                    return Err(error).context("cannot remove abandoned upscale workspace");
                }
            }
        }
    }
    Ok(())
}

fn lease(path: &Path) -> Result<Option<File>> {
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    // The temporary namespace is private to the current effective user.
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o777 != 0o700
    {
        return Ok(None);
    }
    let handle = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(handle) => handle,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    match handle.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(None),
        Err(TryLockError::Error(error)) => return Err(error.into()),
    }
    // A creator and a scavenger may reach a new directory simultaneously.
    let current = match path.symlink_metadata() {
        Ok(current) => current,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !same_file(&metadata, &current) || !same_file(&handle.metadata()?, &current) {
        return Ok(None);
    }
    Ok(Some(handle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abandoned_workspaces_are_reclaimed_while_active_ones_are_leased() {
        let root = tempfile::tempdir().unwrap();
        let first = Workspace::new(root.path()).unwrap();
        let second = Workspace::new(root.path()).unwrap();
        assert!(first.path().is_dir());
        let Workspace { directory, lease } = first;
        let abandoned = directory.keep();
        drop(lease);
        let foreign = root.path().join(".upscale-work-XXXXXXXXXXXXXXXX");
        fs::create_dir(&foreign).unwrap();
        fs::set_permissions(&foreign, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(foreign.join("personal.png"), b"private").unwrap();

        let third = Workspace::new(root.path()).unwrap();
        assert!(!abandoned.exists());
        assert!(foreign.join("personal.png").is_file());
        assert!(second.path().is_dir());
        assert!(third.path().is_dir());
    }
}
