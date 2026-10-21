//! Screenshots through Wayland protocols, for compositors without a
//! ScreenCast portal session to capture from: ext-image-copy-capture-v1
//! with an output source where the compositor offers it, otherwise
//! wlr-screencopy-unstable-v1. Either way the compositor copies one frame
//! of the output into a shared-memory buffer (a memfd) we supply.

use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use pipewire::spa::param::video::VideoFormat;
use wayland_client::protocol::{wl_buffer, wl_output, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1::ExtImageCaptureSourceV1;
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::Options,
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::{
    self, ZwlrScreencopyFrameV1,
};

use crate::capture::CapturedFrame;
use crate::wayland::{Monitor, State, Wayland};

/// How long each step of a capture may take.
const STEP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// What the compositor has said about one capture so far.
#[derive(Default)]
struct Progress {
    /// Buffer constraints: size, the shm formats on offer, and (for
    /// screencopy) the stride.
    size: Option<(u32, u32)>,
    formats: Vec<wl_shm::Format>,
    stride: Option<u32>,
    /// The constraints are complete.
    constraints_done: bool,
    transform: Option<wl_output::Transform>,
    y_invert: bool,
    /// How the capture ended: Ok when the buffer holds the frame.
    result: Option<Result<(), String>>,
}

#[derive(Default)]
struct Pending {
    progress: Mutex<Progress>,
    changed: tokio::sync::Notify,
}

impl Pending {
    fn update(&self, f: impl FnOnce(&mut Progress)) {
        f(&mut self.progress.lock().unwrap());
        self.changed.notify_waiters();
    }

    /// Wait until `ready` holds, for `what`.
    async fn wait(&self, what: &str, ready: impl Fn(&Progress) -> bool) -> Result<()> {
        let wait = async {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                {
                    let p = self.progress.lock().unwrap();
                    if ready(&p) {
                        return Ok(());
                    }
                    if let Some(Err(why)) = &p.result {
                        return Err(anyhow!("capture failed: {why}"));
                    }
                }
                changed.await;
            }
        };
        tokio::time::timeout(STEP_TIMEOUT, wait)
            .await
            .map_err(|_| anyhow!("the compositor didn't {what} within {STEP_TIMEOUT:?}"))?
    }
}

/// Capture one frame of `output`, as it's displayed.
pub(crate) async fn capture_output(
    wayland: &Wayland,
    monitor: &Monitor,
    output: &wl_output::WlOutput,
) -> Result<CapturedFrame> {
    let g = wayland.globals();
    if let (Some(manager), Some(sources)) = (&g.image_copy, &g.output_source) {
        let pending = Arc::new(Pending::default());
        let source = sources.create_source(output, wayland.qh(), ());
        let session = manager.create_session(
            &source,
            Options::PaintCursors,
            wayland.qh(),
            pending.clone(),
        );
        let result = capture_session(wayland, &session, &pending).await;
        session.destroy();
        source.destroy();
        wayland.flush()?;
        return result;
    }
    if let Some(manager) = &g.screencopy {
        let pending = Arc::new(Pending::default());
        let frame = manager.capture_output(1, output, wayland.qh(), pending.clone());
        let result = capture_screencopy(
            wayland,
            &frame,
            &pending,
            manager.version(),
            monitor.transform,
        )
        .await;
        frame.destroy();
        wayland.flush()?;
        return result;
    }
    Err(anyhow!(
        "the compositor offers no way to capture the screen (ext-image-copy-capture-v1 or \
         wlr-screencopy-unstable-v1)"
    ))
}

async fn capture_session(
    wayland: &Wayland,
    session: &ExtImageCopyCaptureSessionV1,
    pending: &Arc<Pending>,
) -> Result<CapturedFrame> {
    wayland.flush()?;
    pending
        .wait("describe the capture buffer", |p| p.constraints_done)
        .await?;
    let (size, format) = {
        let p = pending.progress.lock().unwrap();
        let size = p
            .size
            .ok_or_else(|| anyhow!("the compositor gave no buffer size"))?;
        (size, pick_format(&p.formats)?)
    };
    let stride = size.0 * 4;
    let buffer = ShmBuffer::new(wayland, size, stride, format)?;
    let frame = session.create_frame(wayland.qh(), pending.clone());
    frame.attach_buffer(&buffer.buffer);
    frame.damage_buffer(0, 0, size.0 as i32, size.1 as i32);
    frame.capture();
    wayland.flush()?;
    let done = pending
        .wait("deliver the frame", |p| matches!(p.result, Some(Ok(()))))
        .await;
    frame.destroy();
    done?;
    let transform = pending.progress.lock().unwrap().transform;
    buffer.read(format, false, transform)
}

