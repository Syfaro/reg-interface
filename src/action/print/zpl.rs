use image::{imageops, DynamicImage, ImageBuffer, Luma, LumaA};
use itertools::Itertools;
use tracing::{debug, instrument};

/// Convert an image to a GF instruction.
///
/// This also handles the grayscale conversion, including dithering.
#[instrument(skip(im))]
pub fn image_to_gf(im: &DynamicImage, rotate: bool) -> String {
    // Convert image to grayscale with alpha channel. We need to make sure that
    // transparent pixels are set to white so they aren't printed.
    let im = im.to_luma_alpha8();

    let im = if rotate { imageops::rotate90(&im) } else { im };

    let (width, height) = im.dimensions();
    debug!(width, height, "got image dimensions");

    // ZPL requires that all image widths are padded to a multiple of 8.
    let padding = width % 8;
    let width_padded = width + padding;
    debug!(padding, "calculated needed padding");

    // Create a new image with the padded width, then pull pixels from the
    // original image.
    let mut cleaned_image = ImageBuffer::<Luma<u8>, _>::from_fn(width_padded, height, |x, y| {
        let pixel = im.get_pixel_checked(x, y).unwrap_or(&LumaA([255, 255]));

        // If the pixel is mostly transparent, make it white.
        let val = if pixel.0[1] > 127 { pixel.0[0] } else { 255 };
        Luma([val])
    });
    debug!(width = width_padded, height, "generated image");

    // The image crate provides a bilevel dithering helper to try and make
    // the grayscale to black and white conversion look better.
    imageops::dither(&mut cleaned_image, &imageops::BiLevel);

    // Number of bytes that each line uses.
    let line_size = (width_padded / 8) as usize;

    // The image data, after compression.
    let mut field_data = String::new();

    // Used to keep track of the value from the last line so we can compress it
    // when they are the same.
    let mut last_line = String::new();

    // A buffer for holding bytes for the current line.
    let mut buf = Vec::with_capacity(line_size);
    // A work in progress byte that gets updated with each pixel.
    let mut byte = 0u8;

    for y in 0..height {
        buf.clear();

        for x in 0..width_padded {
            let pos = x % 8;
            let pixel = cleaned_image.get_pixel(x, y).0[0];

            // Need to compress the u8 value into a single bit, so use half of
            // the maximum value as a threshold and shift it into the
            // appropriate bit of our buffer byte.
            if pixel < 128 {
                byte |= 1 << (7 - pos);
            }

            // After processing a complete byte, push it to the buffer. We know
            // this will happen every loop because our image's width was padded
            // to be a multiple of 8.
            if pos == 7 {
                buf.push(byte);
                byte = 0;
            }
        }

        // Now that we're done with the line, we can encode it into a hex
        // representation and compress that value.
        let hex_buf = hex::encode_upper(&buf);
        let compressed_line = zpl_compress_line(&hex_buf);

        // ZPL compression says we can use a colon instead of needing to include
        // the entire line again if they were the same.
        if compressed_line == last_line {
            field_data.push(':');
        } else {
            field_data.push_str(&compressed_line);
            last_line = compressed_line;
        }
    }

    let total_size = line_size * height as usize;
    debug!(
        total_size,
        compressed_size = field_data.len(),
        "got total bytes"
    );

    format!("^GFA,{total_size},{total_size},{line_size},{field_data}^FS")
}

/// Compress an entire line of data using the ZPL ASCII compression system.
fn zpl_compress_line(inp: &str) -> String {
    let mut line = String::new();

    let mut chars = inp.chars().peekable();

    while let Some(c) = chars.next() {
        // Advance the iterator to consume all of the following characters that
        // are the same as the current character.
        let following_count = chars.peeking_take_while(|n| *n == c).count();

        // While compression only makes a difference for more than 1 following
        // character, it makes the code cleaner to always use it.
        if following_count > 0 {
            // If we end on one of these repeating characters, use the special
            // case compression.
            if (c == '0' || c == 'F') && chars.peek().is_none() {
                match c {
                    '0' => line.push(','),
                    'F' => line.push('!'),
                    _ => unreachable!(),
                }
            } else {
                line.push_str(&zpl_repeat_code(c, following_count + 1));
            }
        } else {
            line.push(c);
        }
    }

    line
}

/// Compress a repeating character using the ZPL ASCII compression scheme.
fn zpl_repeat_code(c: char, mut count: usize) -> String {
    let mut s = String::new();

    // Lookup tables for characters. Each character is positioned such that
    // index 0 is 1 instance and is 'g,' index 1 is 2 instances and is 'h,' etc.
    const HIGH_CHAR: &[u8; 20] = b"ghijklmnopqrstuvwxyz";
    const LOW_CHAR: &[u8; 19] = b"GHIJKLMNOPQRSTUVWXY";

    // This scheme has a maximum value, so we just repeat the process if there
    // were somehow that many repeating characters in a row.
    const MAX_REPEAT: usize = 419;
    while count > MAX_REPEAT {
        s.push_str(&zpl_repeat_code(c, MAX_REPEAT));
        count -= MAX_REPEAT;
    }

    // The compression schemed is organized into two sets, the high and low
    // values. These then can be added together to create any value from
    // 1 to 419.

    let high = count / 20;
    let low = count % 20;

    if high > 0 {
        s.push(HIGH_CHAR[high - 1] as char);
    }

    if low > 0 {
        s.push(LOW_CHAR[low - 1] as char);
    }

    format!("{s}{c}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_zpl_repeat_code() {
        use super::zpl_repeat_code;

        let cases = &[
            (('6', 7), "M6"),
            (('B', 40), "hB"),
            (('B', 327), "vMB"),
            (('0', 2), "H0"),
        ];

        for ((c, count), output) in cases {
            assert_eq!(zpl_repeat_code(*c, *count).as_str(), *output);
        }
    }

    #[test]
    fn test_zpl_compress_line() {
        use super::zpl_compress_line;

        let _ = tracing_subscriber::fmt::try_init();

        let cases = &[
            ("6666666", "M6"),
            ("FFFF80", "JF80"),
            ("FFFFFFFF000000", "NF,"),
            ("08000010", "08J010"),
            ("00000000", ","),
            ("80000000", "8,"),
            ("00800000", "H08,"),
            ("00C00300", "H0CH03,"),
            ("00E00700", "H0EH07,"),
            ("00F00F00", "H0FH0F,"),
            ("00FFFF00", "H0JF,"),
            ("00000001", "M01"),
            ("003FFC00", "H03HFC,"),
            ("002021F00F00000C8C31800000", "H02021FH0FK0C8C318,"),
        ];

        for (input, output) in cases {
            assert_eq!(zpl_compress_line(input).as_str(), *output);
        }
    }
}
