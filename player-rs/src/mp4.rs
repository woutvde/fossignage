//! Minimal ISO base media file format (mp4/mov) demuxer: enough to extract
//! the H.264 video track — avcC (SPS/PPS), the sample table (offsets, sizes,
//! timestamps, keyframes) — for feeding a V4L2 stateful decoder.
//! All parsing is bounds-checked; malformed files return errors, not panics.

#[derive(Debug, Clone)]
pub struct Sample {
    pub offset: u64,
    pub size: u32,
    /// Presentation timestamp in track timescale units.
    pub pts: u64,
    pub duration: u64,
    pub is_keyframe: bool,
}

#[derive(Debug)]
pub struct Mp4Video {
    pub timescale: u32,
    pub width: u32,
    pub height: u32,
    /// Annex-B start-code-prefixed SPS/PPS (send once before the samples).
    pub avc_prefix: Vec<u8>,
    pub nal_length_size: u8,
    pub samples: Vec<Sample>,
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }
    fn u8(&mut self) -> Result<u8, String> {
        let b = *self
            .data
            .get(self.pos)
            .ok_or_else(|| "unexpected EOF".to_string())?;
        self.pos += 1;
        Ok(b)
    }
    fn u16(&mut self) -> Result<u16, String> {
        let b = self.slice(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32, String> {
        let b = self.slice(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> Result<u64, String> {
        let b = self.slice(8)?;
        Ok(u64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
    fn slice(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.remaining() < n {
            return Err("unexpected EOF".into());
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn skip(&mut self, n: u64) -> Result<(), String> {
        if self.remaining() < n as usize {
            return Err("unexpected EOF".into());
        }
        self.pos += n as usize;
        Ok(())
    }
    fn fourcc(&mut self) -> Result<String, String> {
        let b = self.slice(4)?;
        Ok(String::from_utf8_lossy(b).to_string())
    }
}

#[derive(Default)]
struct Track {
    is_video: bool,
    timescale: u32,
    width: u32,
    height: u32,
    avc_config: Vec<u8>,
    sample_sizes: Vec<u32>,
    sample_durations: Vec<u64>,
    chunk_offsets: Vec<u64>,
    samples_per_chunk: Vec<u32>,
    keyframes: Vec<u32>,
    has_stss: bool,
}

/// Parses an ISO-BMFF file and returns its first H.264 video track.
pub fn parse(data: &[u8]) -> Result<Mp4Video, String> {
    parse_inner(data)
}

fn parse_inner(data: &[u8]) -> Result<Mp4Video, String> {
    let mut best: Option<Track> = None;
    parse_moov(data, 0, data.len(), &mut best)?;
    let t = best.ok_or("no video track found")?;
    if t.avc_config.is_empty() {
        return Err("video track has no avcC (not H.264?)".into());
    }
    let (avc_prefix, nal_length_size) = crate::h264::parse_avcc(&t.avc_config)?;
    let samples = build_samples(&t)?;
    if samples.is_empty() {
        return Err("video track has no samples".into());
    }
    Ok(Mp4Video {
        timescale: t.timescale.max(1),
        width: t.width,
        height: t.height,
        avc_prefix,
        nal_length_size,
        samples,
    })
}

fn parse_moov(
    data: &[u8],
    start: usize,
    end: usize,
    best: &mut Option<Track>,
) -> Result<(), String> {
    let mut pos = start;
    while pos + 8 <= end {
        let mut r = Reader { data, pos };
        let size = r.u32()?;
        let fourcc = r.fourcc()?;
        let (ps, pe) = if size == 1 {
            let large = r.u64()?;
            (pos + 16, pos + large as usize)
        } else if size == 0 {
            (pos + 8, end)
        } else {
            (pos + 8, pos + size as usize)
        };
        if pe > end || pe < ps {
            return Err(format!("bad box {fourcc}"));
        }
        match fourcc.as_str() {
            "moov" | "mdia" | "minf" | "stbl" => {
                parse_moov(data, ps, pe, best)?;
            }
            "trak" => {
                let mut t = Track::default();
                parse_trak(data, ps, pe, &mut t)?;
                if t.is_video && best.is_none() {
                    *best = Some(t);
                }
            }
            _ => {}
        }
        pos = pe;
    }
    Ok(())
}

fn parse_trak(data: &[u8], start: usize, end: usize, t: &mut Track) -> Result<(), String> {
    let mut pos = start;
    while pos + 8 <= end {
        let mut r = Reader { data, pos };
        let size = r.u32()?;
        let fourcc = r.fourcc()?;
        let (ps, pe) = if size == 1 {
            let large = r.u64()?;
            (pos + 16, pos + large as usize)
        } else if size == 0 {
            (pos + 8, end)
        } else {
            (pos + 8, pos + size as usize)
        };
        if pe > end || pe < ps {
            return Err(format!("bad box {fourcc} in trak"));
        }
        if fourcc == "mdia" {
            parse_mdia(data, ps, pe, t)?;
        }
        pos = pe;
    }
    Ok(())
}

fn parse_mdia(data: &[u8], start: usize, end: usize, t: &mut Track) -> Result<(), String> {
    let mut pos = start;
    while pos + 8 <= end {
        let mut r = Reader { data, pos };
        let size = r.u32()?;
        let fourcc = r.fourcc()?;
        let (ps, pe) = if size == 1 {
            let large = r.u64()?;
            (pos + 16, pos + large as usize)
        } else if size == 0 {
            (pos + 8, end)
        } else {
            (pos + 8, pos + size as usize)
        };
        if pe > end || pe < ps {
            return Err(format!("bad box {fourcc} in mdia"));
        }
        match fourcc.as_str() {
            "hdlr" => {
                let mut r = Reader { data, pos: ps };
                r.skip(8)?;
                t.is_video = r.fourcc()? == "vide";
            }
            "mdhd" => {
                let mut r = Reader { data, pos: ps };
                let version = r.u8()?;
                r.skip(3)?;
                if version == 1 {
                    r.skip(16)?;
                } else {
                    r.skip(8)?;
                }
                t.timescale = r.u32()?;
            }
            "minf" => parse_minf(data, ps, pe, t)?,
            _ => {}
        }
        pos = pe;
    }
    Ok(())
}

fn parse_minf(data: &[u8], start: usize, end: usize, t: &mut Track) -> Result<(), String> {
    let mut pos = start;
    while pos + 8 <= end {
        let mut r = Reader { data, pos };
        let size = r.u32()?;
        let fourcc = r.fourcc()?;
        let (ps, pe) = if size == 1 {
            let large = r.u64()?;
            (pos + 16, pos + large as usize)
        } else if size == 0 {
            (pos + 8, end)
        } else {
            (pos + 8, pos + size as usize)
        };
        if pe > end || pe < ps {
            return Err(format!("bad box {fourcc} in minf"));
        }
        if fourcc == "stbl" {
            parse_stbl(data, ps, pe, t)?;
        } else if fourcc == "vmhd" || fourcc == "smhd" || fourcc == "nmhd" {
            // media header; nothing needed
        }
        pos = pe;
    }
    // width/height fallback from tkhd lives in trak; handled by caller via stsd
    Ok(())
}

fn parse_stbl(data: &[u8], start: usize, end: usize, t: &mut Track) -> Result<(), String> {
    let mut pos = start;
    while pos + 8 <= end {
        let mut r = Reader { data, pos };
        let size = r.u32()?;
        let fourcc = r.fourcc()?;
        let (ps, pe) = if size == 1 {
            let large = r.u64()?;
            (pos + 16, pos + large as usize)
        } else if size == 0 {
            (pos + 8, end)
        } else {
            (pos + 8, pos + size as usize)
        };
        if pe > end || pe < ps {
            return Err(format!("bad box {fourcc} in stbl"));
        }
        match fourcc.as_str() {
            "stsd" => parse_stsd(data, ps, pe, t)?,
            "stsz" => {
                let mut r = Reader { data, pos: ps };
                r.skip(4)?;
                let sample_size = r.u32()?;
                let count = r.u32()?;
                if sample_size == 0 {
                    for _ in 0..count {
                        t.sample_sizes.push(r.u32()?);
                    }
                } else {
                    t.sample_sizes = vec![sample_size; count as usize];
                }
            }
            "stts" => {
                let mut r = Reader { data, pos: ps };
                r.skip(4)?;
                let count = r.u32()?;
                for _ in 0..count {
                    let n = r.u32()? as u64;
                    let d = r.u32()? as u64;
                    for _ in 0..n {
                        t.sample_durations.push(d);
                    }
                }
            }
            "stsc" => {
                let mut r = Reader { data, pos: ps };
                r.skip(4)?;
                let count = r.u32()?;
                for _ in 0..count {
                    let _first = r.u32()?;
                    t.samples_per_chunk.push(r.u32()?);
                    let _desc = r.u32()?;
                }
            }
            "stco" => {
                let mut r = Reader { data, pos: ps };
                r.skip(4)?;
                let count = r.u32()?;
                for _ in 0..count {
                    t.chunk_offsets.push(r.u32()? as u64);
                }
            }
            "co64" => {
                let mut r = Reader { data, pos: ps };
                r.skip(4)?;
                let count = r.u32()?;
                for _ in 0..count {
                    t.chunk_offsets.push(r.u64()?);
                }
            }
            "stss" => {
                t.has_stss = true;
                let mut r = Reader { data, pos: ps };
                r.skip(4)?;
                let count = r.u32()?;
                for _ in 0..count {
                    t.keyframes.push(r.u32()?);
                }
            }
            _ => {}
        }
        pos = pe;
    }
    Ok(())
}

fn parse_stsd(data: &[u8], ps: usize, pe: usize, t: &mut Track) -> Result<(), String> {
    let mut r = Reader { data, pos: ps };
    r.skip(4)?; // version/flags
    let count = r.u32()?;
    for _ in 0..count {
        if r.remaining() < 8 {
            break;
        }
        let entry_size = r.u32()? as usize;
        let entry_type = r.fourcc()?;
        if entry_size < 8 || r.pos - 8 + entry_size > pe {
            break;
        }
        let entry_start = r.pos - 8;
        if entry_type == "avc1" || entry_type == "avc3" {
            let mut er = Reader {
                data,
                pos: entry_start + 8,
            };
            er.skip(8)?; // reserved + data_reference_index
            er.skip(16)?; // pre_defined + reserved
            let w = er.u16()? as u32;
            let h = er.u16()? as u32;
            if w > 0 && h > 0 {
                t.width = w;
                t.height = h;
            }
            // child boxes of the sample entry (VisualSampleEntry body = 78 bytes)
            let entry_end = entry_start + entry_size;
            let mut cp = entry_start + 8 + 78;
            while cp + 8 <= entry_end {
                let mut cr = Reader { data, pos: cp };
                let csize = cr.u32()? as usize;
                let ctype = cr.fourcc()?;
                if csize < 8 || cp + csize > entry_end {
                    break;
                }
                if ctype == "avcC" {
                    t.avc_config = data[cp + 8..cp + csize].to_vec();
                }
                cp += csize;
            }
        }
        r.pos = entry_start + entry_size;
    }
    Ok(())
}

/// Expands stsc/stco/stsz/stts into per-sample offsets/sizes/timestamps.
fn build_samples(t: &Track) -> Result<Vec<Sample>, String> {
    let n = t.sample_sizes.len();
    if n == 0 {
        return Ok(Vec::new());
    }
    if t.sample_durations.len() < n {
        return Err("stts shorter than stsz".into());
    }
    // expand stsc into per-chunk sample counts
    let mut per_chunk: Vec<u32> = Vec::with_capacity(t.chunk_offsets.len());
    for i in 0..t.chunk_offsets.len() {
        let count = t
            .samples_per_chunk
            .get(i)
            .copied()
            .or_else(|| t.samples_per_chunk.last().copied())
            .unwrap_or(0);
        per_chunk.push(count);
    }
    let total_in_chunks: u64 = per_chunk.iter().map(|&c| c as u64).sum();
    if total_in_chunks < n as u64 {
        // some files omit the final stsc entry; extend last chunk
        let need = n as u64 - total_in_chunks;
        if let Some(last) = per_chunk.last_mut() {
            *last += need as u32;
        }
    }
    let mut samples = Vec::with_capacity(n);
    let mut sample_idx = 0u64; // 1-based for stss
    let mut pts = 0u64;
    for (chunk_i, &offset) in t.chunk_offsets.iter().enumerate() {
        let count = per_chunk.get(chunk_i).copied().unwrap_or(0);
        let mut pos = offset;
        for _ in 0..count {
            if sample_idx as usize >= n {
                break;
            }
            let si = sample_idx as usize;
            let size = t.sample_sizes[si];
            let duration = t.sample_durations[si];
            let is_key = if t.has_stss {
                t.keyframes.contains(&((sample_idx + 1) as u32))
            } else {
                true // no stss: every sample is a sync sample
            };
            samples.push(Sample {
                offset: pos,
                size,
                pts,
                duration,
                is_keyframe: is_key,
            });
            pos += size as u64;
            pts += duration;
            sample_idx += 1;
        }
    }
    if samples.len() < n {
        return Err(format!(
            "sample table mismatch: got {} of {} samples",
            samples.len(),
            n
        ));
    }
    Ok(samples)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_ffmpeg_mp4() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/clip_h264.mp4");
        let data = std::fs::read(path).expect("fixture clip_h264.mp4");
        let v = parse(&data).unwrap();
        assert_eq!((v.width, v.height), (320, 240));
        assert_eq!(v.timescale, 10240.max(v.timescale)); // nonzero
        assert!(v.timescale > 0);
        assert!(!v.samples.is_empty());
        assert!(v.samples.len() >= 10, "1.2s @10fps -> ~12 samples");
        // 10 fps, timescale/512... durations should be constant
        let d0 = v.samples[0].duration;
        assert!(v.samples.iter().all(|s| s.duration == d0));
        assert!(v.samples[0].is_keyframe);
        // avc prefix contains SPS (nal 7) and PPS (nal 8)
        let nals = crate::h264::split_annexb(&v.avc_prefix);
        assert!(nals.iter().any(|n| n[0] & 0x1f == 7));
        assert!(nals.iter().any(|n| n[0] & 0x1f == 8));
        // SPS parses to the coded size
        let sps = nals.iter().find(|n| n[0] & 0x1f == 7).unwrap();
        assert_eq!(crate::h264::parse_sps(sps).unwrap(), (320, 240));
        // samples are inside the file
        for s in &v.samples {
            assert!(s.offset + s.size as u64 <= data.len() as u64);
        }
        // pts monotonic
        for w in v.samples.windows(2) {
            assert!(w[1].pts >= w[0].pts);
        }
        // first sample converts to annex-b with start codes
        let s0 = &data[v.samples[0].offset as usize
            ..(v.samples[0].offset + v.samples[0].size as u64) as usize];
        let annexb = crate::h264::avcc_sample_to_annexb(s0, v.nal_length_size).unwrap();
        assert!(annexb.starts_with(&[0, 0, 0, 1]));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse(b"not an mp4 at all").is_err());
        assert!(parse(b"").is_err());
        assert!(parse(&[0u8; 100]).is_err());
    }

    #[test]
    fn hand_built_mp4() {
        // Build a minimal mp4 with one video track, 2 samples.
        fn mkbox(fourcc: &str, payload: &[u8]) -> Vec<u8> {
            let mut v = Vec::new();
            v.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
            v.extend_from_slice(fourcc.as_bytes());
            v.extend_from_slice(payload);
            v
        }
        fn fullbox(fourcc: &str, version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
            let mut p = vec![
                version,
                (flags >> 16) as u8,
                (flags >> 8) as u8,
                flags as u8,
            ];
            p.extend_from_slice(payload);
            mkbox(fourcc, &p)
        }
        // avcC: 1 SPS (fake 5 bytes), 1 PPS
        let sps = [0x67u8, 0x42, 0x00, 0x1e, 0xa9];
        let pps = [0x68u8, 0xee];
        let mut avcc = vec![1, 0x42, 0x00, 0x1e, 0xff, 0xe1];
        avcc.extend_from_slice(&u16::to_be_bytes(sps.len() as u16));
        avcc.extend_from_slice(&sps);
        avcc.push(1);
        avcc.extend_from_slice(&u16::to_be_bytes(pps.len() as u16));
        avcc.extend_from_slice(&pps);
        // avc1 sample entry: 78 bytes header + avcC
        let mut avc1 = vec![0u8; 78];
        avc1[24..26].copy_from_slice(&16u16.to_be_bytes()); // width
        avc1[26..28].copy_from_slice(&16u16.to_be_bytes()); // height
        let mut entry_payload = avc1;
        entry_payload.extend_from_slice(&mkbox("avcC", &avcc));
        let avc1_box = {
            let mut v = ((8 + entry_payload.len()) as u32).to_be_bytes().to_vec();
            v.extend_from_slice(b"avc1");
            v.extend_from_slice(&entry_payload);
            v
        };
        let stsd = fullbox("stsd", 0, 0, &{
            let mut p = 1u32.to_be_bytes().to_vec();
            p.extend_from_slice(&avc1_box);
            p
        });
        let stts = fullbox("stts", 0, 0, &{
            let mut p = 1u32.to_be_bytes().to_vec(); // 1 entry
            p.extend_from_slice(&2u32.to_be_bytes()); // 2 samples
            p.extend_from_slice(&512u32.to_be_bytes()); // duration each
            p
        });
        let stsc = fullbox("stsc", 0, 0, &{
            let mut p = 1u32.to_be_bytes().to_vec();
            p.extend_from_slice(&1u32.to_be_bytes()); // first chunk
            p.extend_from_slice(&2u32.to_be_bytes()); // 2 samples/chunk
            p.extend_from_slice(&1u32.to_be_bytes()); // desc
            p
        });
        let stsz = fullbox("stsz", 0, 0, &{
            let mut p = 0u32.to_be_bytes().to_vec(); // variable sizes
            p.extend_from_slice(&2u32.to_be_bytes()); // 2 samples
            p.extend_from_slice(&10u32.to_be_bytes());
            p.extend_from_slice(&12u32.to_be_bytes());
            p
        });
        let stss = fullbox("stss", 0, 0, &{
            let mut p = 1u32.to_be_bytes().to_vec();
            p.extend_from_slice(&1u32.to_be_bytes()); // sample 1 is key
            p
        });
        let vmhd = fullbox("vmhd", 0, 1, &[0u8; 8]);
        let mdhd = fullbox("mdhd", 0, 0, &{
            let mut p = 0u32.to_be_bytes().to_vec(); // creation
            p.extend_from_slice(&0u32.to_be_bytes()); // modification
            p.extend_from_slice(&1000u32.to_be_bytes()); // timescale
            p.extend_from_slice(&1024u32.to_be_bytes()); // duration
            p.extend_from_slice(&[0x55, 0xc4, 0, 0]); // language
            p
        });
        let hdlr = fullbox("hdlr", 0, 0, &{
            let mut p = 0u32.to_be_bytes().to_vec();
            p.extend_from_slice(b"vide");
            p.extend_from_slice(&[0u8; 12]);
            p.extend_from_slice(b"VideoHandler\0");
            p
        });
        let tkhd = fullbox("tkhd", 0, 7, &{
            let mut p = 0u32.to_be_bytes().to_vec(); // creation
            p.extend_from_slice(&0u32.to_be_bytes());
            p.extend_from_slice(&1u32.to_be_bytes()); // track id
            p.extend_from_slice(&0u32.to_be_bytes());
            p.extend_from_slice(&1024u32.to_be_bytes()); // duration
            p.extend_from_slice(&[0u8; 8]);
            p.extend_from_slice(&[0u8; 8]);
            p.extend_from_slice(&[0u8; 2 + 2 + 2 + 2]);
            p.extend_from_slice(&[0u8; 36]); // matrix
            p.extend_from_slice(&(16u32 << 16).to_be_bytes());
            p.extend_from_slice(&(16u32 << 16).to_be_bytes());
            p
        });
        let mvhd = fullbox("mvhd", 0, 0, &[0u8; 100]);
        let ftyp = mkbox("ftyp", b"isom\0\0\0\x01isom");
        // two valid AVCC samples: 10 bytes (NAL len 3 + payload + pad) and
        // 12 bytes (NAL len 2 + payload + pad)
        let mdat_payload = vec![
            0, 0, 0, 3, 0x65, 0x88, 0x84, 0xAA, 0xAA, 0xAA, // sample 1 (10 bytes)
            0, 0, 0, 2, 0x41, 0x9a, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, // sample 2 (12 bytes)
        ];
        // build(stco_offset) assembles the whole file; stco values are fixed
        // width so a probe build tells us the real mdat data offset
        let build = |stco_offset: u32| -> Vec<u8> {
            let stco = fullbox("stco", 0, 0, &{
                let mut p = 1u32.to_be_bytes().to_vec();
                p.extend_from_slice(&stco_offset.to_be_bytes());
                p
            });
            let stbl = mkbox(
                "stbl",
                &[
                    stsd.clone(),
                    stts.clone(),
                    stsc.clone(),
                    stsz.clone(),
                    stco,
                    stss.clone(),
                ]
                .concat(),
            );
            let minf = mkbox("minf", &[vmhd.clone(), stbl].concat());
            let mdia = mkbox("mdia", &[mdhd.clone(), hdlr.clone(), minf].concat());
            let trak = mkbox("trak", &[tkhd.clone(), mdia].concat());
            let moov = mkbox("moov", &[mvhd.clone(), trak].concat());
            let mut file = ftyp.clone();
            file.extend_from_slice(&moov);
            file.extend_from_slice(&mkbox("mdat", &mdat_payload));
            file
        };
        let probe = build(0);
        let mdat_offset = probe.len() - mdat_payload.len(); // start of mdat payload
        let file = build(mdat_offset as u32);

        let v = parse(&file).unwrap();
        assert_eq!((v.width, v.height), (16, 16));
        assert_eq!(v.timescale, 1000);
        assert_eq!(v.samples.len(), 2);
        assert_eq!(v.samples[0].size, 10);
        assert_eq!(v.samples[1].size, 12);
        assert_eq!(v.samples[0].offset, mdat_offset as u64);
        assert_eq!(v.samples[1].offset, mdat_offset as u64 + 10);
        assert_eq!(v.samples[0].pts, 0);
        assert_eq!(v.samples[1].pts, 512);
        assert!(v.samples[0].is_keyframe);
        assert!(!v.samples[1].is_keyframe);
        let annexb = crate::h264::avcc_sample_to_annexb(
            &file[v.samples[0].offset as usize..(v.samples[0].offset + 10) as usize],
            v.nal_length_size,
        )
        .unwrap();
        assert!(annexb.starts_with(&[0, 0, 0, 1]));
    }
}
