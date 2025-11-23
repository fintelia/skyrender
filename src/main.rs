use std::{
    fs::File,
    io::{Cursor, Read, Write},
    time::Duration,
};

use clap::Parser;
use image::{
    codecs::png::{CompressionType, FilterType, PngEncoder},
    GenericImage, ImageEncoder,
};
use rayon::prelude::*;

#[derive(Parser, Debug)]
struct Args {
    /// Resolution of each cubemap face.
    #[clap(short, long, default_value = "1024")]
    resolution: usize,

    /// Cutoff magnitude to include stars in the cubemap.
    #[clap(short, long)]
    min_magnitude: Option<f32>,

    /// Exposure value to use for the non-HDR output images.
    #[clap(short, long, default_value = "-7.0")]
    exposure_value: f32,

    /// Zstandard compression level to use for the output images. Higher values result in smaller
    /// files, but take longer to compress. The current maxixum value is 22.
    #[clap(short, long, default_value = "22")]
    compression_level: i32,
}

fn parse_f32(bytes: &[u8]) -> Option<f32> {
    String::from_utf8_lossy(bytes).parse::<f32>().ok()
}

fn main() {
    let args = Args::parse();
    let size = args.resolution;

    let directory = dirs::cache_dir().unwrap().join("skyrender");
    std::fs::create_dir_all(&directory).unwrap();

    let file_list = include_str!("../_MD5SUM.txt")
        .lines()
        .map(|line| {
            let mut parts = line.split_whitespace();
            let hash = parts.next().unwrap();
            let filename = parts.next().unwrap();
            (hash, filename)
        })
        .collect::<Vec<_>>();

    file_list.par_iter().for_each(|(_hash, filename)| {
        let output_filename = directory.join(format!("{}.bin", filename));
        if output_filename.exists() {
            return;
        }

        let url = format!("https://cdn.gea.esac.esa.int/Gaia/gdr3/gaia_source/{filename}");
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(3600))
            .build()
            .unwrap();
        let data = client
            .get(&url)
            .send()
            .expect("unable to download file")
            .bytes()
            .expect("unable to read file");
        let uncompressed = flate2::read::GzDecoder::new(Cursor::new(data))
            .bytes()
            .map(|b| b.expect("unable to decompress data"))
            .collect::<Vec<_>>();

        let mut packed: Vec<f32> = Vec::new();

        let mut first_line = true;
        for line in uncompressed.split(|b| *b == b'\n') {
            if line.is_empty() || line[0] == b'#' {
                continue;
            } else if first_line {
                first_line = false;
                continue;
            }

            let parts: Vec<_> = line.split(|b| *b == b',').collect();
            let Some(ra) = parse_f32(parts[5]) else {
                continue;
            };
            let Some(dec) = parse_f32(parts[7]) else {
                continue;
            };
            let Some(mag) = parse_f32(parts[69]) else {
                continue;
            };
            let temp = parse_f32(parts[130]).unwrap_or(0.0);
            packed.extend_from_slice(&[ra, dec, mag, temp]);
        }

        atomicwrites::AtomicFile::new(output_filename, atomicwrites::AllowOverwrite)
            .write(|f| f.write_all(&bytemuck::cast_slice(&packed)))
            .expect("unable to write file");
    });

    let colors = (0..400)
        .map(|i| blackbody::temperature_to_rgb(1000.0 + (i as f32 * 100.0)))
        .collect::<Vec<_>>();

    let mut cubemap = vec![0.0f32; size * size * 6 * 3];
    let mut bright_stars = Vec::new();
    for (_hash, filename) in file_list {
        let filename = directory.join(format!("{}.bin", filename));
        let packed = std::fs::read(filename).expect("unable to read file");

        let mut decoded = vec![0.0f32; packed.len() / 4];
        decoded.copy_from_slice(&bytemuck::cast_slice(&packed));

        for chunk in decoded.chunks(4) {
            let ra = chunk[0].to_radians();
            let dec = chunk[1].to_radians();
            let mag = chunk[2];
            let temp = chunk[3];

            let color = if temp == 0.0 {
                [1.0, 1.0, 1.0]
            } else {
                colors[((temp - 1000.0).max(0.0).round() as usize / 100).min(399)]
            };

            if mag < args.min_magnitude.unwrap_or(-10.0) {
                bright_stars.extend_from_slice(&ra.to_le_bytes());
                bright_stars.extend_from_slice(&dec.to_le_bytes());
                bright_stars.extend_from_slice(&mag.to_le_bytes());
                bright_stars.push((color[0] * 255.0).round() as u8);
                bright_stars.push((color[1] * 255.0).round() as u8);
                bright_stars.push((color[2] * 255.0).round() as u8);
                bright_stars.push(0);
                continue;
            }

            let x = -ra.sin() * dec.cos();
            let y = ra.cos() * dec.cos();
            let z = dec.sin();
            let ax = x.abs();
            let ay = y.abs();
            let az = z.abs();
            let (face, u, v) = if x >= ay.max(az) {
                (0, z, y)
            } else if -x >= ay.max(az) {
                (1, -z, y)
            } else if y >= ax.max(az) {
                (3, x, z)
            } else if -y >= ax.max(az) {
                (2, x, -z)
            } else if z >= ax.max(ay) {
                (5, -x, y)
            } else {
                (4, x, y)
            };
            let u = (((u / ax.max(ay).max(az) * 0.5 + 0.5) * size as f32) as usize).min(size - 1);
            let v = (((v / ax.max(ay).max(az) * 0.5 + 0.5) * size as f32) as usize).min(size - 1);

            let index = (face * size * size) + (v * size) + u;
            let irradiance = f32::powf(10.0, 0.4 * (-mag - 14.18));
            cubemap[index * 3] += irradiance * color[0];
            cubemap[index * 3 + 1] += irradiance * color[1];
            cubemap[index * 3 + 2] += irradiance * color[2];
        }
    }

    let element_area = |x, y| f64::atan2(x * y, f64::sqrt(x * x + y * y + 1.0));
    let inv_size = 1.0 / size as f64;
    for face in 0..6 {
        for y in 0..size {
            for x in 0..size {
                // See https://www.rorydriscoll.com/2012/01/15/cubemap-texel-solid-angle
                let u = 2.0 * (x as f64 + 0.5) / size as f64 - 1.0;
                let v = 2.0 * (y as f64 + 0.5) / size as f64 - 1.0;
                let x0 = u - inv_size;
                let y0 = v - inv_size;
                let x1 = u + inv_size;
                let y1 = v + inv_size;
                let solid_angle =
                    element_area(x0, y0) - element_area(x0, y1) - element_area(x1, y0)
                        + element_area(x1, y1);
                let inv_area = 1.0 / solid_angle as f32;

                let index = (face * size * size) + (y * size) + x;
                cubemap[index * 3] *= inv_area;
                cubemap[index * 3 + 1] *= inv_area;
                cubemap[index * 3 + 2] *= inv_area;
            }
        }
    }

    let scale = 255.0 * f32::exp2(3.0 - args.exposure_value);
    let mut img = image::ImageBuffer::from_fn(size as u32, size as u32 * 6, |x, y| {
        let index = ((y as usize * size) + x as usize) * 3;
        image::Rgba([
            (cubemap[index] * scale).min(255.0) as u8,
            (cubemap[index + 1] * scale).min(255.0) as u8,
            (cubemap[index + 2] * scale).min(255.0) as u8,
            255,
        ])
    });
    PngEncoder::new_with_quality(
        File::create(format!("cubemap-{size:04}x{size:04}.png")).unwrap(),
        CompressionType::Best,
        FilterType::Adaptive,
    )
    .write_image(
        &*img,
        size as u32,
        size as u32 * 6,
        image::ExtendedColorType::Rgba8,
    )
    .unwrap();

    let mut img2 = image::ImageBuffer::new(4 * size as u32, 3 * size as u32);
    for (i, (x, y)) in [(2, 1), (0, 1), (1, 0), (1, 2), (1, 1), (3, 1)]
        .into_iter()
        .enumerate()
    {
        image::imageops::overlay(
            &mut img2,
            &*img.sub_image(0, (i * size) as u32, size as u32, size as u32),
            size as i64 * x,
            size as i64 * y,
        );
    }
    PngEncoder::new_with_quality(
        File::create(format!("net-{size:04}x{size:04}.png")).unwrap(),
        CompressionType::Best,
        FilterType::Adaptive,
    )
    .write_image(
        &*img2,
        4 * size as u32,
        3 * size as u32,
        image::ExtendedColorType::Rgba8,
    )
    .unwrap();

    let mut hdr_pixels = Vec::new();
    for v in cubemap.chunks(3) {
        hdr_pixels.extend_from_slice(&envmap_tools::rgb9e5::float3_to_rgb9e5(v).to_le_bytes());
    }
    std::fs::write(
        format!("hdr-cubemap-{size:04}x{size:04}.ktx2"),
        ktx2encode::encode_ktx2(
            &[hdr_pixels],
            size as u32,
            size as u32,
            0,
            0,
            true,
            ktx2encode::Format::E5B9G9R9_UFLOAT_PACK32,
            args.compression_level,
        ),
    )
    .unwrap();

    if !bright_stars.is_empty() {
        std::fs::write("bright-stars.bin", bright_stars).unwrap();
    }
}
