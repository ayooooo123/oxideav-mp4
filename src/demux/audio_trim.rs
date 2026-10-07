// SPDX-License-Identifier: LGPL-2.1-or-later
// Port of FFmpeg 2da55bf libavformat/mov.c (mov_fix_index's edit-list
// priming and end, mov_build_index's fragmented edit, the iTunSMPB counts,
// mov_get_skip_samples) and libavformat/demux.c (the discard window of
// read_frame_internal).
// Copyright (c) 2001 Fabrice Bellard, 2009 Baptiste Coudurier (mov.c);
// 2000-2002 Fabrice Bellard (demux.c)
//
// This file is free software; you can redistribute it and/or modify it under
// the terms of the GNU Lesser General Public License as published by the Free
// Software Foundation; either version 2.1, or (at your option) any later version.
// It is distributed WITHOUT ANY WARRANTY; without even the implied warranty
// of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See LICENSE-LGPL.

//! Encoder delay and end padding of audio tracks, exposed per packet as
//! `PacketMetadata::audio_trim` in the track's media timescale, with the
//! values FFmpeg's mov demuxer (2da55bf `libavformat/mov.c`, with
//! `libavformat/demux.c`) attaches as `AV_PKT_DATA_SKIP_SAMPLES`. The
//! decoder still sees every sample; a consumer removes the samples after
//! decoding.
//!
//! Sample tables (FFmpeg's default `advanced_editlist`, `mov_fix_index`):
//! - The samples before the first non-empty edit's `media_time` are
//!   priming: the track's first packet skips them, whole samples and the
//!   part of the one that straddles the edit start (not for Vorbis).
//! - With one non-empty edit, the samples after the first one that reaches
//!   the edit's end are not packets of the track.
//! - The last packet discards what lies past the track's duration (the least
//!   of `mdhd`, the `stts` total and the edit list's), counting the packet
//!   as at least one codec frame long (`mov_finalize_packet`, demux.c
//!   `read_frame_internal`). Not in files with movie fragments.
//!
//! Fragmented tracks (no sample table, so no advanced edit list): an AAC
//! track's first packet skips the single edit's `media_time`.
//!
//! iTunSMPB (`moov/udta/meta/ilst/----`, applying to the track parsed last
//! before it): the priming replaces an AAC track's skip; the remainder makes
//! every packet that reaches into the last `remainder` samples discard that
//! part.
//!
//! After a seek, an audio track's next packet skips the priming still ahead
//! of it (`mov_get_skip_samples`).
//!
//! Counts are in the media timescale, which the trim declares as its rate,
//! so a consumer rescales them to the rate the decoder outputs and the
//! skip ends exactly where the edit's media time does. FFmpeg 2da55bf
//! instead applies an edit-list skip's timescale ticks as output samples
//! (`mov_fix_index` to `decode.c`), and counts everything else in output
//! samples: with a timescale below the output rate (HE-AAC at its core
//! rate) it leaves priming in the output, stamped before zero. That one
//! difference is deliberate; the two agree whenever the timescale is the
//! sample rate. Edit lists with more than one non-empty edit get only the
//! start skip; an edit whose duration is zero is open-ended rather than
//! empty.

use std::collections::BTreeMap;
use std::ops::Range;

use super::{SampleRef, Track};
use oxideav_core::MediaType;

/// What a track needs for the skip a seek's landing packet carries.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct SeekTrim {
    /// The track's priming, in its timescale (FFmpeg's `initial_padding`).
    pub(super) initial_padding: i64,
    /// The dts of the track's first packet.
    pub(super) first_dts: i64,
}

impl SeekTrim {
    /// The skip of a packet at `dts` after a seek.
    pub(super) fn skip_at(&self, dts: i64) -> u32 {
        let skip = self.initial_padding.saturating_sub(dts.saturating_sub(self.first_dts));
        clamp_u32(skip)
    }
}

fn clamp_u32(v: i64) -> u32 {
    v.clamp(0, i64::from(u32::MAX)) as u32
}

