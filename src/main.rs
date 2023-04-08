use std::{
    io::{Cursor, Read, Write},
    path::PathBuf,
    time::Duration,
};

use rayon::prelude::*;

fn parse_f32(bytes: &[u8]) -> Option<f32> {
    String::from_utf8_lossy(bytes).parse::<f32>().ok()
}

fn main() {
    let directory = PathBuf::from("/media/data/astronomy");
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
            .timeout(Duration::from_secs(300))
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
            let Some(ra) = parse_f32(parts[5]) else { continue };
            let Some(dec) = parse_f32(parts[7]) else { continue };
            let Some(mag) = parse_f32(parts[69]) else { continue };
            let Some(temp) = parse_f32(parts[130]) else { continue };
            packed.extend_from_slice(&[ra, dec, mag, temp]);
        }

        atomicwrites::AtomicFile::new(output_filename, atomicwrites::AllowOverwrite)
            .write(|f| f.write_all(&bytemuck::cast_slice(&packed)))
            .expect("unable to write file");
    });

    let colors = (0..400)
        .map(|i| blackbody::temperature_to_rgb(1000.0 + (i as f32 * 100.0)))
        .collect::<Vec<_>>();

    const MIN_MAGNITUDE: f32 = 9.0;
    const SIZE: usize = 1024;
    let mut cubemap = vec![0.0f32; SIZE * SIZE * 6 * 3];
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

            if ra == 0.0 && dec == 0.0 || mag < MIN_MAGNITUDE {
                continue;
            }

            let color = colors[((temp - 1000.0).max(0.0).round() as usize / 100).min(399)];

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
                (4, -x, y)
            } else {
                (5, x, y)
            };
            let u = (((u / ax.max(ay).max(az) * 0.5 + 0.5) * SIZE as f32) as usize).min(SIZE - 1);
            let v = (((v / ax.max(ay).max(az) * 0.5 + 0.5) * SIZE as f32) as usize).min(SIZE - 1);
            let index = (face * SIZE * SIZE) + (v * SIZE) + u;
            let irradiance = f32::powf(10.0, 0.4 * (-mag - 14.18));
            cubemap[index * 3] += irradiance * color[0];
            cubemap[index * 3 + 1] += irradiance * color[1];
            cubemap[index * 3 + 2] += irradiance * color[2];
        }
    }

    let element_area = |x, y| f32::atan2(x * y, f32::sqrt(x * x + y * y + 1.0));
    let inv_size = 1.0 / SIZE as f32;

    for face in 0..6 {
        for y in 0..SIZE {
            for x in 0..SIZE {
                // See https://www.rorydriscoll.com/2012/01/15/cubemap-texel-solid-angle
                let u = 2.0 * (x as f32 + 0.5) / SIZE as f32 - 1.0;
                let v = 2.0 * (y as f32 + 0.5) / SIZE as f32 - 1.0;
                let x0 = u - inv_size;
                let y0 = v - inv_size;
                let x1 = u + inv_size;
                let y1 = v + inv_size;
                let solid_angle =
                    element_area(x0, y0) - element_area(x0, y1) - element_area(x1, y0)
                        + element_area(x1, y1);
                let inv_area = 1.0 / solid_angle;

                let index = (face * SIZE * SIZE) + (y * SIZE) + x;
                cubemap[index * 3] *= inv_area;
                cubemap[index * 3 + 1] *= inv_area;
                cubemap[index * 3 + 2] *= inv_area;
            }
        }
    }

    let scale = 255.0 * 1000.0;
    image::ImageBuffer::from_fn(SIZE as u32, SIZE as u32 * 6, |x, y| {
        let index = ((y as usize * SIZE) + x as usize) * 3;
        image::Rgba([
            (cubemap[index] * scale).min(255.0) as u8,
            (cubemap[index + 1] * scale).min(255.0) as u8,
            (cubemap[index + 2] * scale).min(255.0) as u8,
            255,
        ])
    })
    .save("output.png")
    .unwrap();
}
