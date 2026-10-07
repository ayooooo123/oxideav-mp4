//! `Demuxer::packet_metadata().audio_trim` for audio tracks: the encoder
//! delay and end padding FFmpeg's mov demuxer (2da55bf) attaches to packets
//! as `AV_PKT_DATA_SKIP_SAMPLES`, from the edit list, the sample table and
//! iTunSMPB. Synthetic files, built box by box.

use std::io::Cursor;

use oxideav_core::{AudioTrim, Demuxer, Error, NullCodecResolver, ReadSeek};

fn boxed(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = ((8 + body.len()) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(fourcc);
    out.extend_from_slice(body);
    out
}

fn full_box(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
    boxed(fourcc, &[&[0u8; 4][..], body].concat())
}

fn mvhd(timescale: u32) -> Vec<u8> {
    let mut body = vec![0u8; 100];
    body[12..16].copy_from_slice(&timescale.to_be_bytes());
    body[20..24].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    body[96..100].copy_from_slice(&2u32.to_be_bytes());
    boxed(b"mvhd", &body)
}

fn tkhd() -> Vec<u8> {
    let mut body = vec![0u8; 80];
    body[3] = 7;
    body[12..16].copy_from_slice(&1u32.to_be_bytes());
    boxed(b"tkhd", &body)
}

fn mdhd(timescale: u32, duration: u32) -> Vec<u8> {
    let mut body = vec![0u8; 24];
    body[12..16].copy_from_slice(&timescale.to_be_bytes());
    body[16..20].copy_from_slice(&duration.to_be_bytes());
    boxed(b"mdhd", &body)
}

fn hdlr(kind: &[u8; 4]) -> Vec<u8> {
    full_box(b"hdlr", &[&[0u8; 4][..], kind, &[0u8; 13]].concat())
}

/// An AAC (`mp4a`) sample entry without an `esds`.
fn stsd_mp4a(rate: u32) -> Vec<u8> {
    let mut entry = vec![0u8; 28];
    entry[6..8].copy_from_slice(&1u16.to_be_bytes());
    entry[16..18].copy_from_slice(&2u16.to_be_bytes());
    entry[18..20].copy_from_slice(&16u16.to_be_bytes());
    entry[24..28].copy_from_slice(&(rate << 16).to_be_bytes());
    full_box(b"stsd", &[&1u32.to_be_bytes()[..], &boxed(b"mp4a", &entry)].concat())
}

fn stts(runs: &[(u32, u32)]) -> Vec<u8> {
    let mut body = (runs.len() as u32).to_be_bytes().to_vec();
    for &(count, delta) in runs {
        body.extend_from_slice(&count.to_be_bytes());
        body.extend_from_slice(&delta.to_be_bytes());
    }
    full_box(b"stts", &body)
}

/// `elst` v1 from `(segment_duration, media_time)` pairs at rate 1.
fn elst(entries: &[(u64, i64)]) -> Vec<u8> {
    let mut body = vec![1u8, 0, 0, 0];
    body.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for &(duration, media_time) in entries {
        body.extend_from_slice(&duration.to_be_bytes());
        body.extend_from_slice(&media_time.to_be_bytes());
        body.extend_from_slice(&[0, 1, 0, 0]);
    }
    boxed(b"edts", &boxed(b"elst", &body))
}

/// `moov/udta/meta/ilst/----` carrying iTunSMPB `value`.
fn itunsmpb(value: &str) -> Vec<u8> {
    let mean = full_box(b"mean", b"com.apple.iTunes");
    let name = full_box(b"name", b"iTunSMPB");
    let data = boxed(b"data", &[&[0, 0, 0, 1, 0, 0, 0, 0][..], value.as_bytes()].concat());
    let item = boxed(b"----", &[mean, name, data].concat());
    let meta = full_box(b"meta", &[hdlr(b"mdir"), boxed(b"ilst", &item)].concat());
    boxed(b"udta", &meta)
}

/// One AAC track: `runs` of (samples, duration) in `timescale`, the mdhd
/// `duration`, an optional `edts`, and moov-level boxes after the track.
fn aac_file(timescale: u32, runs: &[(u32, u32)], duration: u32, edts: &[u8], after: &[u8]) -> Vec<u8> {
    let count: u32 = runs.iter().map(|r| r.0).sum();
    let moov = |offset: u32| {
        let stsc = full_box(b"stsc", &[1u32, 1, count, 1].iter().flat_map(|v| v.to_be_bytes()).collect::<Vec<_>>());
        let stsz = full_box(b"stsz", &[4u32, count].iter().flat_map(|v| v.to_be_bytes()).collect::<Vec<_>>());
        let stco = full_box(b"stco", &[1u32, offset].iter().flat_map(|v| v.to_be_bytes()).collect::<Vec<_>>());
        let stbl = boxed(b"stbl", &[stsd_mp4a(timescale), stts(runs), stsc, stsz, stco].concat());
        let minf = boxed(b"minf", &[boxed(b"smhd", &[0; 8]), stbl].concat());
        let mdia = boxed(b"mdia", &[mdhd(timescale, duration), hdlr(b"soun"), minf].concat());
        let trak = boxed(b"trak", &[tkhd(), edts.to_vec(), mdia].concat());
        boxed(b"moov", &[mvhd(timescale), trak, after.to_vec()].concat())
    };
    let ftyp = boxed(b"ftyp", b"M4A \0\0\0\0isomM4A ");
    let offset = (ftyp.len() + moov(0).len() + 8) as u32;
    [ftyp, moov(offset), boxed(b"mdat", &vec![0u8; 4 * count as usize])].concat()
}

fn open(file: Vec<u8>) -> Box<dyn Demuxer> {
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(file));
    oxideav_mp4::demux::open(input, &NullCodecResolver).unwrap()
}

