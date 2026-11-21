//! Minimal H.264 bitstream parsing: AVCC (mp4) <-> Annex-B conversion and
//! SPS parsing for coded width/height. Enough to feed a V4L2 stateful
//! decoder, not a full parser.

/// Parses an avcC record (from the avc1 sample entry) into Annex-B
/// start-code-prefixed SPS/PPS NAL units, plus the NAL length size.
pub fn parse_avcc(data: &[u8]) -> Result<(Vec<u8>, u8), String> {
    if data.len() < 7 {
        return Err("avcC too short".into());
    }
    if data[0] != 1 {
        return Err(format!("unsupported avcC version {}", data[0]));
    }
    let length_size = (data[4] & 0x03) + 1;
    let num_sps = data[5] & 0x1f;
    let mut pos = 6;
    let mut out = Vec::new();
    for _ in 0..num_sps {
        if pos + 2 > data.len() {
            return Err("avcC SPS truncated".into());
        }
        let len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;
        if pos + len > data.len() {
            return Err("avcC SPS out of bounds".into());
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[pos..pos + len]);
        pos += len;
    }
    if pos >= data.len() {
        return Err("avcC missing PPS count".into());
    }
    let num_pps = data[pos];
    pos += 1;
    for _ in 0..num_pps {
        if pos + 2 > data.len() {
            return Err("avcC PPS truncated".into());
        }
        let len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;
        if pos + len > data.len() {
            return Err("avcC PPS out of bounds".into());
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[pos..pos + len]);
        pos += len;
    }
    Ok((out, length_size))
}

/// Converts one AVCC sample (length-prefixed NALs) to Annex-B.
pub fn avcc_sample_to_annexb(sample: &[u8], length_size: u8) -> Result<Vec<u8>, String> {
    let ls = length_size as usize;
    let mut out = Vec::with_capacity(sample.len() + 16);
    let mut pos = 0;
    while pos + ls <= sample.len() {
        let len = match ls {
            1 => sample[pos] as usize,
            2 => u16::from_be_bytes([sample[pos], sample[pos + 1]]) as usize,
            4 => u32::from_be_bytes([
                sample[pos],
                sample[pos + 1],
                sample[pos + 2],
                sample[pos + 3],
            ]) as usize,
            _ => return Err(format!("bad NAL length size {ls}")),
        };
        pos += ls;
        if len == 0 {
            continue;
        }
        if pos + len > sample.len() {
            return Err("AVCC NAL out of bounds".into());
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&sample[pos..pos + len]);
        pos += len;
    }
    Ok(out)
}

/// Splits an Annex-B stream into NAL units (without start codes).
pub fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut i = 0;
    let mut starts = Vec::new();
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                starts.push((i, 3));
                i += 3;
                continue;
            }
            if i + 4 <= data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                starts.push((i, 4));
                i += 4;
                continue;
            }
        }
        i += 1;
    }
    for (idx, (pos, sclen)) in starts.iter().enumerate() {
        let begin = pos + sclen;
        let end = starts.get(idx + 1).map(|(p, _)| *p).unwrap_or(data.len());
        let nal = &data[begin..end];
        // strip trailing zero bytes that belong to the next start code prefix
        let nal = strip_trailing_zeros(nal);
        if !nal.is_empty() {
            nals.push(nal);
        }
    }
    nals
}

fn strip_trailing_zeros(mut s: &[u8]) -> &[u8] {
    while s.last() == Some(&0) {
        s = &s[..s.len() - 1];
    }
    s
}

// --- exp-golomb reader ----------------------------------------------------

struct BitReader<'a> {
    data: &'a [u8],
    bitpos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader { data, bitpos: 0 }
    }
    fn bit(&mut self) -> Result<u32, String> {
        let byte = self
            .data
            .get(self.bitpos / 8)
            .ok_or_else(|| "bit reader out of range".to_string())?;
        let b = (byte >> (7 - (self.bitpos % 8))) & 1;
        self.bitpos += 1;
        Ok(b as u32)
    }
    fn bits(&mut self, n: usize) -> Result<u32, String> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Ok(v)
    }
    fn ue(&mut self) -> Result<u32, String> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err("exp-golomb too long".into());
            }
        }
        if zeros == 0 {
            return Ok(0);
        }
        let v = self.bits(zeros)?;
        Ok((1 << zeros) - 1 + v)
    }
    #[allow(dead_code)]
    fn se(&mut self) -> Result<i32, String> {
        let k = self.ue()?;
        Ok(if k % 2 == 0 {
            -((k / 2) as i32)
        } else {
            k.div_ceil(2) as i32
        })
    }
}

