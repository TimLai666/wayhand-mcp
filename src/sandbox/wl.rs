//! Wayland client for the nested sandbox compositor.
//!
//! All Wayland objects live on one dedicated thread. The async side sends
//! commands through a channel and waits for the reply.

use std::{
    io::Write,
    os::{
        fd::{AsFd, AsRawFd},
        unix::net::UnixStream,
    },
    path::Path,
    sync::mpsc,
    thread,
    time::Instant,
};

use anyhow::{Context, Result, anyhow};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, WEnum,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_buffer, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool},
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::{
    screencopy::v1::client::{
        zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
        zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    },
    virtual_pointer::v1::client::{
        zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
        zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    },
};

pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;
const SCROLL_PIXELS_PER_NOTCH: f64 = 15.0;

#[derive(Debug, Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub png: Vec<u8>,
}

enum Command {
    Screenshot(mpsc::Sender<Result<Frame>>),
    Motion {
        x: u32,
        y: u32,
        reply: mpsc::Sender<Result<()>>,
    },
    Button {
        code: u32,
        pressed: bool,
        reply: mpsc::Sender<Result<()>>,
    },
    Scroll {
        horizontal: i32,
        vertical: i32,
        reply: mpsc::Sender<Result<()>>,
    },
    Key {
        evdev: u16,
        pressed: bool,
        modifiers: u32,
        reply: mpsc::Sender<Result<()>>,
    },
    Shutdown,
}

/// Handle used from the async side. Dropping it stops the thread.
pub struct WlClient {
    sender: mpsc::Sender<Command>,
    thread: Option<thread::JoinHandle<()>>,
}

impl WlClient {
    pub fn connect(socket_path: &Path, keymap_text: &str) -> Result<Self> {
        let stream = UnixStream::connect(socket_path)
            .with_context(|| format!("connect to nested compositor {}", socket_path.display()))?;
        let keymap_text = keymap_text.to_owned();
        let (sender, receiver) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("wayhand-sandbox-wl".to_owned())
            .spawn(move || {
                let mut session = match Session::new(stream, &keymap_text) {
                    Ok(session) => {
                        let _ = ready_tx.send(Ok(()));
                        session
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                session.run(receiver);
            })
            .context("spawn sandbox wayland thread")?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                sender,
                thread: Some(thread),
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(anyhow!("sandbox wayland thread exited before it was ready")),
        }
    }

    fn call<T>(&self, build: impl FnOnce(mpsc::Sender<Result<T>>) -> Command) -> Result<T> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.sender
            .send(build(reply_tx))
            .map_err(|_| anyhow!("sandbox wayland thread is gone"))?;
        reply_rx
            .recv()
            .map_err(|_| anyhow!("sandbox wayland thread dropped the reply"))?
    }

    pub fn screenshot(&self) -> Result<Frame> {
        self.call(Command::Screenshot)
    }

    pub fn motion(&self, x: u32, y: u32) -> Result<()> {
        self.call(|reply| Command::Motion { x, y, reply })
    }

    pub fn button(&self, code: u32, pressed: bool) -> Result<()> {
        self.call(|reply| Command::Button {
            code,
            pressed,
            reply,
        })
    }

    pub fn scroll(&self, horizontal: i32, vertical: i32) -> Result<()> {
        self.call(|reply| Command::Scroll {
            horizontal,
            vertical,
            reply,
        })
    }

    pub fn key(&self, evdev: u16, pressed: bool, modifiers: u32) -> Result<()> {
        self.call(|reply| Command::Key {
            evdev,
            pressed,
            modifiers,
            reply,
        })
    }
}

