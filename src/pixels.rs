use std::collections::VecDeque;
use std::io::Write;

use anyhow::Result;
use image::imageops::{FilterType, resize};
use image::{GrayImage, ImageBuffer, Luma, RgbImage, Rgba32FImage, RgbaImage};

use crate::engine::check_interrupted;

pub struct Input {
    pub rgb: RgbImage,
    pub alpha: Option<GrayImage>,
}

pub enum Output {
    Rgb(RgbImage),
    Rgba(RgbaImage),
}

impl Input {
    pub fn fill_transparent(&mut self) -> Result<()> {
        let Some(alpha) = &self.alpha else {
            return Ok(());
        };
        if alpha.as_raw().iter().all(|&value| value != 0) {
            return Ok(());
        }
        let (width, height) = alpha.dimensions();
        let width = width as usize;
        let mut filled: Vec<bool> = alpha.as_raw().iter().map(|&value| value != 0).collect();
        let mut queue: VecDeque<_> = filled
            .iter()
            .enumerate()
            .filter_map(|(index, &visible)| visible.then_some(index))
            .collect();
        let rgb: &mut [u8] = self.rgb.as_mut();
        if queue.is_empty() {
            rgb.fill(0);
            return Ok(());
        }

        // Multi-source flood fill assigns each transparent pixel its nearest
        // visible color by four-connected distance.
        let mut visited = 0;
        while let Some(index) = queue.pop_front() {
            if visited % 16384 == 0 {
                check_interrupted()?;
            }
            visited += 1;
            let x = index % width;
            let y = index / width;
            let neighbors = [
                (x > 0).then(|| index - 1),
                (x + 1 < width).then(|| index + 1),
                (y > 0).then(|| index - width),
                (y + 1 < height as usize).then(|| index + width),
            ];
            for next in neighbors.into_iter().flatten() {
                if !filled[next] {
                    filled[next] = true;
                    rgb.copy_within(index * 3..index * 3 + 3, next * 3);
                    queue.push_back(next);
                }
            }
        }
        Ok(())
    }

    pub fn finish(self, inferred: RgbImage, scale: u32) -> Result<Output> {
        let (width, height) = (self.rgb.width() * scale, self.rgb.height() * scale);
        let Some(alpha) = self.alpha else {
            let rgb = if scale == 4 {
                inferred
            } else {
                resize(&inferred, width, height, FilterType::Lanczos3)
            };
            return Ok(Output::Rgb(rgb));
        };

        let alpha = ImageBuffer::from_fn(alpha.width(), alpha.height(), |x, y| {
            Luma([f32::from(alpha.get_pixel(x, y)[0]) / 255.0])
        });
        let target_alpha = resize(&alpha, width, height, FilterType::Lanczos3);
        let mut output = RgbaImage::new(width, height);
        if scale == 4 {
            for ((out, rgb), alpha) in output
                .pixels_mut()
                .zip(inferred.pixels())
                .zip(target_alpha.pixels())
            {
                let alpha = sample(alpha[0]);
                out.0 = if alpha > 0 {
                    [rgb[0], rgb[1], rgb[2], alpha]
                } else {
                    [0; 4]
                };
            }
            return Ok(Output::Rgba(output));
        }

        check_interrupted()?;
        let alpha4 = resize(
            &alpha,
            inferred.width(),
            inferred.height(),
            FilterType::Lanczos3,
        );
        let mut premultiplied = Rgba32FImage::new(inferred.width(), inferred.height());
        for ((out, rgb), alpha) in premultiplied
            .pixels_mut()
            .zip(inferred.pixels())
            .zip(alpha4.pixels())
        {
            // Leave headroom for positive Lanczos lobes before dividing
            // the filtered color by the filtered coverage.
            let a = 0.25 * alpha[0].clamp(0.0, 1.0);
            let channel = |index| f32::from(rgb[index]) / 255.0 * a;
            out.0 = [channel(0), channel(1), channel(2), a];
        }
        drop(alpha4);
        drop(inferred);

        check_interrupted()?;
        let reduced = resize(&premultiplied, width, height, FilterType::Lanczos3);
        for ((out, premul), alpha) in output
            .pixels_mut()
            .zip(reduced.pixels())
            .zip(target_alpha.pixels())
        {
            let alpha = sample(alpha[0]);
            let weight = premul[3];
            // The filtered 4x alpha is the weight of the filtered RGB. The
            // saved alpha comes directly from the original image.
            if alpha > 0 && weight > f32::EPSILON {
                out.0 = [
                    sample(premul[0] / weight),
                    sample(premul[1] / weight),
                    sample(premul[2] / weight),
                    alpha,
                ];
            }
        }
        // The two Lanczos passes can move a near-zero coverage crossing.
        // Preserve directly sampled alpha there and extend the nearest
        // reconstructed color into those pixels.
        restore_unweighted_colors(&mut output, &reduced, &target_alpha)?;
        Ok(Output::Rgba(output))
    }
}