/// Every packet's pts and trim.
fn trims(d: &mut dyn Demuxer) -> Vec<(i64, Option<AudioTrim>)> {
    let mut out = Vec::new();
    loop {
        match d.next_packet() {
            Ok(p) => out.push((p.pts.unwrap(), d.packet_metadata().audio_trim)),
            Err(Error::Eof) => return out,
            Err(e) => panic!("demux: {e}"),
        }
    }
}

fn trim(skip: u32, discard: u32, rate: u32) -> Option<AudioTrim> {
    Some(AudioTrim { skip_samples: skip, discard_padding: discard, sample_rate: rate })
}

/// 10 × 1024-sample AUs; the edit presents 5000 samples from 2112 on.
fn edited() -> Vec<u8> {
    aac_file(44100, &[(10, 1024)], 10240, &elst(&[(5000, 2112)]), &[])
}

#[test]
fn edit_list_priming_spans_packets_and_the_edit_end_trims_the_tail() {
    let got = trims(&mut *open(edited()));
    // The AU that reaches the edit's end is the last; its decoded tail past
    // 5000 presented samples is padding: 4032 + 1024 - 5000 = 56.
    let pts: Vec<i64> = got.iter().map(|g| g.0).collect();
    assert_eq!(pts, [-2112, -1088, -64, 960, 1984, 3008, 4032]);
    let expected: Vec<_> =
        (0..7).map(|i| match i { 0 => trim(2112, 0, 44100), 6 => trim(0, 56, 44100), _ => None }).collect();
    assert_eq!(got.iter().map(|g| g.1).collect::<Vec<_>>(), expected);
}

#[test]
fn a_seek_skips_the_priming_still_ahead_of_the_landing_packet() {
    let mut d = open(edited());
    assert_eq!(d.seek_to(0, 0).unwrap(), -64);
    let got = trims(&mut *d);
    assert_eq!(got[0], (-64, trim(64, 0, 44100)));
    assert_eq!(got.last().unwrap().1, trim(0, 56, 44100));
    // Past the priming nothing is skipped; the metadata resets on seek.
    d.seek_to(0, 1000).unwrap();
    assert_eq!(d.packet_metadata().audio_trim, None);
    assert_eq!(trims(&mut *d)[0], (960, None));
}

#[test]
fn a_track_shorter_than_its_last_frame_discards_the_rest() {
    // ER AAC ELD's shape: 512-sample AUs, mdhd 58 samples short of them.
    let got = trims(&mut *open(aac_file(48000, &[(4, 512)], 1990, &[], &[])));
    assert_eq!(got.iter().map(|g| g.1).collect::<Vec<_>>(), [None, None, None, trim(0, 58, 48000)]);
    // A last sample whose duration is the presented rest still decodes a
    // whole frame: the usual frame length counts.
    let got = trims(&mut *open(aac_file(48000, &[(3, 1024), (1, 261)], 3333, &[], &[])));
    assert_eq!(got.last().unwrap().1, trim(0, 1024 - 261, 48000));
}

