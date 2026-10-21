//! Frame capture over PipeWire, from the nodes the ScreenCast portal
//! hands out.

use std::os::fd::OwnedFd;

use anyhow::{Context, Result, anyhow};

/// One captured frame as tightly packed RGBA8, `width * height * 4`
/// bytes.
pub(crate) struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Max wall-clock a single capture may wait for frames before it is
/// abandoned. A healthy capture delivers within well under a second, but
/// nothing guarantees a frame ever arrives: the compositor sends one
/// when it has one to send. Without the watchdog the main loop would
/// block forever, holding the daemon's capture lock.
const CAPTURE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How long to wait for the PipeWire server to list the session's nodes.
const REGISTRY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Capture one frame per node over `fd`, a PipeWire connection of its
/// own from the portal's OpenPipeWireRemote: one stream per node id,
/// waiting until every stream has captured a frame. Returns the frames
/// in `node_ids` order. Runs synchronously on its own thread (PipeWire's
/// main loop is blocking and not Tokio-compatible).
///
/// The connection belongs to this capture alone. When the function
/// returns, `fd` is closed and the server destroys everything the
/// connection created, whichever way the capture ended.
pub(crate) fn pipewire_capture_frames(fd: OwnedFd, node_ids: &[u32]) -> Result<Vec<CapturedFrame>> {
    use pipewire as pw;
    use pw::spa;
    use spa::pod::Pod;
    use std::cell::RefCell;
    use std::rc::Rc;

    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw main_loop")?;
    let context = pw::context::ContextRc::new(&mainloop, None).context("pw context")?;
    let core = context.connect_fd_rc(fd, None).context("pw connect_fd")?;

    // Streams target their node by serial (`target.object`): node ids
    // are reused once freed, serials aren't.
    let serials = node_serials(&mainloop, &core, node_ids)?;

    #[derive(Default)]
    struct Slot {
        format: Option<spa::param::video::VideoInfoRaw>,
        captured: Option<(u32, u32, Vec<u8>)>,
    }
    let slots: Vec<Rc<RefCell<Slot>>> = (0..node_ids.len())
        .map(|_| Rc::new(RefCell::new(Slot::default())))
        .collect();

    let failed: Rc<RefCell<Option<String>>> = Rc::default();

    // Keep the listeners alive: dropping any of them detaches the
    // callbacks before the mainloop has a chance to fire them.
    let mut listeners = Vec::new();
    let mut streams = Vec::new();

    for idx in 0..node_ids.len() {
        let stream = pw::stream::StreamBox::new(
            &core,
            &format!("waytool-screenshot-{idx}"),
            pw::properties::properties! {
                *pw::keys::MEDIA_TYPE => "Video",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Screen",
                "target.object" => serials[idx].to_string(),
                // If the screencast node goes away (a monitor change),
                // the session manager would otherwise relink the stream
                // to the default video source: the webcam.
                "node.dont-fallback" => "true",
            },
        )
        .context("pw stream new")?;

        let main_q = mainloop.clone();
        let main_s = mainloop.clone();
        let failed_s = failed.clone();
        let slot_p = slots[idx].clone();
        let slot_pr = slots[idx].clone();
        let slots_all = slots.clone();

        let listener = stream
            .add_local_listener_with_user_data(())
            .state_changed(move |_, _, old, new| {
                eprintln!("waytool: pw stream[{idx}] {:?} -> {:?}", old, new);
                // An error, or a stream dropped after connecting (its
                // node went away), won't deliver a frame: stop waiting.
                let why = match (&old, &new) {
                    (_, pw::stream::StreamState::Error(e)) => Some(e.clone()),
                    (
                        pw::stream::StreamState::Unconnected,
                        pw::stream::StreamState::Unconnected,
                    ) => None,
                    (_, pw::stream::StreamState::Unconnected) => {
                        Some("the stream was disconnected".into())
                    }
                    _ => None,
                };
                if let Some(why) = why {
                    failed_s
                        .borrow_mut()
                        .get_or_insert(format!("stream {idx}: {why}"));
                    main_s.quit();
                }
            })
            .param_changed(move |_s, _ud, id, param| {
                let Some(param) = param else { return };
                if id != spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let (mt, ms) = match spa::param::format_utils::parse_format(param) {
                    Ok(v) => v,
                    Err(_) => return,
                };
                if mt != spa::param::format::MediaType::Video
                    || ms != spa::param::format::MediaSubtype::Raw
                {
                    return;
                }
                let mut info = spa::param::video::VideoInfoRaw::default();
                if info.parse(param).is_err() {
                    return;
                }
                eprintln!(
                    "waytool: pw stream[{idx}] format {:?} {}x{}",
                    info.format(),
                    info.size().width,
                    info.size().height,
                );
                slot_p.borrow_mut().format = Some(info);
            })
            .process(move |s, _ud| {
                if slot_pr.borrow().captured.is_some() {
                    return;
                }
                let Some(mut buffer) = s.dequeue_buffer() else {
                    return;
                };
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }
                let Some(format) = slot_pr.borrow().format else {
                    return;
                };
                let chunk = datas[0].chunk();
                let (offset, size, stride) = (chunk.offset(), chunk.size(), chunk.stride());
                let pix = format.format();
                let (w, h) = (format.size().width, format.size().height);
                // A zero-size chunk is a metadata-only buffer; wait for
                // the next one.
                if size == 0 {
                    return;
                }
                let Some(data) = datas[0].data() else { return };
                // This runs inside a C callback, where a panic aborts the
                // whole daemon, so a buffer that doesn't hold the frame
                // its format describes is skipped rather than sliced.
                let Some(rgba) = convert_to_rgba(data, offset, w, h, stride, pix) else {
                    eprintln!(
                        "waytool: pw stream[{idx}] skipping a {pix:?} {w}x{h} buffer \
                         that doesn't fit (offset {offset}, stride {stride}, {} bytes)",
                        data.len()
                    );
                    return;
                };
                slot_pr.borrow_mut().captured = Some((w, h, rgba));

                // All slots have captured? Quit the mainloop.
                if slots_all.iter().all(|s| s.borrow().captured.is_some()) {
                    main_q.quit();
                }
            })
            .register()
            .context("pw stream register")?;

        // Format negotiation pod: accept the formats we can decode in
        // convert_to_rgba below. Order matters — we ask for the
        // first-listed format first.
        let obj = pw::spa::pod::object!(
            pw::spa::utils::SpaTypes::ObjectParamFormat,
            pw::spa::param::ParamType::EnumFormat,
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::MediaType,
                Id,
                pw::spa::param::format::MediaType::Video
            ),
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::MediaSubtype,
                Id,
                pw::spa::param::format::MediaSubtype::Raw
            ),
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::VideoFormat,
                Choice,
                Enum,
                Id,
                pw::spa::param::video::VideoFormat::BGRx,
                pw::spa::param::video::VideoFormat::BGRx,
                pw::spa::param::video::VideoFormat::RGBx,
                pw::spa::param::video::VideoFormat::RGBA,
                pw::spa::param::video::VideoFormat::BGRA,
                pw::spa::param::video::VideoFormat::RGB,
                pw::spa::param::video::VideoFormat::BGR
            ),
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::VideoSize,
                Choice,
                Range,
                Rectangle,
                pw::spa::utils::Rectangle {
                    width: 1920,
                    height: 1080
                },
                pw::spa::utils::Rectangle {
                    width: 1,
                    height: 1
                },
                pw::spa::utils::Rectangle {
                    width: 8192,
                    height: 8192
                }
            ),
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::VideoFramerate,
                Choice,
                Range,
                Fraction,
                pw::spa::utils::Fraction { num: 30, denom: 1 },
                pw::spa::utils::Fraction { num: 0, denom: 1 },
                pw::spa::utils::Fraction { num: 240, denom: 1 }
            ),
        );
        let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &pw::spa::pod::Value::Object(obj),
        )
        .map_err(|e| anyhow!("serialize format pod: {e}"))?
        .0
        .into_inner();
        let mut params = [Pod::from_bytes(&values).ok_or_else(|| anyhow!("Pod::from_bytes"))?];

        stream
            .connect(
                spa::utils::Direction::Input,
                None,
                pw::stream::StreamFlags::AUTOCONNECT
                    | pw::stream::StreamFlags::MAP_BUFFERS
                    | pw::stream::StreamFlags::DONT_RECONNECT,
                &mut params,
            )
            .context("pw stream connect")?;

        listeners.push(listener);
        streams.push(stream);
    }

    // Watchdog: a background-thread timer can't call quit() because
    // MainLoopRc isn't Send, but a pw timer *source* runs inside the
    // loop thread, so its callback may quit() freely. Arm a one-shot
    // that fires if the frames don't all arrive within CAPTURE_TIMEOUT.
    let timed_out = Rc::new(std::cell::Cell::new(false));
    let timed_out_cb = timed_out.clone();
    let main_for_timeout = mainloop.clone();
    let timer = mainloop.loop_().add_timer(move |_| {
        timed_out_cb.set(true);
        main_for_timeout.quit();
    });
    let _ = timer.update_timer(Some(CAPTURE_TIMEOUT), None);

    mainloop.run();

    if let Some(why) = failed.borrow_mut().take() {
        return Err(anyhow!("capture failed: {why}"));
    }
    if timed_out.get() {
        let captured = slots
            .iter()
            .filter(|s| s.borrow().captured.is_some())
            .count();
        return Err(anyhow!(
            "capture timed out after {}s: {captured}/{} streams delivered a frame \
             in time",
            CAPTURE_TIMEOUT.as_secs(),
            slots.len()
        ));
    }

    slots
        .iter()
        .enumerate()
        .map(|(idx, slot)| {
            let (width, height, rgba) = slot
                .borrow_mut()
                .captured
                .take()
                .ok_or_else(|| anyhow!("stream {idx}: no frame captured before quit"))?;
            Ok(CapturedFrame {
                width,
                height,
                rgba,
            })
        })
        .collect()
}

