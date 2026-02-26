use screencap::capture::ScreenCapturer;
use screencap::codec::{FfmpegDecoder, FfmpegEncoder};
use screencap::Quality;

fn main() {
    env_logger::init();

    let quality = Quality::HD30;
    println!("Loopback test: capture 5 frames, encode, decode, save last as BMP");

    let mut capturer = ScreenCapturer::primary(quality).expect("Failed to create capturer");
    let mut encoder = FfmpegEncoder::new(quality).expect("Failed to create encoder");
    let mut decoder = FfmpegDecoder::new().expect("Failed to create decoder");

    println!("Encoder: {}", encoder.encoder_name());

    let mut last_decoded = None;
    for i in 0..5 {
        let frame = capturer.capture().expect("Capture failed");
        let encoded = encoder.encode(&frame).expect("Encode failed");
        println!(
            "  frame {}: {} bytes, keyframe={}",
            i,
            encoded.data.len(),
            encoded.is_keyframe
        );
        if let Some(decoded) = decoder.decode(&encoded).expect("Decode failed") {
            println!(
                "    decoded: {}x{}, {} bytes RGBA",
                decoded.width,
                decoded.height,
                decoded.data.len()
            );
            last_decoded = Some(decoded);
        }
    }

    if let Some(decoded) = last_decoded {
        let path = "loopback_output.bmp";
        write_bmp(path, decoded.width, decoded.height, &decoded.data);
        println!("Saved decoded frame to {}", path);
    } else {
        println!("No frames decoded (decoder may still be buffering)");
    }
}

/// Quick and dirty BMP writer for RGBA data (no external deps).
fn write_bmp(path: &str, width: u32, height: u32, rgba: &[u8]) {
    use std::io::Write;
    let row_size = (width * 3 + 3) & !3;
    let pixel_data_size = row_size * height;
    let file_size = 54 + pixel_data_size;
    let mut f = std::fs::File::create(path).expect("create file");
    f.write_all(b"BM").unwrap();
    f.write_all(&file_size.to_le_bytes()).unwrap();
    f.write_all(&[0u8; 4]).unwrap();
    f.write_all(&54u32.to_le_bytes()).unwrap();
    f.write_all(&40u32.to_le_bytes()).unwrap();
    f.write_all(&width.to_le_bytes()).unwrap();
    f.write_all(&height.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap();
    f.write_all(&24u16.to_le_bytes()).unwrap();
    f.write_all(&[0u8; 24]).unwrap();
    let mut row_buf = vec![0u8; row_size as usize];
    for y in (0..height).rev() {
        for x in 0..width {
            let src = ((y * width + x) * 4) as usize;
            let dst = (x * 3) as usize;
            row_buf[dst] = rgba[src + 2];
            row_buf[dst + 1] = rgba[src + 1];
            row_buf[dst + 2] = rgba[src];
        }
        f.write_all(&row_buf).unwrap();
    }
}
