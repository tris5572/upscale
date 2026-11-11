use std::fmt;
use std::fs;
use std::io::Cursor;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use image::metadata::Orientation;
use image::{DynamicImage, GrayImage, RgbImage};
use moxcms::{
    Chromaticity, ColorPrimaries, ColorProfile, DataColorSpace, Layout, ToneReprCurve,
    TransferCharacteristics, XyY,
};
use png::{BitDepth, ColorType};
use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

use crate::pixels::Input;
use crate::storage::Format;

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

#[derive(Debug)]
pub struct Unsupported(&'static str);

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Unsupported {}

struct Raster {
    width: u32,
    height: u32,
    color: Vec<u8>,
    alpha: Option<Vec<u8>>,
    gray: bool,
}

pub fn input(path: &Path) -> Result<Input> {
    let bytes = fs::read(path)?;
    let (raster, profile, orientation) = match Format::from_extension(path) {
        Some(Format::Png) if bytes.starts_with(PNG_SIGNATURE) => read_png(&bytes, None)?,
        Some(Format::Jpeg) if bytes.starts_with(b"\xff\xd8") => read_jpeg(&bytes)?,
        _ => {
            return Err(Unsupported("file contents do not match the PNG or JPEG extension").into());
        }
    };
    raster.normalize(profile, orientation)
}

pub fn inferred(path: &Path, dimensions: (u32, u32)) -> Result<RgbImage> {
    let bytes = fs::read(path).context("engine did not produce an output PNG")?;
    let (raster, _, _) = read_png(&bytes, Some(dimensions))?;
    ensure!(
        !raster.gray && raster.alpha.is_none(),
        "engine output is not RGB"
    );
    RgbImage::from_raw(raster.width, raster.height, raster.color)
        .context("invalid engine output buffer")
}

fn input_size(width: u32, height: u32) -> Result<()> {
    // The native engine indexes its 4x RGB output with signed integers.
    ensure!(
        u64::from(width) * u64::from(height) <= i32::MAX as u64 / (4 * 4 * 3),
        Unsupported("image dimensions exceed the inference engine's limits")
    );
    Ok(())
}

fn read_png(
    bytes: &[u8],
    expected: Option<(u32, u32)>,
) -> Result<(Raster, Option<ColorProfile>, Orientation)> {
    let has_icc = inspect_png(bytes)?;
    let mut options = png::DecodeOptions::default();
    options.set_skip_ancillary_crc_failures(false);
    let mut decoder = png::Decoder::new_with_options(Cursor::new(bytes), options);
    decoder.set_ignore_text_chunk(true);
    decoder.set_transformations(png::Transformations::EXPAND);
    let info = decoder.read_header_info()?;
    if info.bit_depth == BitDepth::Sixteen {
        return Err(Unsupported("16-bit PNG is unsupported").into());
    }
    if let Some((width, height)) = expected {
        ensure!(
            info.size() == (width, height),
            "engine output dimensions are {}×{}, expected {width}×{height}",
            info.width,
            info.height
        );
        ensure!(
            info.color_type == ColorType::Rgb,
            "engine output is not RGB"
        );
    } else {
        input_size(info.width, info.height)?;
    }
    let mut reader = decoder.read_info()?;
    let mut pixels = vec![
        0;
        reader
            .output_buffer_size()
            .context("PNG image is too large")?
    ];
    let frame = reader.next_frame(&mut pixels)?;
    reader.finish()?;
    pixels.truncate(frame.buffer_size());

    let info = reader.info();
    let gray = matches!(
        frame.color_type,
        ColorType::Grayscale | ColorType::GrayscaleAlpha
    );
    let (profile, orientation) = if expected.is_none() {
        let profile = png_profile(info, has_icc)?;
        let (orientation, exif_profile) =
            exif_fields(info.exif_metadata.as_deref(), profile.is_none())?;
        (profile.or(exif_profile), orientation)
    } else {
        (None, Orientation::NoTransforms)
    };
    let alpha = matches!(
        frame.color_type,
        ColorType::GrayscaleAlpha | ColorType::Rgba
    );
    let (color, alpha) = if alpha {
        let channels = frame.color_type.samples();
        let mut color = Vec::with_capacity(pixels.len() / channels * (channels - 1));
        let mut alpha = Vec::with_capacity(pixels.len() / channels);
        for pixel in pixels.chunks_exact(channels) {
            color.extend_from_slice(&pixel[..channels - 1]);
            alpha.push(pixel[channels - 1]);
        }
        (color, Some(alpha))
    } else {
        (pixels, None)
    };
    Ok((
        Raster {
            width: frame.width,
            height: frame.height,
            color,
            alpha,
            gray,
        },
        profile,
        orientation,
    ))
}

fn inspect_png(bytes: &[u8]) -> Result<bool> {
    let mut chunks = bytes
        .strip_prefix(PNG_SIGNATURE)
        .context("invalid PNG signature")?;
    let mut has_icc = false;
    loop {
        let header = chunks.get(..8).context("truncated PNG chunk header")?;
        let length = u32::from_be_bytes(header[..4].try_into()?) as usize;
        chunks = chunks.get(length + 12..).context("truncated PNG chunk")?;
        match &header[4..] {
            b"IEND" => return Ok(has_icc),
            b"acTL" | b"fcTL" | b"fdAT" => {
                return Err(Unsupported("animated PNG is unsupported").into());
            }
            b"mDCV" | b"cLLI" => return Err(Unsupported("HDR PNG is unsupported").into()),
            b"iCCP" => {
                ensure!(!has_icc, "PNG contains duplicate ICC profiles");
                has_icc = true;
            }
            _ => {}
        }
    }
}

fn png_profile(info: &png::Info<'_>, has_icc: bool) -> Result<Option<ColorProfile>> {
    let icc = if has_icc {
        Some(embedded_profile(
            info.icc_profile
                .as_deref()
                .context("corrupt PNG ICC profile")?,
        )?)
    } else {
        None
    };
    if let Some(cicp) = info.coding_independent_code_points {
        ensure!(
            cicp.matrix_coefficients == 0 && cicp.is_video_full_range_image,
            Unsupported("unsupported PNG cICP matrix or range")
        );
        return match (cicp.color_primaries, cicp.transfer_function) {
            (1, 13) => Ok(Some(ColorProfile::new_srgb())),
            (12, 13) => Ok(Some(ColorProfile::new_display_p3())),
            (_, 16 | 18) => Err(Unsupported("HDR PNG is unsupported").into()),
            _ => Err(Unsupported("unsupported PNG cICP color space").into()),
        };
    }
    if icc.is_some() {
        return Ok(icc);
    }
    if info.srgb.is_some() {
        return Ok(Some(ColorProfile::new_srgb()));
    }
    if info.gama_chunk.is_none() && info.chrm_chunk.is_none() {
        return Ok(None);
    }

    let mut profile = ColorProfile::new_srgb();
    profile.cicp = None;
    if let Some(chroma) = info.chrm_chunk {
        let point = |(x, y): (png::ScaledFloat, png::ScaledFloat)| -> Result<Chromaticity> {
            let (x, y) = (x.into_value(), y.into_value());
            ensure!(y > 0.0 && x + y <= 1.0, "invalid PNG chromaticities");
            Ok(Chromaticity { x, y })
        };
        let white = point(chroma.white)?;
        let white = XyY {
            x: white.x.into(),
            y: white.y.into(),
            yb: 1.0,
        };
        let primaries = ColorPrimaries {
            red: point(chroma.red)?,
            green: point(chroma.green)?,
            blue: point(chroma.blue)?,
        };
        let matrix = ColorProfile::colorants_matrix(white, primaries);
        ensure!(
            matrix.v.iter().flatten().all(|value| value.is_finite())
                && matrix.determinant().is_some(),
            "invalid PNG chromaticities"
        );
        profile.update_rgb_colorimetry(white, primaries);
        profile.media_white_point = Some(white.to_xyzd());
    }
    if let Some(gamma) = info.gama_chunk {
        ensure!(gamma.into_scaled() != 0, "invalid PNG gamma");
        let curve = ToneReprCurve::Parametric(vec![gamma.into_value().recip()]);
        profile.red_trc = Some(curve.clone());
        profile.green_trc = Some(curve.clone());
        profile.blue_trc = Some(curve);
    }
    Ok(Some(profile))
}

fn read_jpeg(bytes: &[u8]) -> Result<(Raster, Option<ColorProfile>, Orientation)> {
    let has_icc = jpeg_has_icc(bytes)?;
    let options = DecoderOptions::default().set_strict_mode(true);
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(bytes), options);
    decoder
        .decode_headers()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let gray = match decoder.input_colorspace() {
        Some(ColorSpace::Luma) => true,
        Some(ColorSpace::YCbCr | ColorSpace::RGB) => false,
        _ => {
            return Err(
                Unsupported("CMYK and other non-RGB JPEG color spaces are unsupported").into(),
            );
        }
    };
    if gray {
        decoder.set_options(options.jpeg_set_out_colorspace(ColorSpace::Luma));
    }
    let info = decoder.info().context("missing JPEG dimensions")?;
    let (width, height) = (u32::from(info.width), u32::from(info.height));
    input_size(width, height)?;
    let icc = if has_icc {
        let bytes = decoder.icc_profile().context("corrupt JPEG ICC segments")?;
        Some(embedded_profile(&bytes)?)
    } else {
        None
    };
    let (orientation, exif_profile) =
        exif_fields(decoder.exif().map(Vec::as_slice), icc.is_none())?;
    let profile = icc.or(exif_profile);
    let color = decoder
        .decode()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok((
        Raster {
            width,
            height,
            color,
            alpha: None,
            gray,
        },
        profile,
        orientation,
    ))
}