impl Drop for WlClient {
    fn drop(&mut self) {
        let _ = self.sender.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct State {
    shm: wl_shm::WlShm,
    output: wl_output::WlOutput,
    output_size: Option<(u32, u32)>,
    pending: Option<PendingFrame>,
}

#[derive(Default)]
struct PendingFrame {
    buffer_info: Option<(wl_shm::Format, u32, u32, u32)>,
    y_invert: bool,
    ready: bool,
    failed: bool,
}

struct Session {
    conn: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    screencopy: ZwlrScreencopyManagerV1,
    pointer: ZwlrVirtualPointerV1,
    keyboard: ZwpVirtualKeyboardV1,
    started: Instant,
    /// Extent used for absolute motion: the size of the last captured frame.
    extent: (u32, u32),
    last_modifiers: u32,
}

impl Session {
    fn new(stream: UnixStream, keymap_text: &str) -> Result<Self> {
        let conn = Connection::from_socket(stream).context("wayland handshake")?;
        let (globals, mut queue) =
            registry_queue_init::<State>(&conn).context("read wayland globals")?;
        let qh = queue.handle();

        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).context("bind wl_shm")?;
        let output: wl_output::WlOutput = globals.bind(&qh, 1..=4, ()).context("bind wl_output")?;
        let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=7, ()).context("bind wl_seat")?;
        let screencopy: ZwlrScreencopyManagerV1 = globals.bind(&qh, 1..=1, ()).context(
            "bind zwlr_screencopy_manager_v1 (does the compositor support wlr screencopy?)",
        )?;
        let pointer_manager: ZwlrVirtualPointerManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .context("bind zwlr_virtual_pointer_manager_v1")?;
        let keyboard_manager: ZwpVirtualKeyboardManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .context("bind zwp_virtual_keyboard_manager_v1")?;

        let pointer = pointer_manager.create_virtual_pointer(None, &qh, ());
        let keyboard = keyboard_manager.create_virtual_keyboard(&seat, &qh, ());

        let keymap_fd = memfd_with(keymap_text.as_bytes(), true)?;
        keyboard.keymap(
            1, // WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1
            keymap_fd.as_fd(),
            u32::try_from(keymap_text.len() + 1).context("keymap too large")?,
        );

        let mut state = State {
            shm,
            output,
            output_size: None,
            pending: None,
        };
        queue.roundtrip(&mut state).context("initial roundtrip")?;
        let extent = state.output_size.unwrap_or((1, 1));

        Ok(Self {
            conn,
            queue,
            qh,
            state,
            screencopy,
            pointer,
            keyboard,
            started: Instant::now(),
            extent,
            last_modifiers: 0,
        })
    }

    fn time_ms(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }

    fn run(&mut self, receiver: mpsc::Receiver<Command>) {
        while let Ok(command) = receiver.recv() {
            match command {
                Command::Screenshot(reply) => {
                    let _ = reply.send(self.capture());
                }
                Command::Motion { x, y, reply } => {
                    let _ = reply.send(self.motion(x, y));
                }
                Command::Button {
                    code,
                    pressed,
                    reply,
                } => {
                    let _ = reply.send(self.button(code, pressed));
                }
                Command::Scroll {
                    horizontal,
                    vertical,
                    reply,
                } => {
                    let _ = reply.send(self.scroll(horizontal, vertical));
                }
                Command::Key {
                    evdev,
                    pressed,
                    modifiers,
                    reply,
                } => {
                    let _ = reply.send(self.key(evdev, pressed, modifiers));
                }
                Command::Shutdown => break,
            }
        }
    }

    fn sync(&mut self) -> Result<()> {
        self.conn.flush().context("flush wayland requests")?;
        self.queue
            .roundtrip(&mut self.state)
            .context("wayland roundtrip")?;
        Ok(())
    }

    fn motion(&mut self, x: u32, y: u32) -> Result<()> {
        let (width, height) = self.extent;
        if x >= width || y >= height {
            return Err(anyhow!(
                "sandbox pointer target ({x}, {y}) is outside the sandbox output {width}x{height}"
            ));
        }
        let time = self.time_ms();
        self.pointer.motion_absolute(time, x, y, width, height);
        self.pointer.frame();
        self.sync()
    }