/// The `object.serial` of each node in `node_ids`. The portal's
/// connection lists only the session's own nodes, so a node missing
/// here belongs to a stream that has ended.
fn node_serials(
    mainloop: &pipewire::main_loop::MainLoopRc,
    core: &pipewire::core::CoreRc,
    node_ids: &[u32],
) -> Result<Vec<u64>> {
    let nodes = list_nodes(mainloop, core)?;
    node_ids
        .iter()
        .map(|id| {
            nodes.get(id).copied().ok_or_else(|| {
                anyhow!("screencast node {id} is gone (its monitor changed or was removed)")
            })
        })
        .collect()
}

/// Every node the connection can see, by id, with its `object.serial`.
fn list_nodes(
    mainloop: &pipewire::main_loop::MainLoopRc,
    core: &pipewire::core::CoreRc,
) -> Result<std::collections::HashMap<u32, u64>> {
    use pipewire as pw;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    let registry = core.get_registry_rc().context("pw registry")?;
    let found: Rc<RefCell<HashMap<u32, u64>>> = Rc::default();
    let found_g = found.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.type_ != pw::types::ObjectType::Node {
                return;
            }
            let serial = global
                .props
                .and_then(|p| p.get("object.serial"))
                .and_then(|s| s.parse().ok());
            if let Some(serial) = serial {
                found_g.borrow_mut().insert(global.id, serial);
            }
        })
        .register();

    // The registry sends every global before it answers a sync.
    let pending = core.sync(0).context("pw core sync")?;
    let main_q = mainloop.clone();
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq == pending {
                main_q.quit();
            }
        })
        .register();
    let timed_out = Rc::new(std::cell::Cell::new(false));
    let timed_out_cb = timed_out.clone();
    let main_t = mainloop.clone();
    let timer = mainloop.loop_().add_timer(move |_| {
        timed_out_cb.set(true);
        main_t.quit();
    });
    let _ = timer.update_timer(Some(REGISTRY_TIMEOUT), None);
    mainloop.run();
    if timed_out.get() {
        return Err(anyhow!("PipeWire didn't list the session's nodes"));
    }

    Ok(found.take())
}