fn jpeg_has_icc(bytes: &[u8]) -> Result<bool> {
    let mut data = &bytes[2..];
    let mut icc = false;
    while let Some(mut rest) = data.strip_prefix(b"\xff") {
        while let Some(next) = rest.strip_prefix(b"\xff") {
            rest = next;
        }
        let Some((&marker, rest)) = rest.split_first() else {
            break;
        };
        data = rest;
        match marker {
            0xda => return Ok(icc),
            0x01 => continue,
            _ => {
                let length = data.get(..2).context("truncated JPEG marker")?;
                let length = usize::from(u16::from_be_bytes([length[0], length[1]]));
                let body = data.get(2..length).context("invalid JPEG marker length")?;
                if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf)
                    && let Some(&precision) = body.first()
                {
                    ensure!(precision == 8, Unsupported("only 8-bit JPEG is supported"));
                }
                if marker == 0xe2 {
                    icc |= body.starts_with(b"ICC_PROFILE\0");
                    if body.starts_with(b"MPF\0") {
                        return Err(Unsupported("multi-picture JPEG is unsupported").into());
                    }
                }
                data = &data[length..];
            }
        }
    }
    bail!("missing JPEG scan")
}

fn embedded_profile(bytes: &[u8]) -> Result<ColorProfile> {
    ensure!(
        bytes.len() >= 132
            && bytes.get(36..40) == Some(b"acsp")
            && u32::from_be_bytes(bytes[..4].try_into()?) as usize == bytes.len(),
        "corrupt ICC profile header"
    );
    let profile = ColorProfile::new_from_slice(bytes).context("invalid embedded ICC profile")?;
    if let Some(cicp) = profile.cicp {
        ensure!(
            !matches!(
                cicp.transfer_characteristics,
                TransferCharacteristics::Smpte428
                    | TransferCharacteristics::Smpte2084
                    | TransferCharacteristics::Hlg
            ),
            Unsupported("HDR ICC profiles are unsupported")
        );
    }
    Ok(profile)
}

