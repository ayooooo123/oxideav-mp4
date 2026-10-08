//! AMR in 3GPP files is mono at the AMR rate. 3GPP fixes an AMR sample
//! entry's channel count at 2 (TS 26.244 §6.5); FFmpeg 2da55bf's mov
//! demuxer forces mono and 8000 Hz (AMR-NB) or 16000 Hz (AMR-WB)
//! (`mov_finalize_stsd_codec`), and so does this one.

use std::io::Cursor;

use oxideav_core::{CodecId, CodecResolver, CodecTag, ProbeContext, ReadSeek};

/// Resolves the two AMR entries as the AMR decoders claim them.
struct Amr;

impl CodecResolver for Amr {
    fn resolve_tag(&self, ctx: &ProbeContext) -> Option<CodecId> {
        let CodecTag::Fourcc(fourcc) = ctx.tag else { return None };
        match fourcc {
            b"SAMR" => Some(CodecId::new("amr_nb")),
            b"SAWB" => Some(CodecId::new("amr_wb")),
            _ => None,
        }
    }
}

fn boxed(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = ((8 + body.len()) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(fourcc);
    out.extend_from_slice(body);
    out
}

fn full(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
    boxed(fourcc, &[&[0u8; 4][..], body].concat())
}

/// A `3gp4` file with one AMR track: `format` entry saying 2 channels and
/// `rate`, one 13-byte frame.
fn amr_3gp(format: &[u8; 4], rate: u32) -> Vec<u8> {
    let mut entry = vec![0u8; 28];
    entry[6..8].copy_from_slice(&1u16.to_be_bytes());
    entry[16..18].copy_from_slice(&2u16.to_be_bytes());
    entry[18..20].copy_from_slice(&16u16.to_be_bytes());
    entry[24..28].copy_from_slice(&(rate << 16).to_be_bytes());
    let mut stsd = 1u32.to_be_bytes().to_vec();
    stsd.extend_from_slice(&boxed(format, &entry));
    let moov = |offset: u32| {
        let mut stbl = full(b"stsd", &stsd);
        stbl.extend_from_slice(&full(b"stts", &[0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 160]));
        stbl.extend_from_slice(&full(b"stsc", &[0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1]));
        stbl.extend_from_slice(&full(b"stsz", &[0, 0, 0, 13, 0, 0, 0, 1]));
        stbl.extend_from_slice(&full(b"stco", &[&1u32.to_be_bytes()[..], &offset.to_be_bytes()].concat()));
        let mut minf = full(b"smhd", &[0u8; 4]);
        minf.extend_from_slice(&boxed(b"stbl", &stbl));
        let mut mdhd = vec![0u8; 20];
        mdhd[8..12].copy_from_slice(&rate.to_be_bytes());
        let mut mdia = full(b"mdhd", &mdhd);
        mdia.extend_from_slice(&full(b"hdlr", &[&[0u8; 4][..], b"soun", &[0u8; 13]].concat()));
        mdia.extend_from_slice(&boxed(b"minf", &minf));
        let mut tkhd = vec![0u8; 80];
        tkhd[12..16].copy_from_slice(&1u32.to_be_bytes());
        let mut trak = full(b"tkhd", &tkhd);
        trak.extend_from_slice(&boxed(b"mdia", &mdia));
        let mut mvhd = vec![0u8; 96];
        mvhd[8..12].copy_from_slice(&1000u32.to_be_bytes());
        let mut moov = full(b"mvhd", &mvhd);
        moov.extend_from_slice(&boxed(b"trak", &trak));
        boxed(b"moov", &moov)
    };
    let mut file = boxed(b"ftyp", b"3gp4\0\0\0\0isom3gp4");
    let offset = (file.len() + moov(0).len() + 8) as u32;
    file.extend_from_slice(&moov(offset));
    file.extend_from_slice(&boxed(b"mdat", &[0x3C; 13]));
    file
}

#[test]
fn amr_entries_are_mono_at_the_amr_rate() {
    for (format, entry_rate, codec, rate) in [(b"samr", 8000, "amr_nb", 8000), (b"sawb", 16000, "amr_wb", 16000), (b"samr", 0, "amr_nb", 8000)] {
        let input: Box<dyn ReadSeek> = Box::new(Cursor::new(amr_3gp(format, entry_rate)));
        let d = oxideav_mp4::demux::open(input, &Amr).expect("open");
        let params = &d.streams()[0].params;
        assert_eq!(params.codec_id.as_str(), codec);
        assert_eq!((params.channels, params.sample_rate), (Some(1), Some(rate)), "{codec} entry at {entry_rate} Hz");
    }
}
