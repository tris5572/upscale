use std::fmt;
use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    version,
    about = "Upscale still PNG and JPEG images locally using Real-ESRGAN.",
    subcommand_negates_reqs = true,
    args_conflicts_with_subcommands = true,
    arg_required_else_help = false
)]
pub struct Cli {
    /// A PNG or JPEG file, or a directory of images
    #[arg(required = true)]
    input: Option<PathBuf>,

    /// Output PNG for one image, or an output directory for a batch
    #[arg(short, long, required = true, value_name = "PATH")]
    output: Option<PathBuf>,

    /// Final multiplier for each side (2, 3 or 4). Inference always runs at 4x
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u32).range(2..=4))]
    scale: u32,

    /// general for photos and other images, anime for illustrations and line art
    #[arg(long, value_enum, default_value_t = Preset::General)]
    preset: Preset,

    /// Inference tile size in input pixels (auto or an integer of at least 32)
    #[arg(long, default_value = "auto", value_parser = parse_tile)]
    tile: Tile,

    /// Suppress progress and success messages
    #[arg(long)]
    quiet: bool,

    #[command(subcommand)]
    pub operation: Option<Operation>,
}

#[derive(Subcommand)]
pub enum Operation {
    /// Verify the platform, bundle integrity and GPU inference
    Doctor,
    /// Show the bundled model presets
    Models {
        #[command(subcommand)]
        command: ModelCommand,
    },
}

#[derive(Subcommand)]
pub enum ModelCommand {
    /// List preset names, model names and native scales
    List,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Preset {
    General,
    Anime,
}

impl Preset {
    pub const ALL: [Self; 2] = [Self::General, Self::Anime];

    pub const fn model(self) -> &'static str {
        match self {
            Self::General => "realesrgan-x4plus",
            Self::Anime => "realesrgan-x4plus-anime",
        }
    }
}

impl fmt::Display for Preset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.to_possible_value().expect("preset name").get_name())
    }
}

#[derive(Clone, Copy)]
pub enum Tile {
    Auto,
    Fixed(i32),
}

fn parse_tile(value: &str) -> Result<Tile, &'static str> {
    if value == "auto" {
        return Ok(Tile::Auto);
    }
    // ncnn multiplies the tile size by the model scale in a signed integer.
    const MAX_TILE: i32 = i32::MAX / 4;
    value
        .parse::<i32>()
        .ok()
        .filter(|&size| (32..=MAX_TILE).contains(&size))
        .map(Tile::Fixed)
        .ok_or("expected auto or an integer from 32 to 536870911")
}

pub struct Options {
    pub input: PathBuf,
    pub output: PathBuf,
    pub scale: u32,
    pub preset: Preset,
    pub tile: Tile,
    pub quiet: bool,
}

impl Cli {
    pub fn options(self) -> Result<Options> {
        Ok(Options {
            input: self
                .input
                .ok_or_else(|| anyhow!("input path is required"))?,
            output: self
                .output
                .ok_or_else(|| anyhow!("output path is required"))?,
            scale: self.scale,
            preset: self.preset,
            tile: self.tile,
            quiet: self.quiet,
        })
    }
}