fn exif_fields(
    bytes: Option<&[u8]>,
    use_color: bool,
) -> Result<(Orientation, Option<ColorProfile>)> {
    let Some(bytes) = bytes else {
        return Ok((Orientation::NoTransforms, None));
    };
    let exif = exif::Reader::new()
        .read_raw(bytes.to_vec())
        .context("invalid EXIF data")?;
    let value = |tag| {
        exif.get_field(tag, exif::In::PRIMARY)
            .and_then(|field| field.value.get_uint(0))
    };
    let orientation = value(exif::Tag::Orientation)
        .and_then(|value| u8::try_from(value).ok())
        .and_then(Orientation::from_exif)
        .unwrap_or(Orientation::NoTransforms);
    if !use_color {
        return Ok((orientation, None));
    }
    let color = match value(exif::Tag::ColorSpace) {
        Some(1) => None,
        Some(2) => Some(ColorProfile::new_adobe_rgb()),
        None | Some(65535) => {
            match exif.get_field(exif::Tag::InteroperabilityIndex, exif::In::PRIMARY) {
                Some(field) if matches!(&field.value, exif::Value::Ascii(values) if values.first().is_some_and(|value| value == b"R03")) => {
                    Some(ColorProfile::new_adobe_rgb())
                }
                _ => None,
            }
        }
        Some(_) => return Err(Unsupported("unsupported EXIF color space").into()),
    };
    Ok((orientation, color))
}