#[test]
fn itunsmpb_priming_and_remainder() {
    let tag = " 00000000 00000840 000005DC 0000000000000AC4 00000000 00000000";
    let got = trims(&mut *open(aac_file(44100, &[(5, 1024)], 5120, &[], &itunsmpb(tag))));
    // 2112 priming; the last 1500 samples are padding: 476 of the fourth
    // AU and all of the fifth.
    let expected = [trim(2112, 0, 44100), None, None, trim(0, 476, 44100), trim(0, 1024, 44100)];
    assert_eq!(got.iter().map(|g| g.1).collect::<Vec<_>>(), expected);
}

/// A fragmented AAC file: an empty sample table, `mvex`, `edts`, and one
/// fragment of `count` 1024-sample AUs.
fn fragmented_aac(edts: &[u8], count: u32) -> Vec<u8> {
    let words = |v: &[u32]| v.iter().flat_map(|w| w.to_be_bytes()).collect::<Vec<u8>>();
    let stbl = [
        stsd_mp4a(48000),
        full_box(b"stts", &[0; 4]),
        full_box(b"stsc", &[0; 4]),
        full_box(b"stsz", &[0; 8]),
        full_box(b"stco", &[0; 4]),
    ];
    let minf = boxed(b"minf", &[boxed(b"smhd", &[0; 8]), boxed(b"stbl", &stbl.concat())].concat());
    let mdia = boxed(b"mdia", &[mdhd(48000, 0), hdlr(b"soun"), minf].concat());
    let trak = boxed(b"trak", &[tkhd(), edts.to_vec(), mdia].concat());
    let mvex = boxed(b"mvex", &full_box(b"trex", &words(&[1, 1, 1024, 4, 0])));
    let moov = boxed(b"moov", &[mvhd(48000), trak, mvex].concat());
    let moof = |offset: u32| {
        let tfhd = boxed(b"tfhd", &[&[0, 2, 0, 0][..], &words(&[1])].concat());
        let tfdt = boxed(b"tfdt", &[&[1, 0, 0, 0][..], &0u64.to_be_bytes()].concat());
        let sizes = vec![4u32; count as usize];
        let trun = boxed(b"trun", &[&[0, 0, 2, 1][..], &words(&[count, offset]), &words(&sizes)].concat());
        let traf = boxed(b"traf", &[tfhd, tfdt, trun].concat());
        boxed(b"moof", &[full_box(b"mfhd", &words(&[1])), traf].concat())
    };
    let size = moof(0).len() as u32;
    let ftyp = boxed(b"ftyp", b"iso6\0\0\0\0iso6");
    [ftyp, moov, moof(size + 8), boxed(b"mdat", &vec![0u8; 4 * count as usize])].concat()
}

#[test]
fn fragmented_aac_skips_its_edit_media_time() {
    // No end padding without a sample table, as FFmpeg's fragment path.
    let got = trims(&mut *open(fragmented_aac(&elst(&[(0, 1024)]), 5)));
    assert_eq!(got.iter().map(|g| g.1).collect::<Vec<_>>(), [trim(1024, 0, 48000), None, None, None, None]);
}

#[test]
fn hostile_edit_lists_stay_bounded() {
    // An edit that starts past the media makes all of it priming; durations
    // that overflow saturate instead of wrapping, and no count exceeds the
    // media or a frame.
    let starts_past_the_end = [elst(&[(u64::MAX, i64::MAX - 10)]), elst(&[(1, i64::MAX)])];
    for edts in starts_past_the_end.iter().chain([&elst(&[(u64::MAX, -1), (7, 3)])]) {
        let got = trims(&mut *open(aac_file(44100, &[(6, 1024)], 6144, edts, &[])));
        for t in got.iter().filter_map(|g| g.1) {
            assert!(t.skip_samples <= 6 * 1024 && t.discard_padding <= 1024, "{t:?}");
        }
        if starts_past_the_end.contains(edts) {
            let skip = got[0].1.map_or(0, |t| t.skip_samples);
            assert!(skip >= 5 * 1024, "skip {skip} leaves priming");
        }
    }
}