fn restore_unweighted_colors(
    output: &mut RgbaImage,
    reduced: &Rgba32FImage,
    target_alpha: &ImageBuffer<Luma<f32>, Vec<f32>>,
) -> Result<()> {
    if !reduced
        .pixels()
        .zip(target_alpha.pixels())
        .any(|(premul, alpha)| premul[3] <= f32::EPSILON && sample(alpha[0]) > 0)
    {
        return Ok(());
    }
    let (width, height) = output.dimensions();
    let rgb = ImageBuffer::from_fn(width, height, |x, y| {
        let pixel = output.get_pixel(x, y);
        image::Rgb([pixel[0], pixel[1], pixel[2]])
    });
    let validity = ImageBuffer::from_fn(width, height, |x, y| {
        Luma([u8::from(
            reduced.get_pixel(x, y)[3] > f32::EPSILON && output.get_pixel(x, y)[3] > 0,
        )])
    });
    let mut filled = Input {
        rgb,
        alpha: Some(validity),
    };
    filled.fill_transparent()?;
    for ((out, rgb), alpha) in output
        .pixels_mut()
        .zip(filled.rgb.pixels())
        .zip(target_alpha.pixels())
    {
        let alpha = sample(alpha[0]);
        if out[3] == 0 && alpha > 0 {
            out.0 = [rgb[0], rgb[1], rgb[2], alpha];
        }
    }
    Ok(())
}

fn sample(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

pub fn encode_png(writer: impl Write, output: &Output) -> Result<()> {
    let (dimensions, color, bytes) = match output {
        Output::Rgb(image) => (image.dimensions(), png::ColorType::Rgb, image.as_raw()),
        Output::Rgba(image) => (image.dimensions(), png::ColorType::Rgba, image.as_raw()),
    };
    write_png(writer, dimensions, color, bytes)
}

pub fn encode_rgb_png(writer: impl Write, image: &RgbImage) -> Result<()> {
    write_png(
        writer,
        image.dimensions(),
        png::ColorType::Rgb,
        image.as_raw(),
    )
}

fn write_png(
    writer: impl Write,
    (width, height): (u32, u32),
    color: png::ColorType,
    bytes: &[u8],
) -> Result<()> {
    let mut encoder = png::Encoder::new(writer, width, height);
    encoder.set_color(color);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(bytes)?;
    writer.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transparent_color_is_filled_and_rgba_is_rebuilt_without_chunks() {
        let visible = image::Rgb([203, 91, 42]);
        for scale in 2..=4 {
            let mut input =
                Input {
                    rgb: ImageBuffer::from_fn(6, 1, |x, _| {
                        if x < 2 { visible } else { image::Rgb([255; 3]) }
                    }),
                    alpha: Some(GrayImage::from_raw(6, 1, vec![255, 128, 0, 0, 0, 0]).unwrap()),
                };
            input.fill_transparent().unwrap();
            assert!(input.rgb.pixels().all(|&pixel| pixel == visible));
            let output = input
                .finish(RgbImage::from_pixel(24, 4, visible), scale)
                .unwrap();
            let Output::Rgba(rgba) = &output else {
                panic!("expected RGBA")
            };
            assert_eq!(rgba.dimensions(), (6 * scale, scale));
            assert!(rgba.pixels().any(|pixel| pixel[3] == 0));
            assert!(rgba.pixels().any(|pixel| (1..255).contains(&pixel[3])));
            for pixel in rgba.pixels() {
                if pixel[3] == 0 {
                    assert_eq!(pixel.0, [0; 4]);
                } else {
                    assert_eq!(pixel.0[..3], visible.0);
                }
            }
            let mut bytes = Vec::new();
            encode_png(&mut bytes, &output).unwrap();
            let mut chunks = &bytes[8..];
            while !chunks.is_empty() {
                let length = u32::from_be_bytes(chunks[..4].try_into().unwrap()) as usize;
                assert!(matches!(&chunks[4..8], b"IHDR" | b"IDAT" | b"IEND"));
                chunks = &chunks[length + 12..];
            }
        }
    }
}