impl Raster {
    fn normalize(self, profile: Option<ColorProfile>, orientation: Orientation) -> Result<Input> {
        let Self {
            width,
            height,
            mut color,
            alpha,
            gray,
        } = self;
        let use_gray_profile = gray
            && profile
                .as_ref()
                .is_some_and(|profile| profile.color_space == DataColorSpace::Gray);
        if gray && !use_gray_profile {
            color = color.into_iter().flat_map(|gray| [gray; 3]).collect();
        }
        if let Some(profile) = profile {
            let (space, layout) = if use_gray_profile {
                (DataColorSpace::Gray, Layout::Gray)
            } else {
                (DataColorSpace::Rgb, Layout::Rgb)
            };
            ensure!(
                profile.color_space == space,
                "ICC profile does not match the image color model"
            );
            let options = moxcms::TransformOptions {
                rendering_intent: moxcms::RenderingIntent::RelativeColorimetric,
                ..Default::default()
            };
            let transform = profile
                .create_transform_8bit(layout, &ColorProfile::new_srgb(), Layout::Rgb, options)
                .context("unsupported input color profile")?;
            let mut rgb = vec![0; width as usize * height as usize * 3];
            transform
                .transform(&color, &mut rgb)
                .context("cannot convert input to sRGB")?;
            color = rgb;
        }

        let rgb = RgbImage::from_raw(width, height, color).context("invalid decoded RGB buffer")?;
        let mut rgb = DynamicImage::ImageRgb8(rgb);
        rgb.apply_orientation(orientation);
        let alpha = alpha
            .map(|alpha| -> Result<_> {
                let alpha = GrayImage::from_raw(width, height, alpha)
                    .context("invalid decoded alpha buffer")?;
                let mut alpha = DynamicImage::ImageLuma8(alpha);
                alpha.apply_orientation(orientation);
                Ok(alpha.into_luma8())
            })
            .transpose()?;
        Ok(Input {
            rgb: rgb.into_rgb8(),
            alpha,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(info: png::Info<'static>, pixels: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut writer = png::Encoder::with_info(&mut bytes, info)
            .unwrap()
            .write_header()
            .unwrap();
        writer.write_image_data(pixels).unwrap();
        writer.finish().unwrap();
        bytes
    }

    fn decode(bytes: &[u8]) -> Result<Input> {
        let (image, profile, orientation) = read_png(bytes, None)?;
        image.normalize(profile, orientation)
    }

    #[test]
    fn all_exif_orientations_apply_to_color_and_alpha() {
        let cases = [
            ((2, 3), [1, 2, 3, 4, 5, 6]),
            ((2, 3), [2, 1, 4, 3, 6, 5]),
            ((2, 3), [6, 5, 4, 3, 2, 1]),
            ((2, 3), [5, 6, 3, 4, 1, 2]),
            ((3, 2), [1, 3, 5, 2, 4, 6]),
            ((3, 2), [5, 3, 1, 6, 4, 2]),
            ((3, 2), [6, 4, 2, 5, 3, 1]),
            ((3, 2), [2, 4, 6, 1, 3, 5]),
        ];
        for (index, (dimensions, expected)) in cases.into_iter().enumerate() {
            let mut info = png::Info::with_size(2, 3);
            info.color_type = ColorType::GrayscaleAlpha;
            info.exif_metadata = Some(
                vec![
                    b'I',
                    b'I',
                    42,
                    0,
                    8,
                    0,
                    0,
                    0,
                    1,
                    0,
                    0x12,
                    1,
                    3,
                    0,
                    1,
                    0,
                    0,
                    0,
                    (index + 1) as u8,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                ]
                .into(),
            );
            let pixels: Vec<_> = (1..=6).flat_map(|value| [value, value + 10]).collect();
            let image = decode(&png(info, &pixels)).unwrap();
            assert_eq!(image.rgb.dimensions(), dimensions);
            let rgb: Vec<_> = expected.into_iter().flat_map(|value| [value; 3]).collect();
            assert_eq!(image.rgb.as_raw(), &rgb);
            assert_eq!(
                image.alpha.unwrap().as_raw(),
                &expected.map(|value| value + 10)
            );
        }
    }

    #[test]
    fn expands_palette_and_low_bit_depth() {
        let mut palette = png::Info::with_size(2, 1);
        palette.color_type = ColorType::Indexed;
        palette.bit_depth = BitDepth::One;
        palette.palette = Some(vec![21, 33, 55, 13, 17, 19].into());
        palette.trns = Some(vec![0, 201].into());
        let image = decode(&png(palette, &[0b0100_0000])).unwrap();
        assert_eq!(image.rgb.as_raw(), &[21, 33, 55, 13, 17, 19]);
        assert_eq!(image.alpha.unwrap().as_raw(), &[0, 201]);

        let mut gray = png::Info::with_size(4, 1);
        gray.bit_depth = BitDepth::Two;
        let image = decode(&png(gray, &[0b0001_1011])).unwrap();
        assert_eq!(
            image.rgb.as_raw(),
            &[0, 0, 0, 85, 85, 85, 170, 170, 170, 255, 255, 255]
        );
    }

    #[test]
    fn converts_icc_and_gamma_and_rejects_corrupt_icc() {
        let mut info = png::Info::with_size(1, 1);
        info.color_type = ColorType::Rgba;
        info.icc_profile = Some(ColorProfile::new_display_p3().encode().unwrap().into());
        let image = decode(&png(info.clone(), &[160, 80, 20, 93])).unwrap();
        let channels = image.rgb.get_pixel(0, 0).0;
        assert!(
            channels[0] > 167 && channels[1] < 78 && channels[2] < 10,
            "{channels:?}"
        );
        assert_eq!(image.alpha.unwrap().as_raw(), &[93]);
        info.icc_profile = Some(vec![1, 2, 3].into());
        assert!(decode(&png(info, &[160, 80, 20, 93])).is_err());

        let mut linear = png::Info::with_size(1, 1);
        linear.gama_chunk = Some(png::ScaledFloat::new(1.0));
        linear.source_gamma = linear.gama_chunk;
        let image = decode(&png(linear, &[128])).unwrap();
        assert_eq!(image.rgb.as_raw(), &[188, 188, 188]);
    }

    #[test]
    fn rejects_animation_and_sixteen_bit() {
        let mut info = png::Info::with_size(1, 1);
        info.bit_depth = BitDepth::Sixteen;
        assert!(
            decode(&png(info, &[0, 12]))
                .err()
                .unwrap()
                .is::<Unsupported>()
        );

        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
        encoder.set_animated(1, 0).unwrap();
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[48]).unwrap();
        writer.finish().unwrap();
        assert!(decode(&bytes).err().unwrap().is::<Unsupported>());
    }
}
