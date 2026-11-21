//! V4L2 memory-to-memory hardware H.264 decoder (Linux). This is what makes
//! video playback fast on a Raspberry Pi without mpv/vlc: the SoC's decoder
//! (bcm2835-codec on Pi <= 4, rpivid on Pi 5, or any other stateful M2M
//! decoder) turns an Annex-B H.264 elementary stream into NV12/YUV420 frames
//! via raw kernel ioctls. No libavcodec, no GPU stack.

use crate::frame::{Chroma, YuvFrame};
use crate::mp4::Mp4Video;
use edgefirst_v4l2::device::{self, Device};
use edgefirst_v4l2::events;
use edgefirst_v4l2::m2m::M2m;
use edgefirst_v4l2::queue::{BufType, Mapping, Memory, Plane, Queue};
use edgefirst_v4l2::uapi::*;
use std::os::fd::{AsFd, AsRawFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

const FOURCC_H264: u32 = fourcc(b'H', b'2', b'6', b'4');
const FOURCC_NV12: u32 = fourcc(b'N', b'V', b'1', b'2');
const FOURCC_YUV420: u32 = fourcc(b'Y', b'U', b'1', b'2');

// VIDIOC_DECODER_CMD — verified against /usr/include/linux/videodev2.h
const fn ioc(dir: u32, ty: u8, nr: u8, size: u32) -> libc::c_ulong {
    ((dir << 30) | (size << 16) | ((ty as u32) << 8) | nr as u32) as libc::c_ulong
}
const IOCTL_DECODER_CMD: libc::c_ulong = ioc(2 | 1, b'V', 96, size_of::<v4l2_decoder_cmd>() as u32);

pub fn decoder_cmd_stop(fd: std::os::fd::RawFd) -> Result<(), String> {
    let mut cmd = v4l2_decoder_cmd {
        cmd: V4L2_DEC_CMD_STOP,
        flags: 0,
        u: [0; 8],
    };
    let r = unsafe { libc::ioctl(fd, IOCTL_DECODER_CMD, &mut cmd) };
    if r < 0 {
        Err(format!(
            "VIDIOC_DECODER_CMD STOP: {}",
            std::io::Error::last_os_error()
        ))
    } else {
        Ok(())
    }
}

/// Finds a /dev/video* node that is an M2M device accepting H264 on the
/// output queue and producing NV12 (or YUV420) on the capture queue.
pub fn find_h264_decoder() -> Option<PathBuf> {
    let devs = device::enumerate().ok()?;
    for info in devs {
        let Ok(caps) = info.capabilities else {
            continue;
        };
        if !caps.is_m2m() || !caps.has_streaming() {
            continue;
        }
        let (Some(out_type), Some(cap_type)) = (caps.output_buf_type(), caps.capture_buf_type())
        else {
            continue;
        };
        let Ok(dev) = Device::open(&info.path) else {
            continue;
        };
        let out_ok = dev
            .formats(out_type)
            .map(|fs| fs.iter().any(|f| f.fourcc == FOURCC_H264))
            .unwrap_or(false);
        let cap_ok = dev
            .formats(cap_type)
            .map(|fs| {
                fs.iter()
                    .any(|f| f.fourcc == FOURCC_NV12 || f.fourcc == FOURCC_YUV420)
            })
            .unwrap_or(false);
        if out_ok && cap_ok {
            return Some(info.path);
        }
    }
    None
}

pub struct DecodedFrame {
    pub yuv: YuvFrame,
    pub pts: Duration,
    pub keyframe: bool,
}

pub enum DecEvent {
    Frame(DecodedFrame),
    Ended,
}

/// Decodes `mp4` (already parsed) from `data`, sending frames to `emit`.
/// Returns Err on decoder errors. `should_stop` is polled between samples so
/// the player can switch items mid-playback.
pub fn decode_mp4_h264(
    dev_path: &Path,
    mp4: &Mp4Video,
    data: &[u8],
    mut emit: impl FnMut(DecEvent),
    should_stop: impl Fn() -> bool,
) -> Result<(), String> {
    let dev = Device::open(dev_path).map_err(|e| format!("open {}: {e}", dev_path.display()))?;
    let m2m = M2m::new(&dev, true).map_err(|e| format!("M2m::new: {e}"))?;
    let out_type = BufType::VideoOutputMplane;
    let cap_type = BufType::VideoCaptureMplane;

    // output (compressed) format: H264 at coded size
    let coded_w = mp4.width.max(16);
    let coded_h = mp4.height.max(16);
    let mut of = v4l2_format {
        type_: out_type.raw(),
        ..Default::default()
    };
    {
        let p = unsafe { of.pix_mp() };
        p.width = coded_w;
        p.height = coded_h;
        p.pixelformat = FOURCC_H264;
        p.field = V4L2_FIELD_NONE;
        p.num_planes = 1;
        p.plane_fmt[0].sizeimage = (coded_w * coded_h).max(1024 * 1024);
        p.plane_fmt[0].bytesperline = 0;
    }
    dev.set_format(&mut of)
        .map_err(|e| format!("S_FMT output: {e}"))?;

    // capture (raw) format: NV12 preferred, driver may adjust
    let mut cf = v4l2_format {
        type_: cap_type.raw(),
        ..Default::default()
    };
    {
        let p = unsafe { cf.pix_mp() };
        p.width = coded_w;
        p.height = coded_h;
        p.pixelformat = FOURCC_NV12;
        p.field = V4L2_FIELD_NONE;
        p.num_planes = 2;
    }
    let _ = dev.set_format(&mut cf); // best effort; SOURCE_CHANGE corrects it

    let (mut oq, mut cq) = m2m.into_queues();
    oq.request(Memory::Mmap, 6)
        .map_err(|e| format!("REQBUFS output: {e}"))?;
    cq.request(Memory::Mmap, 8)
        .map_err(|e| format!("REQBUFS capture: {e}"))?;
    let out_maps: Vec<Vec<Mapping>> = (0..oq.len() as u32)
        .map(|i| oq.map(i).map_err(|e| format!("map output {i}: {e}")))
        .collect::<Result<_, _>>()?;
    let mut cap_maps: Vec<Vec<Mapping>> = (0..cq.len() as u32)
        .map(|i| cq.map(i).map_err(|e| format!("map capture {i}: {e}")))
        .collect::<Result<_, _>>()?;
    let out_plane_len = oq
        .query(0)
        .ok()
        .and_then(|b| b.planes.first().map(|p| p.length as usize))
        .unwrap_or(0);
    let cap_num_planes = cq.num_planes().max(1);

    // subscribe to decoder events before streaming
    let _ = events::subscribe(&dev, V4L2_EVENT_SOURCE_CHANGE, 0, 0);
    let _ = events::subscribe(&dev, V4L2_EVENT_EOS, 0, 0);

    // queue all capture buffers, then start both queues
    for i in 0..cq.len() as u32 {
        let planes: Vec<Plane> = (0..cap_num_planes).map(|_| Plane::mmap()).collect();
        cq.enqueue(i, &planes, None)
            .map_err(|e| format!("QBUF capture {i}: {e}"))?;
    }
    oq.stream_on()
        .map_err(|e| format!("STREAMON output: {e}"))?;
    cq.stream_on()
        .map_err(|e| format!("STREAMON capture: {e}"))?;

    // current capture geometry (updated on SOURCE_CHANGE)
    let mut cap_fmt = dev
        .format(cap_type)
        .map_err(|e| format!("G_FMT capture: {e}"))?;

    let result = feed_and_drain(
        &dev,
        &mut oq,
        &mut cq,
        &mut cap_maps,
        &out_maps,
        out_plane_len,
        cap_num_planes,
        &mut cap_fmt,
        mp4,
        data,
        &mut emit,
        &should_stop,
    );

    let _ = cq.stream_off();
    let _ = oq.stream_off();
    result
}

#[allow(clippy::too_many_arguments)]
fn feed_and_drain(
    dev: &Device,
    oq: &mut Queue,
    cq: &mut Queue,
    cap_maps: &mut [Vec<Mapping>],
    out_maps: &[Vec<Mapping>],
    out_plane_len: usize,
    cap_num_planes: usize,
    cap_fmt: &mut v4l2_format,
    mp4: &Mp4Video,
    data: &[u8],
    emit: &mut impl FnMut(DecEvent),
    should_stop: &impl Fn() -> bool,
) -> Result<(), String> {
    // Build the full Annex-B elementary stream: SPS/PPS first, then samples.
    let mut es = mp4.avc_prefix.clone();
    let mut sample_ranges = Vec::with_capacity(mp4.samples.len());
    for s in &mp4.samples {
        let begin = s.offset as usize;
        let end = begin + s.size as usize;
        if end > data.len() {
            return Err("sample outside file".into());
        }
        match crate::h264::avcc_sample_to_annexb(&data[begin..end], mp4.nal_length_size) {
            Ok(annexb) => {
                sample_ranges.push((es.len(), annexb.len(), s.pts, s.is_keyframe));
                es.extend_from_slice(&annexb);
            }
            Err(e) => {
                crate::log!("skipping undecodable sample: {e}");
            }
        }
    }

    let timescale = mp4.timescale as u64;
    let mut fed = 0usize; // bytes of `es` already queued
    let mut sample_i = 0usize;
    let mut eos_sent = false;

    // Feed loop: split `es` across output buffers as they become free.
    while fed < es.len() || !eos_sent {
        if should_stop() {
            return Ok(());
        }
        // drain whatever capture buffers are ready
        drain_capture(cq, cap_maps, cap_num_planes, cap_fmt, emit, dev)?;
        if fed >= es.len() {
            if !eos_sent {
                eos_sent = true;
                decoder_cmd_stop(dev.as_fd().as_raw_fd())?;
            }
            // keep draining until EOS event / no more buffers
            if !drain_until_eos(cq, cap_maps, cap_num_planes, cap_fmt, emit, dev)? {
                break;
            }
            continue;
        }
        // wait for a free output buffer, then fill it
        if !oq.wait(Some(Duration::from_millis(500))).unwrap_or(false) {
            continue;
        }
        let dq = match oq.dequeue() {
            Ok(Some(d)) => d,
            Ok(None) => continue,
            Err(e) => return Err(format!("DQBUF output: {e}")),
        };
        let idx = dq.index as usize;
        if idx >= out_maps.len() {
            continue;
        }
        let map = &out_maps[idx];
        let cap = out_plane_len.min(map.first().map(|m| m.len()).unwrap_or(0));
        if cap == 0 {
            return Err("zero-length output buffer".into());
        }
        let chunk_len = (es.len() - fed).min(cap);
        unsafe {
            let dst = map[0].as_ptr();
            std::ptr::copy_nonoverlapping(es.as_ptr().add(fed), dst, chunk_len);
        }
        // pts of the sample this chunk belongs to
        let pts = sample_ranges
            .iter()
            .take(sample_i + 1)
            .rev()
            .find(|(start, len, _, _)| fed >= *start && fed < start + len)
            .map(|(_, _, pts, _)| *pts)
            .unwrap_or(0);
        let _ = sample_i;
        let pts_dur = Duration::from_nanos(pts.saturating_mul(1_000_000_000) / timescale.max(1));
        oq.enqueue(
            dq.index,
            &[Plane::Mmap {
                bytesused: chunk_len as u32,
            }],
            Some(pts_dur),
        )
        .map_err(|e| format!("QBUF output: {e}"))?;
        fed += chunk_len;
        while sample_i < sample_ranges.len()
            && fed >= sample_ranges[sample_i].0 + sample_ranges[sample_i].1
        {
            sample_i += 1;
        }
    }
    emit(DecEvent::Ended);
    Ok(())
}

fn drain_capture(
    cq: &Queue,
    cap_maps: &[Vec<Mapping>],
    cap_num_planes: usize,
    cap_fmt: &v4l2_format,
    emit: &mut impl FnMut(DecEvent),
    dev: &Device,
) -> Result<(), String> {
    loop {
        // events first (SOURCE_CHANGE may alter capture geometry)
        while let Ok(Some(ev)) = events::dequeue(dev) {
            if ev.kind() == V4L2_EVENT_SOURCE_CHANGE {
                let _ = ev.source_change();
            }
        }
        match cq.dequeue() {
            Ok(Some(dq)) => {
                if let Some(frame) = capture_to_yuv(&dq, cap_maps, cap_num_planes, cap_fmt) {
                    emit(DecEvent::Frame(DecodedFrame {
                        yuv: frame.0,
                        pts: dq.timestamp,
                        keyframe: dq.flags.is_keyframe(),
                    }));
                }
                let planes: Vec<Plane> = (0..cap_num_planes).map(|_| Plane::mmap()).collect();
                let _ = cq.enqueue(dq.index, &planes, None);
            }
            Ok(None) => return Ok(()),
            Err(e) => return Err(format!("DQBUF capture: {e}")),
        }
    }
}

/// Drains after EOS. Returns Ok(true) if an EOS event was seen.
fn drain_until_eos(
    cq: &Queue,
    cap_maps: &[Vec<Mapping>],
    cap_num_planes: usize,
    cap_fmt: &v4l2_format,
    emit: &mut impl FnMut(DecEvent),
    dev: &Device,
) -> Result<bool, String> {
    let mut saw_eos = false;
    for _ in 0..64 {
        while let Ok(Some(ev)) = events::dequeue(dev) {
            if ev.kind() == V4L2_EVENT_EOS {
                saw_eos = true;
            }
        }
        if saw_eos {
            return Ok(true);
        }
        match cq.dequeue() {
            Ok(Some(dq)) => {
                if let Some(frame) = capture_to_yuv(&dq, cap_maps, cap_num_planes, cap_fmt) {
                    emit(DecEvent::Frame(DecodedFrame {
                        yuv: frame.0,
                        pts: dq.timestamp,
                        keyframe: dq.flags.is_keyframe(),
                    }));
                }
            }
            Ok(None) => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) if format!("{e}").contains("end of stream") => return Ok(true),
            Err(e) => return Err(format!("DQBUF capture drain: {e}")),
        }
    }
    Ok(saw_eos)
}