fn bytes_per_pixel(f: pipewire::spa::param::video::VideoFormat) -> Option<usize> {
    use pipewire::spa::param::video::VideoFormat;
    match f {
        VideoFormat::BGRx | VideoFormat::RGBx | VideoFormat::BGRA | VideoFormat::RGBA => Some(4),
        VideoFormat::BGR | VideoFormat::RGB => Some(3),
        _ => None,
    }
}

/// Convert a `w`x`h` frame of format `f`, starting `offset` bytes into
/// `src` with rows `stride` bytes apart, to tightly packed RGBA8.
/// Returns None for a format we can't decode or a buffer too small for
/// the frame it claims to hold.
pub(crate) fn convert_to_rgba(
    src: &[u8],
    offset: u32,
    w: u32,
    h: u32,
    stride: i32,
    f: pipewire::spa::param::video::VideoFormat,
) -> Option<Vec<u8>> {
    use pipewire::spa::param::video::VideoFormat;
    let bpp = bytes_per_pixel(f)?;
    let (w, h, offset) = (w as usize, h as usize, offset as usize);
    let stride = usize::try_from(stride).ok()?;
    let row_len = w.checked_mul(bpp)?;
    if stride < row_len {
        return None;
    }
    if h > 0 {
        let end = offset
            .checked_add((h - 1).checked_mul(stride)?)?
            .checked_add(row_len)?;
        if end > src.len() {
            return None;
        }
    }
    let swap = matches!(f, VideoFormat::BGRx | VideoFormat::BGRA | VideoFormat::BGR);
    let mut out = Vec::with_capacity(w.checked_mul(h)?.checked_mul(4)?);
    for y in 0..h {
        let row = &src[offset + y * stride..][..row_len];
        for px in row.chunks_exact(bpp) {
            let (r, b) = if swap { (px[2], px[0]) } else { (px[0], px[2]) };
            out.extend_from_slice(&[r, px[1], b, 0xff]);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pipewire::spa::param::video::VideoFormat;

    #[test]
    fn converts_padded_rows_and_offset() {
        // Two BGRx pixels per row, rows padded to 12 bytes, frame
        // starting 2 bytes in.
        let mut src = vec![0xee; 2];
        src.extend_from_slice(&[1, 2, 3, 0, 4, 5, 6, 0, 9, 9, 9, 9]);
        src.extend_from_slice(&[7, 8, 9, 0, 10, 11, 12, 0]);
        let out = convert_to_rgba(&src, 2, 2, 2, 12, VideoFormat::BGRx).unwrap();
        assert_eq!(
            out,
            [3, 2, 1, 255, 6, 5, 4, 255, 9, 8, 7, 255, 12, 11, 10, 255]
        );
        let out = convert_to_rgba(&[1, 2, 3], 0, 1, 1, 3, VideoFormat::RGB).unwrap();
        assert_eq!(out, [1, 2, 3, 255]);
    }

    #[test]
    fn rejects_buffers_that_dont_fit() {
        let src = [0u8; 16];
        // Short by one byte.
        assert!(convert_to_rgba(&src, 1, 2, 2, 8, VideoFormat::BGRx).is_none());
        // Stride narrower than a row, and negative.
        assert!(convert_to_rgba(&src, 0, 2, 2, 4, VideoFormat::BGRx).is_none());
        assert!(convert_to_rgba(&src, 0, 2, 2, -8, VideoFormat::BGRx).is_none());
        // Unsupported format.
        assert!(convert_to_rgba(&src, 0, 1, 1, 4, VideoFormat::I420).is_none());
        // Fits exactly.
        assert!(convert_to_rgba(&src, 0, 2, 2, 8, VideoFormat::BGRx).is_some());
    }
}