async fn capture_screencopy(
    wayland: &Wayland,
    frame: &ZwlrScreencopyFrameV1,
    pending: &Arc<Pending>,
    version: u32,
    transform: wl_output::Transform,
) -> Result<CapturedFrame> {
    wayland.flush()?;
    // Version 3 ends the buffer descriptions with buffer_done; before it
    // the one shm buffer event is all there is.
    pending
        .wait("describe the capture buffer", |p| {
            if version >= 3 {
                p.constraints_done
            } else {
                p.stride.is_some()
            }
        })
        .await?;
    let (size, stride, format) = {
        let p = pending.progress.lock().unwrap();
        let size = p
            .size
            .ok_or_else(|| anyhow!("the compositor offered no shm buffer"))?;
        let stride = p
            .stride
            .ok_or_else(|| anyhow!("the compositor offered no shm buffer"))?;
        (size, stride, pick_format(&p.formats)?)
    };
    let buffer = ShmBuffer::new(wayland, size, stride, format)?;
    frame.copy(&buffer.buffer);
    wayland.flush()?;
    pending
        .wait("deliver the frame", |p| matches!(p.result, Some(Ok(()))))
        .await?;
    let y_invert = pending.progress.lock().unwrap().y_invert;
    // wlr-screencopy copies the output's framebuffer, which the output's
    // transform turns for display.
    buffer.read(format, y_invert, Some(transform))
}

/// The first offered format we can turn into RGBA.
fn pick_format(formats: &[wl_shm::Format]) -> Result<wl_shm::Format> {
    formats
        .iter()
        .copied()
        .find(|f| video_format(*f).is_some())
        .ok_or_else(|| anyhow!("the compositor offered no 32-bit RGB buffer format ({formats:?})"))
}

/// The byte layout of a wl_shm format: they're little-endian packed,
/// so ARGB8888 is B, G, R, A in memory.
fn video_format(f: wl_shm::Format) -> Option<VideoFormat> {
    Some(match f {
        wl_shm::Format::Argb8888 => VideoFormat::BGRA,
        wl_shm::Format::Xrgb8888 => VideoFormat::BGRx,
        wl_shm::Format::Abgr8888 => VideoFormat::RGBA,
        wl_shm::Format::Xbgr8888 => VideoFormat::RGBx,
        _ => return None,
    })
}

/// A wl_shm buffer backed by a memfd of our own.
struct ShmBuffer {
    file: std::fs::File,
    pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
    size: (u32, u32),
    stride: u32,
}

impl ShmBuffer {
    fn new(
        wayland: &Wayland,
        size: (u32, u32),
        stride: u32,
        format: wl_shm::Format,
    ) -> Result<ShmBuffer> {
        let shm = wayland
            .globals()
            .shm
            .as_ref()
            .ok_or_else(|| anyhow!("the compositor has no wl_shm"))?;
        let len = stride as u64 * size.1 as u64;
        let len_i32 =
            i32::try_from(len).map_err(|_| anyhow!("a {}x{} frame is too big", size.0, size.1))?;
        // SAFETY: memfd_create takes a NUL-terminated name and flags, and
        // returns a new fd we immediately take ownership of.
        let raw = unsafe { libc::memfd_create(c"waytool".as_ptr(), libc::MFD_CLOEXEC) };
        if raw < 0 {
            return Err(anyhow!("memfd_create: {}", std::io::Error::last_os_error()));
        }
        let file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(raw) });
        file.set_len(len).context("size the capture buffer")?;
        let pool = shm.create_pool(file.as_fd(), len_i32, wayland.qh(), ());
        let buffer = pool.create_buffer(
            0,
            size.0 as i32,
            size.1 as i32,
            stride as i32,
            format,
            wayland.qh(),
            (),
        );
        Ok(ShmBuffer {
            file,
            pool,
            buffer,
            size,
            stride,
        })
    }

    /// The captured frame as RGBA, as displayed: `transform` is what
    /// was applied to the buffer contents, so it's undone.
    fn read(
        self,
        format: wl_shm::Format,
        y_invert: bool,
        transform: Option<wl_output::Transform>,
    ) -> Result<CapturedFrame> {
        let (w, h) = self.size;
        let mut data = vec![0; self.stride as usize * h as usize];
        self.file
            .read_exact_at(&mut data, 0)
            .context("read the captured frame")?;
        if y_invert {
            let rows: Vec<&[u8]> = data.chunks(self.stride as usize).rev().collect();
            data = rows.concat();
        }
        let video = video_format(format).expect("picked by pick_format");
        let rgba = crate::capture::convert_to_rgba(&data, 0, w, h, self.stride as i32, video)
            .ok_or_else(|| anyhow!("the captured buffer doesn't hold a {w}x{h} frame"))?;
        Ok(untransform(
            CapturedFrame {
                width: w,
                height: h,
                rgba,
            },
            transform.unwrap_or(wl_output::Transform::Normal),
        ))
    }
}