/// Copies a dequeued capture buffer into a tightly packed YuvFrame.
/// Returns None for empty/error buffers.
fn capture_to_yuv(
    dq: &edgefirst_v4l2::queue::Dequeued,
    cap_maps: &[Vec<Mapping>],
    _cap_num_planes: usize,
    cap_fmt: &v4l2_format,
) -> Option<(YuvFrame,)> {
    if dq.flags.is_error() {
        return None;
    }
    let idx = dq.index as usize;
    let maps = cap_maps.get(idx)?;
    let p = unsafe { &*(cap_fmt.fmt.as_ptr() as *const v4l2_pix_format_mplane) };
    let (w, h) = (p.width as usize, p.height as usize);
    if w == 0 || h == 0 || maps.is_empty() {
        return None;
    }
    let planes = dq.planes();
    let fourcc = p.pixelformat;
    let y_stride = p.plane_fmt[0].bytesperline.max(w as u32) as usize;
    let y_used = planes.first().map(|u| u.bytesused as usize).unwrap_or(0);
    if y_used == 0 {
        return None;
    }
    let mut y = vec![0u8; w * h];
    unsafe {
        let src = maps[0].as_ptr();
        for row in 0..h {
            let s = src.add(row * y_stride);
            std::ptr::copy_nonoverlapping(s, y.as_mut_ptr().add(row * w), w);
        }
    }
    let chroma = if fourcc == FOURCC_NV12 && maps.len() >= 2 {
        let cw = w / 2;
        let ch = h / 2;
        let uv_stride = p.plane_fmt[1].bytesperline.max((cw * 2) as u32) as usize;
        let mut uv = vec![0u8; cw * ch * 2];
        unsafe {
            let src = maps[1].as_ptr();
            for row in 0..ch {
                let s = src.add(row * uv_stride);
                std::ptr::copy_nonoverlapping(s, uv.as_mut_ptr().add(row * cw * 2), cw * 2);
            }
        }
        Chroma::Nv12(uv)
    } else {
        // planar I420 fallback (3 planes) or unknown: treat missing as grey
        let cw = w / 2;
        let ch = h / 2;
        let (u, v) = if fourcc == FOURCC_YUV420 && maps.len() >= 3 {
            let mut u = vec![0u8; cw * ch];
            let mut v = vec![0u8; cw * ch];
            unsafe {
                let us = p.plane_fmt[1].bytesperline.max(cw as u32) as usize;
                let vs = p.plane_fmt[2].bytesperline.max(cw as u32) as usize;
                for row in 0..ch {
                    std::ptr::copy_nonoverlapping(
                        maps[1].as_ptr().add(row * us),
                        u.as_mut_ptr().add(row * cw),
                        cw,
                    );
                    std::ptr::copy_nonoverlapping(
                        maps[2].as_ptr().add(row * vs),
                        v.as_mut_ptr().add(row * cw),
                        cw,
                    );
                }
            }
            (u, v)
        } else {
            (vec![128u8; cw * ch], vec![128u8; cw * ch])
        };
        Chroma::I420 { u, v }
    };
    Some((YuvFrame {
        w: w as u32,
        h: h as u32,
        y,
        chroma,
    },))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_cmd_ioctl_matches_kernel_uapi() {
        // verified with gcc against /usr/include/linux/videodev2.h
        assert_eq!(IOCTL_DECODER_CMD, 0xc0485660);
        assert_eq!(size_of::<v4l2_decoder_cmd>(), 72);
        assert_eq!(size_of::<v4l2_format>(), 208);
        assert_eq!(size_of::<v4l2_pix_format_mplane>(), 192);
        assert_eq!(size_of::<v4l2_buffer>(), 88);
        assert_eq!(FOURCC_H264, 0x34363248);
        assert_eq!(FOURCC_NV12, 0x3231564e);
        assert_eq!(FOURCC_YUV420, 0x32315559);
        assert_eq!(V4L2_DEC_CMD_STOP, 1);
        assert_eq!(V4L2_EVENT_EOS, 2);
        assert_eq!(V4L2_EVENT_SOURCE_CHANGE, 5);
    }
}