    fn button(&mut self, code: u32, pressed: bool) -> Result<()> {
        let time = self.time_ms();
        let state = if pressed {
            wl_pointer::ButtonState::Pressed
        } else {
            wl_pointer::ButtonState::Released
        };
        self.pointer.button(time, code, state);
        self.pointer.frame();
        self.sync()
    }

    fn scroll(&mut self, horizontal: i32, vertical: i32) -> Result<()> {
        let time = self.time_ms();
        self.pointer.axis_source(wl_pointer::AxisSource::Wheel);
        if vertical != 0 {
            self.pointer.axis_discrete(
                time,
                wl_pointer::Axis::VerticalScroll,
                f64::from(vertical) * SCROLL_PIXELS_PER_NOTCH,
                vertical,
            );
        }
        if horizontal != 0 {
            self.pointer.axis_discrete(
                time,
                wl_pointer::Axis::HorizontalScroll,
                f64::from(horizontal) * SCROLL_PIXELS_PER_NOTCH,
                horizontal,
            );
        }
        self.pointer.frame();
        self.sync()
    }

    fn key(&mut self, evdev: u16, pressed: bool, modifiers: u32) -> Result<()> {
        let time = self.time_ms();
        if modifiers != self.last_modifiers {
            self.keyboard.modifiers(modifiers, 0, 0, 0);
            self.last_modifiers = modifiers;
        }
        self.keyboard
            .key(time, u32::from(evdev), if pressed { 1 } else { 0 });
        self.sync()
    }

    fn capture(&mut self) -> Result<Frame> {
        self.state.pending = Some(PendingFrame::default());
        let frame = self
            .screencopy
            .capture_output(1, &self.state.output, &self.qh, ());
        self.sync()?;

        let (format, width, height, stride) = self
            .state
            .pending
            .as_ref()
            .and_then(|pending| pending.buffer_info)
            .ok_or_else(|| anyhow!("screencopy did not announce a buffer"))?;
        let size = usize::try_from(stride)? * usize::try_from(height)?;
        let memfd = memfd_with(&vec![0u8; size], false)?;
        let pool: wl_shm_pool::WlShmPool =
            self.state
                .shm
                .create_pool(memfd.as_fd(), i32::try_from(size)?, &self.qh, ());
        let buffer: wl_buffer::WlBuffer = pool.create_buffer(
            0,
            i32::try_from(width)?,
            i32::try_from(height)?,
            i32::try_from(stride)?,
            format,
            &self.qh,
            (),
        );
        frame.copy(&buffer);

        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        loop {
            self.queue
                .dispatch_pending(&mut self.state)
                .context("dispatch screencopy events")?;
            let pending = self
                .state
                .pending
                .as_ref()
                .ok_or_else(|| anyhow!("screencopy state lost"))?;
            if pending.failed {
                return Err(anyhow!("compositor reported screencopy failure"));
            }
            if pending.ready {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(anyhow!(
                    "screencopy timed out after 5 seconds; the sandbox compositor produced no frame (a visible sandbox window that is covered, minimized or on another workspace does not get frames from GNOME; use a headless sandbox)"
                ));
            }
            self.conn.flush().context("flush screencopy")?;
            let Some(guard) = self.queue.prepare_read() else {
                continue;
            };
            let mut pollfd = libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut pollfd, 1, remaining.as_millis() as i32) };
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(anyhow!("poll wayland socket: {error}"));
            }
            if ready == 0 {
                drop(guard);
                continue;
            }
            guard.read().context("read wayland events")?;
        }
        let y_invert = self.state.pending.take().is_some_and(|p| p.y_invert);
        buffer.destroy();
        pool.destroy();
        frame.destroy();

        let mapping = unsafe { memmap2::Mmap::map(&memfd) }.context("map screencopy buffer")?;
        let png = encode_png(&mapping, format, width, height, stride, y_invert)?;
        self.extent = (width, height);
        Ok(Frame { width, height, png })
    }
}

