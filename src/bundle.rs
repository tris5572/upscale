use std::fs::{self, File};
use std::io;
use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::cli::{Preset, Tile};
use crate::decode;
use crate::engine::{Engine, check_interrupted};
use crate::storage::Workspace;

const MANIFEST: &str = include_str!("../resources/manifest.json");
const ENGINE: &str = "libexec/realesrgan-ncnn-vulkan";

#[derive(Deserialize)]
struct Manifest {
    engine_version: String,
    files: Vec<Resource>,
}

#[derive(Deserialize)]
struct Resource {
    path: String,
    sha256: String,
}

pub struct Bundle {
    root: PathBuf,
    manifest: Manifest,
    engine: Engine,
}

impl Bundle {
    pub fn open() -> Result<Self> {
        ensure!(
            cfg!(all(target_os = "macos", target_arch = "aarch64")),
            "macOS arm64 is required"
        );
        let executable = std::env::current_exe()?.canonicalize()?;
        let root = executable
            .parent()
            .and_then(|bin| bin.parent())
            .context("cannot locate the bundle relative to the executable")?;
        let manifest_path = root.join("manifest.json");
        let contents = fs::read(&manifest_path).with_context(|| {
            format!(
                "missing distribution manifest at {}",
                manifest_path.display()
            )
        })?;
        let mut distributed: serde_json::Value =
            serde_json::from_slice(&contents).context("invalid distribution manifest")?;
        distributed
            .as_object_mut()
            .context("invalid distribution manifest")?
            .remove("distribution_files");
        let expected: serde_json::Value = serde_json::from_str(MANIFEST)?;
        ensure!(
            distributed == expected,
            "bundle manifest does not match this version of upscale"
        );
        let manifest: Manifest = serde_json::from_str(MANIFEST)?;
        for resource in &manifest.files {
            check_interrupted()?;
            let path = root.join(&resource.path);
            let mut file = File::open(&path)
                .with_context(|| format!("missing bundle resource {}", path.display()))?;
            ensure!(
                file.metadata()?.is_file(),
                "bundle resource is not a file: {}",
                path.display()
            );
            let mut hash = Sha256::new();
            io::copy(&mut file, &mut hash)
                .with_context(|| format!("cannot verify {}", path.display()))?;
            let hash = format!("{:x}", hash.finalize());
            ensure!(
                hash == resource.sha256,
                "SHA-256 mismatch: {}",
                path.display()
            );
        }
        Ok(Self {
            root: root.to_path_buf(),
            engine: Engine::new(root.join(ENGINE), root.join("models")),
            manifest,
        })
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn engine_version(&self) -> &str {
        &self.manifest.engine_version
    }

    pub fn diagnose(&self, workspace: &Workspace) -> Result<()> {
        const MAX_MEAN_ERROR: f64 = 1.0;
        const MAX_SAMPLE_ERROR: u8 = 16;
        let directory = self.root.join("diagnostics");
        let input = decode::input(&directory.join("input.png"))?;
        let dimensions = (input.rgb.width() * 4, input.rgb.height() * 4);
        for preset in Preset::ALL {
            let reference = decode::inferred(&directory.join(format!("{preset}.png")), dimensions)?;
            let actual = self
                .engine
                .infer(workspace, &input.rgb, preset, Tile::Auto)
                .with_context(|| format!("{preset} GPU diagnostic failed"))?;
            let (sum, max) = actual
                .as_raw()
                .iter()
                .zip(reference.as_raw())
                .map(|(&actual, &reference)| actual.abs_diff(reference))
                .fold((0u64, 0), |(sum, max), difference| {
                    (sum + u64::from(difference), max.max(difference))
                });
            let mean = sum as f64 / reference.as_raw().len() as f64;
            ensure!(
                mean <= MAX_MEAN_ERROR && max <= MAX_SAMPLE_ERROR,
                "{preset} diagnostic exceeded tolerance (mean {mean:.3}, max {max})"
            );
            eprintln!(
                "  {preset}: {}×{}, mean difference {mean:.3}, max {max}",
                dimensions.0, dimensions.1
            );
        }
        Ok(())
    }
}
