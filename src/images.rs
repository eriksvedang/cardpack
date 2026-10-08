//! Reading images from disk and turning them into PDF image XObjects.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use flate2::Compression;
use flate2::write::ZlibEncoder;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use lopdf::{Document, Object, ObjectId, Stream, dictionary};

pub const EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "gif", "bmp", "tif", "tiff", "webp"];

pub struct ImageInfo {
    pub path: PathBuf,
    /// Pixel size after applying EXIF orientation.
    pub width: u32,
    pub height: u32,
    format: Option<ImageFormat>,
    orientation: Orientation,
}

/// Reads an image's header only (size, format, EXIF orientation).
pub fn probe(path: &Path) -> Result<ImageInfo> {
    let reader = ImageReader::open(path)?.with_guessed_format()?;
    let format = reader.format();
    let mut decoder = reader.into_decoder()?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let (w, h) = decoder.dimensions();
    let swapped = matches!(
        orientation,
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    );
    let (width, height) = if swapped { (h, w) } else { (w, h) };
    Ok(ImageInfo { path: path.to_owned(), width, height, format, orientation })
}

/// Adds `info` to `doc` as an image XObject and returns its id.
pub fn embed(doc: &mut Document, info: &ImageInfo) -> Result<ObjectId> {
    if info.format == Some(ImageFormat::Jpeg) && info.orientation == Orientation::NoTransforms {
        let bytes = fs::read(&info.path)?;
        if let Some((8, components)) = jpeg_header(&bytes) {
            let color_space = match components {
                1 => Some("DeviceGray"),
                3 => Some("DeviceRGB"),
                _ => None, // CMYK/YCCK JPEGs are often inverted; re-encode those.
            };
            if let Some(cs) = color_space {
                // Embed the JPEG data verbatim: no quality loss, small file.
                let dict = dictionary! {
                    "Type" => "XObject",
                    "Subtype" => "Image",
                    "Width" => info.width as i64,
                    "Height" => info.height as i64,
                    "ColorSpace" => cs,
                    "BitsPerComponent" => 8,
                    "Filter" => "DCTDecode",
                };
                return Ok(doc.add_object(Stream::new(dict, bytes).with_compression(false)));
            }
        }
    }

    let reader = ImageReader::open(&info.path)?.with_guessed_format()?;
    let mut img = DynamicImage::from_decoder(reader.into_decoder()?)
        .with_context(|| format!("decoding {}", info.path.display()))?;
    img.apply_orientation(info.orientation);

    let color = img.color();
    let (width, height) = (img.width() as i64, img.height() as i64);

    let smask = if color.has_alpha() {
        let alpha: Vec<u8> = img.to_luma_alpha8().pixels().map(|p| p.0[1]).collect();
        // Skip the mask entirely if the image is fully opaque.
        if alpha.iter().all(|&a| a == 255) {
            None
        } else {
            Some(doc.add_object(flate_image(width, height, "DeviceGray", &alpha)?))
        }
    } else {
        None
    };

    let (cs, pixels) = if color.has_color() {
        ("DeviceRGB", img.to_rgb8().into_raw())
    } else {
        ("DeviceGray", img.to_luma8().into_raw())
    };
    let mut stream = flate_image(width, height, cs, &pixels)?;
    if let Some(id) = smask {
        stream.dict.set("SMask", Object::Reference(id));
    }
    Ok(doc.add_object(stream))
}

fn flate_image(width: i64, height: i64, color_space: &str, pixels: &[u8]) -> Result<Stream> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(pixels)?;
    let dict = dictionary! {
        "Type" => "XObject",
        "Subtype" => "Image",
        "Width" => width,
        "Height" => height,
        "ColorSpace" => color_space,
        "BitsPerComponent" => 8,
        "Filter" => "FlateDecode",
    };
    Ok(Stream::new(dict, enc.finish()?).with_compression(false))
}

/// Returns (bits per component, number of components) from a JPEG's SOF marker.
fn jpeg_header(data: &[u8]) -> Option<(u8, u8)> {
    if data.get(..2)? != [0xFF, 0xD8] {
        return None;
    }
    let mut i = 2;
    loop {
        while *data.get(i)? != 0xFF {
            i += 1;
        }
        while *data.get(i)? == 0xFF {
            i += 1;
        }
        let marker = *data.get(i)?;
        i += 1;
        if marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            continue; // standalone markers, no length
        }
        let len = u16::from_be_bytes([*data.get(i)?, *data.get(i + 1)?]) as usize;
        let is_sof = (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if is_sof {
            return Some((*data.get(i + 2)?, *data.get(i + 7)?));
        }
        i += len;
    }
}