/// Undo `transform` (wl_output.transform: a counter-clockwise rotation,
/// after a flip around the vertical axis for the flipped ones), which
/// was applied to an image to make `buffer`.
fn untransform(buffer: CapturedFrame, transform: wl_output::Transform) -> CapturedFrame {
    use wl_output::Transform as T;
    if transform == T::Normal {
        return buffer;
    }
    let (bw, bh) = (buffer.width as usize, buffer.height as usize);
    let quarter = matches!(transform, T::_90 | T::_270 | T::Flipped90 | T::Flipped270);
    let (w, h) = if quarter { (bh, bw) } else { (bw, bh) };
    // Where image pixel (x, y) went in the buffer.
    let source = |x: usize, y: usize| -> (usize, usize) {
        match transform {
            T::_90 => (y, w - 1 - x),
            T::_180 => (w - 1 - x, h - 1 - y),
            T::_270 => (h - 1 - y, x),
            T::Flipped => (w - 1 - x, y),
            T::Flipped90 => (y, x),
            T::Flipped180 => (x, h - 1 - y),
            T::Flipped270 => (h - 1 - y, w - 1 - x),
            _ => (x, y),
        }
    };
    let mut rgba = vec![0; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let (sx, sy) = source(x, y);
            let from = (sy * bw + sx) * 4;
            rgba[(y * w + x) * 4..][..4].copy_from_slice(&buffer.rgba[from..from + 4]);
        }
    }
    CapturedFrame {
        width: w as u32,
        height: h as u32,
        rgba,
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, Arc<Pending>> for State {
    fn event(
        _: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        pending: &Arc<Pending>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_session_v1::Event;
        pending.update(|p| match event {
            Event::BufferSize { width, height } => p.size = Some((width, height)),
            Event::ShmFormat {
                format: WEnum::Value(f),
            } => p.formats.push(f),
            Event::Done => p.constraints_done = true,
            Event::Stopped => {
                p.result
                    .get_or_insert(Err("the compositor stopped the capture session".into()));
            }
            _ => {}
        });
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, Arc<Pending>> for State {
    fn event(
        _: &mut Self,
        _: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        pending: &Arc<Pending>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_frame_v1::Event;
        pending.update(|p| match event {
            Event::Transform {
                transform: WEnum::Value(t),
            } => p.transform = Some(t),
            Event::Ready => p.result = Some(Ok(())),
            Event::Failed { reason } => {
                p.result = Some(Err(format!("the compositor couldn't capture ({reason:?})")))
            }
            _ => {}
        });
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, Arc<Pending>> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        pending: &Arc<Pending>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event;
        pending.update(|p| match event {
            Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } => {
                p.size = Some((width, height));
                p.stride = Some(stride);
                p.formats = vec![format];
            }
            Event::Flags {
                flags: WEnum::Value(flags),
            } => p.y_invert = flags.contains(zwlr_screencopy_frame_v1::Flags::YInvert),
            Event::BufferDone => p.constraints_done = true,
            Event::Ready { .. } => p.result = Some(Ok(())),
            Event::Failed => p.result = Some(Err("the compositor couldn't capture".into())),
            _ => {}
        });
    }
}

macro_rules! no_events {
    ($($proxy:ty),* $(,)?) => {$(
        impl Dispatch<$proxy, ()> for State {
            fn event(
                _: &mut Self,
                _: &$proxy,
                _: <$proxy as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    )*};
}

no_events!(
    ExtImageCaptureSourceV1,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
);

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: u32, height: u32, pixels: &[u8]) -> CapturedFrame {
        CapturedFrame {
            width,
            height,
            rgba: pixels.iter().flat_map(|&p| [p, p, p, 255]).collect(),
        }
    }

    #[test]
    fn untransform_turns_buffers_back() {
        use wl_output::Transform as T;
        // Image [A B], one row. Turned 90° counter-clockwise it's a
        // column with B above A; turned 270°, A above B.
        let (a, b) = (1, 2);
        let image = frame(2, 1, &[a, b]);
        let undone = untransform(frame(1, 2, &[b, a]), T::_90);
        assert_eq!((undone.width, undone.height), (2, 1));
        assert_eq!(undone.rgba, image.rgba);
        assert_eq!(untransform(frame(1, 2, &[a, b]), T::_270).rgba, image.rgba);
        assert_eq!(untransform(frame(2, 1, &[b, a]), T::_180).rgba, image.rgba);
        assert_eq!(
            untransform(frame(2, 1, &[b, a]), T::Flipped).rgba,
            image.rgba
        );
    }
}