fn unescape_rbsp(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// Parses an SPS NAL (with or without start code) -> (width, height).
pub fn parse_sps(nal: &[u8]) -> Result<(u32, u32), String> {
    let nal = if nal.starts_with(&[0, 0, 0, 1]) {
        &nal[4..]
    } else if nal.starts_with(&[0, 0, 1]) {
        &nal[3..]
    } else {
        nal
    };
    if nal.is_empty() {
        return Err("empty SPS".into());
    }
    let nal_type = nal[0] & 0x1f;
    if nal_type != 7 {
        return Err(format!("not an SPS (nal type {nal_type})"));
    }
    let rbsp = unescape_rbsp(&nal[1..]);
    let mut r = BitReader::new(&rbsp);
    let profile = r.bits(8)?;
    let _constraints = r.bits(8)?;
    let _level = r.bits(8)?;
    let _sps_id = r.ue()?;
    let mut chroma_format_idc = 1u32;
    let mut separate_colour_plane = false;
    if matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            separate_colour_plane = r.bit()? == 1;
        }
        let _bit_depth_luma = r.ue()?;
        let _bit_depth_chroma = r.ue()?;
        let _qpprime = r.bit()?;
        if r.bit()? == 1 {
            // seq_scaling_matrix_present: skip scaling lists
            let count = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..count {
                if r.bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let mut last = 8i32;
                    let mut next = 8i32;
                    for _ in 0..size {
                        if next != 0 {
                            let d = r.se()?;
                            next = (last + d + 256) % 256;
                        }
                        last = if next == 0 { last } else { next };
                    }
                }
            }
        }
    }
    let _log2_max_frame_num = r.ue()?;
    let poc_type = r.ue()?;
    if poc_type == 0 {
        let _ = r.ue()?;
    } else if poc_type == 1 {
        let _ = r.bit()?;
        let _ = r.se()?;
        let _ = r.se()?;
        let n = r.ue()?;
        for _ in 0..n {
            let _ = r.se()?;
        }
    }
    let _max_ref = r.ue()?;
    let _gaps = r.bit()?;
    let pic_width_in_mbs = r.ue()? + 1;
    let pic_height_in_map_units = r.ue()? + 1;
    let frame_mbs_only = r.bit()?;
    if frame_mbs_only == 0 {
        let _mb_adaptive = r.bit()?;
    }
    let _direct_8x8 = r.bit()?;
    let mut crop_l = 0u32;
    let mut crop_r = 0u32;
    let mut crop_t = 0u32;
    let mut crop_b = 0u32;
    if r.bit()? == 1 {
        crop_l = r.ue()?;
        crop_r = r.ue()?;
        crop_t = r.ue()?;
        crop_b = r.ue()?;
    }
    let (sub_w, sub_h) = match chroma_format_idc {
        0 => (1u32, 1u32),
        1 => (2, 2), // 4:2:0
        2 => (2, 1), // 4:2:2
        _ => (1, 1), // 4:4:4
    };
    let crop_unit_x = sub_w;
    let crop_unit_y = sub_h * (2 - frame_mbs_only);
    let _ = separate_colour_plane;
    let width = pic_width_in_mbs * 16 - (crop_l + crop_r) * crop_unit_x;
    let height =
        (2 - frame_mbs_only) * pic_height_in_map_units * 16 - (crop_t + crop_b) * crop_unit_y;
    Ok((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annexb_split_handles_3_and_4_byte_codes() {
        let data = [
            0, 0, 0, 1, 0x67, 0x42, 0, 0, 1, 0x68, 0xee, 0, 0, 0, 1, 0x65, 0x88,
        ];
        let nals = split_annexb(&data);
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0], &[0x67, 0x42]);
        assert_eq!(nals[1], &[0x68, 0xee]);
        assert_eq!(nals[2], &[0x65, 0x88]);
    }

    #[test]
    fn avcc_roundtrip() {
        // avcC: v1, profile 0x42, compat 0, level 0x1e, length_size_minus1=3 (4), 1 SPS, 1 PPS
        let sps = [0x67u8, 0x42, 0x00, 0x1e, 0xa9];
        let pps = [0x68u8, 0xee, 0x3c];
        let mut avcc = vec![1, 0x42, 0x00, 0x1e, 0xff, 0xe1];
        avcc.extend_from_slice(&u16::to_be_bytes(sps.len() as u16));
        avcc.extend_from_slice(&sps);
        avcc.push(1);
        avcc.extend_from_slice(&u16::to_be_bytes(pps.len() as u16));
        avcc.extend_from_slice(&pps);
        let (annexb, ls) = parse_avcc(&avcc).unwrap();
        assert_eq!(ls, 4);
        assert_eq!(
            annexb,
            vec![
                0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1e, 0xa9, // SPS
                0, 0, 0, 1, 0x68, 0xee, 0x3c, // PPS
            ]
        );
        // a sample with two 4-byte-length NALs converts back
        let mut sample = Vec::new();
        sample.extend_from_slice(&u32::to_be_bytes(3));
        sample.extend_from_slice(&[0x65, 0x88, 0x84]);
        sample.extend_from_slice(&u32::to_be_bytes(2));
        sample.extend_from_slice(&[0x41, 0x9a]);
        let out = avcc_sample_to_annexb(&sample, 4).unwrap();
        assert_eq!(
            out,
            vec![0, 0, 0, 1, 0x65, 0x88, 0x84, 0, 0, 0, 1, 0x41, 0x9a]
        );
    }

    #[test]
    fn sps_parses_real_h264() {
        // SPS extracted from the ffmpeg-generated fixture tests/fixtures/clip_h264.mp4
        // (320x240 baseline)
        let sps = [
            0x67u8, 0x42, 0xc0, 0x0b, 0xd9, 0x01, 0x41, 0xfb, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00,
            0x10, 0x00, 0x00, 0x03, 0x01, 0x40, 0xf1, 0x42, 0xa4, 0x80,
        ];
        let (w, h) = parse_sps(&sps).unwrap();
        assert_eq!((w, h), (320, 240));
    }

    #[test]
    fn sps_rejects_non_sps() {
        assert!(parse_sps(&[0x68, 0xee]).is_err());
        assert!(parse_sps(&[]).is_err());
    }

    #[test]
    fn sps_1080p() {
        // SPS extracted from tests/fixtures/clip_h264_1080p.mp4 (1920x1080 high)
        let sps = [
            0x67u8, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x40, 0x78, 0x02, 0x27, 0xe5, 0xc0, 0x44, 0x00,
            0x00, 0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0x50, 0x3c, 0x60, 0xc6, 0x58,
        ];
        let (w, h) = parse_sps(&sps).unwrap();
        assert_eq!((w, h), (1920, 1080));
    }
}