/// `a * b / c` rounded to nearest, halves away from zero (`av_rescale`),
/// saturating; `c` must be positive.
fn rescale(a: i64, b: i64, c: i64) -> i64 {
    let n = i128::from(a) * i128::from(b);
    let c = i128::from(c);
    let r = if n >= 0 { (n + c / 2) / c } else { (n - c / 2) / c };
    r.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

/// The iTunSMPB fields `(priming, remainder, samples)`: four hexadecimal
/// numbers of at most 16 digits separated by spaces, none above 2^40
/// (`ff_itunes_parse_smpb`).
pub(super) fn parse_smpb(value: &str) -> Option<(i64, i64, i64)> {
    let b = value.as_bytes();
    let mut at = 0;
    let mut fields = [0i64; 4];
    for field in &mut fields {
        while b.get(at).is_some_and(|c| c.is_ascii_whitespace()) {
            at += 1;
        }
        let start = at;
        while at - start < 16 && b.get(at).is_some_and(|c| c.is_ascii_hexdigit()) {
            at += 1;
        }
        if at == start || b.get(at).is_some_and(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let v = u64::from_str_radix(std::str::from_utf8(&b[start..at]).ok()?, 16).ok()?;
        if v > 1 << 40 {
            return None;
        }
        *field = v as i64;
    }
    Some((fields[1], fields[2], fields[3]))
}

/// The codec frame length FFmpeg's decoders report for codecs with fixed
/// frames (the track's most common sample duration), 0 for the others.
fn frame_size(t: &Track, codec: &str) -> i64 {
    if !matches!(codec, "aac" | "mp1" | "mp2" | "mp3" | "ac3" | "eac3") {
        return 0;
    }
    let mut counts: BTreeMap<u32, u64> = BTreeMap::new();
    for &(count, delta) in &t.stts {
        *counts.entry(delta).or_default() += u64::from(count);
    }
    counts.into_iter().max_by_key(|&(delta, count)| (count, delta)).map_or(0, |(delta, _)| i64::from(delta))
}

/// The edits FFmpeg's `mov_fix_index` walks, in the media timescale.
struct Edits {
    /// `(media_time, duration)` of the first non-empty edit.
    first: Option<(i64, i64)>,
    /// Edits after the leading empty ones, the first non-empty included.
    count: usize,
    /// The presentation length of the whole list.
    total: i64,
}

fn edits(t: &Track, movie_timescale: u32) -> Edits {
    let mut out = Edits { first: None, count: 0, total: 0 };
    if movie_timescale == 0 || t.timescale == 0 {
        return out;
    }
    for e in &t.elst {
        let raw = i64::try_from(e.segment_duration).unwrap_or(i64::MAX);
        let mut duration = rescale(raw, i64::from(t.timescale), i64::from(movie_timescale));
        if duration.checked_add(e.media_time).is_none() {
            duration = 0;
        }
        out.total = out.total.saturating_add(duration);
        if out.count == 0 && e.media_time == -1 {
            continue;
        }
        out.count += 1;
        if out.first.is_none() {
            out.first = Some((e.media_time, duration));
        }
    }
    out
}

/// The track's iTunSMPB `(priming, remainder, samples)`, when it has one.
pub(super) type Smpb = Option<(i64, i64, i64)>;

/// Trims the sample-table samples `range` of audio track `t` (in decode
/// order, the track's only samples in `samples` there): drops the ones past
/// the edit, stores each remaining sample's skip and padding, and returns
/// what a seek needs. `fragmented`: the file has movie fragments.
pub(super) fn sample_table(
    t: &Track,
    codec: &str,
    movie_timescale: u32,
    smpb: Smpb,
    fragmented: bool,
    samples: &mut Vec<SampleRef>,
    range: Range<usize>,
) -> Option<SeekTrim> {
    if t.media_type != MediaType::Audio || t.timescale == 0 || range.is_empty() {
        return None;
    }
    let edits = edits(t, movie_timescale);
    // The media composition time of a sample: `delta_for_cts` gives every
    // sample up to the end of the first segment the same shift.
    let shift = edits.first.map_or(0, |(media_time, _)| t.elst_timeline.delta_for_cts(media_time).0);
    let cts = |s: &SampleRef| s.pts.saturating_sub(shift);
    let track = &samples[range.clone()];
    // FFmpeg's frame duration: the dts step to the next sample, and the
    // edit's duration for the last one.
    let step = |k: usize, last: i64| match track.get(k + 1) {
        Some(next) => next.dts.saturating_sub(track[k].dts),
        None => last,
    };

    let mut skip = 0i64;
    let mut keep = track.len();
    if let Some((media_time, duration)) = edits.first {
        if codec != "vorbis" {
            for (k, s) in track.iter().enumerate() {
                let c = cts(s);
                if c >= media_time {
                    break;
                }
                let frame = step(k, duration);
                if c.saturating_add(frame) > media_time {
                    skip = skip.saturating_add(media_time - c);
                    break;
                }
                skip = skip.saturating_add(frame);
            }
        }
        if edits.count == 1 && duration > 0 {
            let end = media_time.saturating_add(duration);
            if let Some(k) = (0..track.len()).find(|&k| cts(&track[k]).saturating_add(step(k, duration)) >= end) {
                keep = k + 1;
            }
        }
    }
    samples.drain(range.start + keep..range.end);
    let track = &mut samples[range.start..range.start + keep];

    // st->duration: mdhd, then the stts total, then the edit list's length.
    let mut duration = t.duration.map(|d| if d == u64::from(u32::MAX) || d == u64::MAX { 0 } else { d });
    let stts_total = t.stts.iter().fold(0i64, |sum, &(n, d)| sum.saturating_add(i64::from(n).saturating_mul(i64::from(d))));
    if let Some(d) = duration.as_mut() {
        if stts_total > 0 {
            *d = (*d).min(stts_total as u64);
        }
        if !t.elst.is_empty() {
            *d = (*d).min(edits.total.max(0) as u64);
        }
    }
    let duration = duration.map(|d| i64::try_from(d).unwrap_or(i64::MAX));

    // The samples FFmpeg discards: `(first, last, every packet)`.
    let mut initial_padding = skip;
    let mut window: Option<(i64, i64, bool)> = None;
    if let Some((priming, remainder, valid)) = smpb {
        if priming > 0 && priming < 16384 {
            initial_padding = priming;
            if codec == "aac" {
                skip = priming;
            }
        }
        if let Some(total) = duration {
            if remainder > 0 && total > remainder && total > valid {
                window = Some((total - remainder, total, true));
            }
        }
    }
    if window.is_none() && !fragmented {
        if let (Some(total), Some(last)) = (duration, track.last()) {
            let presented = if total < last.pts { 0 } else { last.duration.min(total.saturating_sub(last.pts)) };
            window = Some((last.pts.saturating_add(presented), total, false));
        }
    }
    if let Some((first, last, every)) = window.filter(|&(first, _, _)| first != 0) {
        let frame = frame_size(t, codec);
        let from = if every { 0 } else { track.len() - 1 };
        for s in &mut track[from..] {
            let length = frame.max(s.duration);
            let end = s.pts.saturating_add(length);
            if length > 0 && end >= first && s.pts < last {
                s.trim_discard = clamp_u32((end - first).min(length));
            }
        }
    }
    if let Some(first) = track.first_mut() {
        first.trim_skip = clamp_u32(skip);
    }
    Some(SeekTrim { initial_padding, first_dts: track.first().map_or(0, |s| s.dts) })
}

/// The skip of fragmented audio track `track_idx`, whose samples all come
/// from movie fragments: an AAC track's first packet skips its single edit's
/// media time, or its iTunSMPB priming.
pub(super) fn fragments(
    t: &Track,
    track_idx: u32,
    codec: &str,
    smpb: Smpb,
    samples: &mut [SampleRef],
) -> Option<SeekTrim> {
    if t.media_type != MediaType::Audio || t.timescale == 0 || codec != "aac" {
        return None;
    }
    // mov.c `mov_build_index`: an optional empty first edit, then one edit
    // whose media time starts the media; anything else is several edits.
    let mut start = 0i64;
    let mut index = 0;
    let mut multiple = false;
    for (i, e) in t.elst.iter().enumerate() {
        if i == 0 && e.media_time == -1 {
            index = 1;
        } else if i == index && e.media_time >= 0 {
            start = e.media_time;
        } else {
            multiple = true;
        }
    }
    let mut initial_padding = if !multiple && start > 0 { start } else { 0 };
    if let Some((priming, _, _)) = smpb {
        if priming > 0 && priming < 16384 {
            initial_padding = priming;
        }
    }
    let first = samples.iter_mut().find(|s| s.track_idx == track_idx)?;
    first.trim_skip = clamp_u32(initial_padding);
    Some(SeekTrim { initial_padding, first_dts: first.dts })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smpb_fields_parse_as_ffmpeg_reads_them() {
        let v = " 00000000 00000840 0000037C 000000000000AC44 00000000 00000000";
        assert_eq!(parse_smpb(v), Some((0x840, 0x37C, 0xAC44)));
        // A 17th digit would shift every later field: rejected.
        assert_eq!(parse_smpb(" 0 840 37C 00000000000000AC44"), None);
        assert_eq!(parse_smpb(" 0 840 37C"), None, "three fields");
        assert_eq!(parse_smpb(" 0 840 37C 20000000000"), None, "above 2^40");
    }

    #[test]
    fn rescale_rounds_halves_away_from_zero() {
        assert_eq!(rescale(6000, 48000, 1000), 288000);
        assert_eq!(rescale(1, 3, 2), 2);
        assert_eq!(rescale(-1, 3, 2), -2);
        assert_eq!(rescale(i64::MAX, 4, 1), i64::MAX);
    }
}