fn memfd_with(bytes: &[u8], nul_terminate: bool) -> Result<std::fs::File> {
    let fd = rustix::fs::memfd_create("wayhand-mcp", rustix::fs::MemfdFlags::CLOEXEC)
        .context("memfd_create")?;
    let mut file = std::fs::File::from(fd);
    file.write_all(bytes).context("write memfd")?;
    if nul_terminate {
        file.write_all(&[0]).context("write memfd terminator")?;
    }
    Ok(file)
}

fn encode_png(
    data: &[u8],
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
    y_invert: bool,
) -> Result<Vec<u8>> {
    let (r, g, b) = match format {
        wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888 => (2usize, 1usize, 0usize),
        wl_shm::Format::Xbgr8888 | wl_shm::Format::Abgr8888 => (0, 1, 2),
        other => return Err(anyhow!("unsupported screencopy pixel format {other:?}")),
    };
    let width_usize = usize::try_from(width)?;
    let height_usize = usize::try_from(height)?;
    let stride_usize = usize::try_from(stride)?;
    let mut rgb = Vec::with_capacity(width_usize * height_usize * 3);
    for row in 0..height_usize {
        let source_row = if y_invert {
            height_usize - 1 - row
        } else {
            row
        };
        let line = data
            .get(source_row * stride_usize..source_row * stride_usize + width_usize * 4)
            .ok_or_else(|| anyhow!("screencopy buffer is shorter than its stride implies"))?;
        for pixel in line.chunks_exact(4) {
            rgb.extend_from_slice(&[pixel[r], pixel[g], pixel[b]]);
        }
    }
    let mut png_bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_bytes, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().context("write PNG header")?;
        writer
            .write_image_data(&rgb)
            .context("write PNG image data")?;
    }
    Ok(png_bytes)
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Mode { width, height, .. } = event
            && width > 0
            && height > 0
        {
            state.output_size = Some((width as u32, height as u32));
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(pending) = state.pending.as_mut() else {
            return;
        };
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } => pending.buffer_info = Some((format, width, height, stride)),
            zwlr_screencopy_frame_v1::Event::Flags {
                flags: WEnum::Value(flags),
            } => {
                pending.y_invert = flags.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => pending.ready = true,
            zwlr_screencopy_frame_v1::Event::Failed => pending.failed = true,
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(State: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(State: ignore wl_seat::WlSeat);
wayland_client::delegate_noop!(State: ignore ZwlrScreencopyManagerV1);
wayland_client::delegate_noop!(State: ignore ZwlrVirtualPointerManagerV1);
wayland_client::delegate_noop!(State: ignore ZwlrVirtualPointerV1);
wayland_client::delegate_noop!(State: ignore ZwpVirtualKeyboardManagerV1);
wayland_client::delegate_noop!(State: ignore ZwpVirtualKeyboardV1);

#[cfg(test)]
mod tests {
    use super::encode_png;
    use wayland_client::protocol::wl_shm;

    #[test]
    fn encodes_xrgb_buffer_to_png_with_correct_size() {
        // 2x2 XRGB8888, stride 8: pixels (B,G,R,X)
        let data = [
            0, 0, 255, 0, 0, 255, 0, 0, // row 0: red, green
            255, 0, 0, 0, 255, 255, 255, 0, // row 1: blue, white
        ];
        let png_bytes = encode_png(&data, wl_shm::Format::Xrgb8888, 2, 2, 8, false).unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(&png_bytes));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (2, 2));
        assert_eq!(&buf[..3], &[255, 0, 0]);
        assert_eq!(&buf[9..12], &[255, 255, 255]);
    }

    #[test]
    fn honors_y_invert() {
        let data = [
            0, 0, 255, 0, 0, 0, 255, 0, // row 0: red red
            255, 0, 0, 0, 255, 0, 0, 0, // row 1: blue blue
        ];
        let png_bytes = encode_png(&data, wl_shm::Format::Xrgb8888, 2, 2, 8, true).unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(&png_bytes));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        reader.next_frame(&mut buf).unwrap();
        assert_eq!(&buf[..3], &[0, 0, 255]);
    }
}
