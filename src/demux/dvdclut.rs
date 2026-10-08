// SPDX-License-Identifier: LGPL-2.1-or-later
// Port of FFmpeg 2da55bf libavformat/dvdclut.c (ff_dvdclut_yuv_to_rgb,
// ff_dvdclut_palette_extradata_cat) with the YUV_TO_RGB1_CCIR macro of
// libavutil/colorspace.h, as mov_read_header applies them.
// Copyright (c) the FFmpeg developers (dvdclut.c);
// Copyright (c) 2001, 2002, 2003 Fabrice Bellard (colorspace.h).
//
// This file is free software; you can redistribute it and/or modify it under
// the terms of the GNU Lesser General Public License as published by the Free
// Software Foundation; either version 2.1, or (at your option) any later version.
// It is distributed WITHOUT ANY WARRANTY; without even the implied warranty
// of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See LICENSE-LGPL.

//! A DVD subtitle's colour table, as an MP4 `mp4s` entry carries it (16
//! big-endian `0x00YYCrCb` words in the esds DecoderSpecificInfo), turned
//! into the `palette:` text DVD subtitle decoders read, exactly as FFmpeg's
//! MP4 demuxer does.

/// The colour table's size in bytes.
pub(crate) const CLUT_SIZE: usize = 64;

const SCALEBITS: u32 = 10;
const ONE_HALF: i32 = 1 << (SCALEBITS - 1);

/// `FIX(x)`: `x` in 10-bit fixed point, rounded as C's `(int)(x * 1024 + 0.5)`.
fn fix(x: f64) -> i32 {
    (x * f64::from(1 << SCALEBITS) + 0.5) as i32
}

/// `ff_dvdclut_yuv_to_rgb` for one `0x00YYCrCb` entry, as `0x00RRGGBB`.
fn yuv_to_rgb(entry: u32) -> u32 {
    let y = ((entry >> 16) & 0xFF) as i32;
    let cr = ((entry >> 8) & 0xFF) as i32 - 128;
    let cb = (entry & 0xFF) as i32 - 128;
    // YUV_TO_RGB1_CCIR
    let r_add = fix(1.40200 * 255.0 / 224.0) * cr + ONE_HALF;
    let g_add = -fix(0.34414 * 255.0 / 224.0) * cb - fix(0.71414 * 255.0 / 224.0) * cr + ONE_HALF;
    let b_add = fix(1.77200 * 255.0 / 224.0) * cb + ONE_HALF;
    let y = (y - 16) * fix(255.0 / 219.0);
    let channel = |add: i32| ((y + add - 1024) >> SCALEBITS).clamp(0, 255) as u32;
    channel(r_add) << 16 | channel(g_add) << 8 | channel(b_add)
}

/// The extradata FFmpeg's DVD subtitle decoder reads for a `CLUT_SIZE`
/// byte colour table: `palette: rrggbb, …` and a newline.
pub(crate) fn palette_extradata(clut: &[u8]) -> Vec<u8> {
    let entries: Vec<String> = clut
        .chunks_exact(4)
        .map(|word| format!("{:06x}", yuv_to_rgb(u32::from_be_bytes([word[0], word[1], word[2], word[3]]))))
        .collect();
    format!("palette: {}\n", entries.join(", ")).into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_convert_as_ffmpeg_converts_them() {
        // Black, white and a saturated colour; FFmpeg's `-1024` darkens
        // every channel by one step.
        assert_eq!(yuv_to_rgb(0x0010_8080), 0x000000);
        assert_eq!(yuv_to_rgb(0x00EB_8080), 0xFEFEFE);
        let table: Vec<u8> = (0..16u32).flat_map(|i| (0x0010_8080 + (i << 16)).to_be_bytes()).collect();
        let text = String::from_utf8(palette_extradata(&table)).unwrap();
        assert!(text.starts_with("palette: 000000, 000000, 010101, "), "{text}");
        assert!(text.ends_with('\n') && text.matches(", ").count() == 15, "{text}");
    }
}
