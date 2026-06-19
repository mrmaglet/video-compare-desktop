//! Playback engine, lifted almost verbatim from the main app's `src/player.rs`.
//!
//! The ONLY change for the prototype: instead of waking a winit event loop via an
//! `EventLoopProxy`, the appsink calls a generic `wake` closure (here it requests
//! a Slint redraw). Everything that makes tab switching instant — one pipeline,
//! one shared clock, every source kept live with its latest frame retained — is
//! unchanged, so the switch behaves exactly as in the real app.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use gst::glib;
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;

use crate::media::{self, MediaInfo};

/// Called (off the UI thread, from the appsink) when a new frame is ready, to ask
/// the UI to redraw. In the real app this posted a winit `UserEvent::Redraw`.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// Per-tab data sent to the UI for the tab strip and tooltip.
#[derive(PartialEq)]
pub struct TabUiData {
    pub label: String,
    pub active: bool,
    pub starred: bool,
    pub path: String,
    pub info: String,
    pub video_codec: String,
    pub filesize: String,
}

enum SeekReq {
    Seek {
        pos: gst::ClockTime,
        flags: gst::SeekFlags,
    },
    Exit,
}

/// One open video: its `playbin3`, the slot holding its most recent frame, and
/// the static media facts filled in asynchronously by the discoverer.
struct Source {
    playbin: gst::Element,
    latest: Arc<Mutex<Option<gst::Sample>>>,
    info: Arc<Mutex<Option<MediaInfo>>>,
}

/// A decoded RGBA frame borrowed for upload.
pub struct FrameRef<'a> {
    pub width: i32,
    pub height: i32,
    pub stride: i32,
    pub data: &'a [u8],
}

/// A snapshot of engine state the UI needs for one frame.
#[derive(Default)]
pub struct Snapshot {
    pub position: f64,
    pub duration: f64,
    pub paused: bool,
    pub path: Option<String>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub fps: Option<f64>,
    /// Static codec/stream facts for the active tab (empty until discovered).
    pub media: MediaInfo,
}

/// A tab the user closed, kept so Ctrl+Shift+T can bring it back at its old spot
/// (Chrome-style: most-recently-closed is restored first).
struct ClosedTab {
    path: String,
    index: usize,
    starred: bool,
}

/// Owns the single pipeline and the open tabs.
pub struct Players {
    pipeline: gst::Pipeline,
    tabs: Vec<String>,
    sources: Vec<Source>,
    /// Parallel to `tabs`/`sources`: whether each tab is starred.
    starred: Vec<bool>,
    /// LIFO of tabs the user closed, for Ctrl+Shift+T restore.
    closed: Vec<ClosedTab>,
    active: usize,
    paused: bool,
    /// Whether end-of-stream rewinds and keeps playing (on by default). Toggled
    /// from the UI (L / Ctrl+L). Pipeline-wide, like the shared clock.
    loop_enabled: bool,
    duration: Option<gst::ClockTime>,
    position: gst::ClockTime,
    wake: Wake,
    seek_tx: Sender<SeekReq>,
    seek_thread: Option<JoinHandle<()>>,
    /// DIAG: set when a tab switch happens, cleared when the first frame for the
    /// new active tab is uploaded — to measure perceived switch latency.
    switch_pending: Option<std::time::Instant>,
    /// Set whenever there's new content to show (a freshly decoded frame, a tab
    /// switch, a seek…). The UI presents — and so Slint actually redraws — only
    /// when this is set, so a paused switch repaints at once while a truly idle
    /// window stays quiet. Shared with the appsink so it can flag new frames.
    dirty: Arc<AtomicBool>,
}

impl Players {
    pub fn new(wake: Wake) -> Self {
        let pipeline = gst::Pipeline::new();
        let dirty = Arc::new(AtomicBool::new(true));
        let (seek_tx, seek_rx) = channel::<SeekReq>();
        let seek_thread = {
            let pipeline = pipeline.clone();
            std::thread::Builder::new()
                .name("seek".to_owned())
                .spawn(move || seek_thread(pipeline, seek_rx))
                .ok()
        };
        Self {
            pipeline,
            tabs: Vec::new(),
            sources: Vec::new(),
            starred: Vec::new(),
            closed: Vec::new(),
            active: 0,
            paused: true,
            loop_enabled: true,
            duration: None,
            position: gst::ClockTime::ZERO,
            wake,
            seek_tx,
            seek_thread,
            switch_pending: None,
            dirty,
        }
    }

    /// Mark that there's new content to present (flags a redraw next tick).
    fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
    }

    /// Consume the dirty flag: returns `true` (and clears it) when the UI should
    /// rebuild the frame image and redraw. Driven by the UI tick.
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }

    fn send_seek(&mut self, pos: gst::ClockTime, flags: gst::SeekFlags) {
        self.position = pos;
        self.mark_dirty();
        let _ = self.seek_tx.send(SeekReq::Seek { pos, flags });
    }

    pub fn open(&mut self, path: &str) {
        let latest = Arc::new(Mutex::new(None));
        let playbin = match build_source(path, latest.clone(), &self.wake, self.dirty.clone()) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("failed to open {path}: {e}");
                return;
            }
        };

        // Probe the static media facts in the background; the info bar fills in
        // as soon as the discoverer reports (a redraw is flagged when it lands).
        let info = Arc::new(Mutex::new(None));
        if let Ok(uri) = to_uri(path) {
            let info = info.clone();
            let wake = self.wake.clone();
            let dirty = self.dirty.clone();
            std::thread::Builder::new()
                .name("discover".to_owned())
                .spawn(move || {
                    if let Some(mi) = media::discover(&uri) {
                        if let Ok(mut slot) = info.lock() {
                            *slot = Some(mi);
                        }
                        dirty.store(true, Ordering::Release);
                        wake();
                    }
                })
                .ok();
        }
        if let Err(e) = self.pipeline.add(&playbin) {
            eprintln!("failed to add source to pipeline: {e}");
            return;
        }

        // Where the active tab is right now — the new source should open at the
        // same moment (and same play/pause state), not from the start. We query
        // before pushing the new tab so this reads the *current* active source;
        // for the very first tab there's nothing playing yet, so it stays at 0.
        let join_at = self.query_position().unwrap_or(self.position);

        // A freshly added source ignores seeks until it has prerolled to PAUSED —
        // which is exactly why it otherwise begins at 0. Bring it to PAUSED, wait
        // for that (best-effort, with a cap), then jump it to the shared position
        // before it goes live so it lands frame-aligned with the active tab.
        let _ = playbin.set_state(gst::State::Paused);
        let _ = playbin.state(gst::ClockTime::from_seconds(3));
        if join_at > gst::ClockTime::ZERO {
            let _ = playbin.seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                join_at,
            );
        }

        self.tabs.push(path.to_string());
        self.sources.push(Source {
            playbin,
            latest,
            info,
        });
        self.starred.push(false);

        // Match the shared play/pause state for the whole pipeline.
        let target = if self.paused {
            gst::State::Paused
        } else {
            gst::State::Playing
        };
        let _ = self.pipeline.set_state(target);

        if join_at > gst::ClockTime::ZERO {
            // Snap every source to one shared position: the new tab is already
            // prerolled and seeked (so it shows the right frame immediately), and
            // the others may have crept forward during that preroll — this keeps
            // all tabs frame-aligned for comparison.
            self.send_seek(join_at, gst::SeekFlags::FLUSH);
        } else {
            self.position = join_at;
        }
        self.switch_to(self.tabs.len() - 1);
    }

    /// Switch to tab `i`. Because every source is already live and in sync, this
    /// just changes which one we draw; the next render uploads its retained latest
    /// frame — so even while paused the target appears instantly, same moment.
    pub fn switch_to(&mut self, i: usize) {
        if i < self.tabs.len() {
            self.active = i;
            let t = std::time::Instant::now();
            self.apply_audio();
            let audio = t.elapsed();
            self.switch_pending = Some(std::time::Instant::now());
            self.mark_dirty();
            eprintln!("[switch] -> tab {i}: apply_audio took {audio:?}");
            (self.wake)();
        }
    }

    /// DIAG: take the pending-switch timestamp (set by `switch_to`), if any.
    pub fn take_switch_pending(&mut self) -> Option<std::time::Instant> {
        self.switch_pending.take()
    }

    fn apply_audio(&self) {
        for (i, src) in self.sources.iter().enumerate() {
            src.playbin.set_property("mute", i != self.active);
        }
    }

    pub fn close(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        let src = self.sources.remove(i);
        let path = self.tabs.remove(i);
        let was_starred = self.starred.remove(i);
        self.closed.push(ClosedTab {
            path,
            index: i,
            starred: was_starred,
        });
        let _ = self.pipeline.remove(&src.playbin);
        let _ = src.playbin.set_state(gst::State::Null);

        if self.tabs.is_empty() {
            self.active = 0;
            self.paused = true;
            let _ = self.pipeline.set_state(gst::State::Ready);
            self.position = gst::ClockTime::ZERO;
            self.duration = None;
            return;
        }
        if i < self.active || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        self.apply_audio();
        self.mark_dirty();
        (self.wake)();
    }

    /// Toggle the star flag on tab `i` (the gold marker in the strip).
    pub fn toggle_star(&mut self, i: usize) {
        if let Some(s) = self.starred.get_mut(i) {
            *s = !*s;
            self.mark_dirty();
            (self.wake)();
        }
    }

    /// Move the tab at `from` to position `to`, keeping `tabs`/`sources`/`starred`
    /// in lockstep and following the active tab so it stays selected.
    pub fn reorder(&mut self, from: usize, to: usize) {
        if from >= self.tabs.len() {
            return;
        }
        let to = to.min(self.tabs.len() - 1);
        if from == to {
            return;
        }
        let tab = self.tabs.remove(from);
        let src = self.sources.remove(from);
        let star = self.starred.remove(from);
        self.tabs.insert(to, tab);
        self.sources.insert(to, src);
        self.starred.insert(to, star);

        // Track where the active tab landed.
        self.active = if self.active == from {
            to
        } else {
            let mut a = self.active;
            if from < a {
                a -= 1;
            }
            if to <= a {
                a += 1;
            }
            a
        };
        self.apply_audio();
        self.mark_dirty();
        (self.wake)();
    }

    /// Re-open the most-recently-closed tab at its old index (Chrome's
    /// Ctrl+Shift+T). Repeated calls walk back the close history.
    pub fn reopen_closed(&mut self) {
        let Some(c) = self.closed.pop() else {
            return;
        };
        // `open` appends the new tab at the end and switches to it.
        self.open(&c.path);
        let last = self.tabs.len().saturating_sub(1);
        if let Some(s) = self.starred.get_mut(last) {
            *s = c.starred;
        }
        let dest = c.index.min(last);
        if dest != last {
            self.reorder(last, dest);
        }
    }

    /// Index of the active tab (for keyboard shortcuts that act on "this tab").
    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn toggle_pause(&mut self) {
        self.set_paused(!self.paused);
    }

    /// Whether end-of-stream rewinds and keeps playing.
    pub fn loop_enabled(&self) -> bool {
        self.loop_enabled
    }

    /// Flip looping on/off; returns the new state.
    pub fn toggle_loop(&mut self) -> bool {
        self.loop_enabled = !self.loop_enabled;
        self.mark_dirty();
        self.loop_enabled
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        if self.tabs.is_empty() {
            return;
        }
        let target = if paused {
            gst::State::Paused
        } else {
            gst::State::Playing
        };
        let _ = self.pipeline.set_state(target);
        self.mark_dirty();
    }

    pub fn seek_absolute(&mut self, pos: f64) {
        self.send_seek(secs_to_clock(pos), gst::SeekFlags::FLUSH);
    }

    pub fn seek_relative(&mut self, secs: f64) {
        let cur = self.query_position().unwrap_or(self.position);
        let target = (cur.nseconds() as i64 + (secs * 1e9) as i64).max(0) as u64;
        self.send_seek(gst::ClockTime::from_nseconds(target), gst::SeekFlags::FLUSH);
    }

    pub fn seek_keyframe(&mut self, dir: i32) {
        if dir == 0 || self.tabs.is_empty() {
            return;
        }
        let frame = self.active_frame_duration_ns().unwrap_or(1_000_000_000 / 30) as i64;
        let cur = self.query_position().unwrap_or(self.position).nseconds() as i64;
        let last = self
            .duration
            .map(|d| (d.nseconds() as i64 - frame).max(0))
            .unwrap_or(i64::MAX);
        let (target, snap) = if dir > 0 {
            ((cur + frame).min(last), gst::SeekFlags::SNAP_AFTER)
        } else {
            ((cur - frame).max(0), gst::SeekFlags::SNAP_BEFORE)
        };
        self.send_seek(
            gst::ClockTime::from_nseconds(target.max(0) as u64),
            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT | snap,
        );
    }

    pub fn frame_step(&mut self, dir: i32) {
        if self.tabs.is_empty() {
            return;
        }
        self.mark_dirty();
        if dir >= 0 {
            self.pipeline.send_event(gst::event::Step::new(
                gst::format::Buffers::from_u64(1),
                1.0,
                true,
                false,
            ));
        } else {
            let frame_ns = self.active_frame_duration_ns().unwrap_or(1_000_000_000 / 30);
            let cur = self.query_position().unwrap_or(self.position);
            let target = (cur.nseconds() as i64 - frame_ns as i64).max(0) as u64;
            self.send_seek(
                gst::ClockTime::from_nseconds(target),
                gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
            );
        }
    }

    pub fn tick(&mut self) {
        if let Some(p) = self.query_position() {
            self.position = p;
        }
        if self.duration.is_none() || self.duration == Some(gst::ClockTime::ZERO) {
            self.duration = self.pipeline.query_duration::<gst::ClockTime>();
        }
    }

    pub fn pump_events(&mut self) -> bool {
        use gst::MessageView;
        let Some(bus) = self.pipeline.bus() else {
            return false;
        };
        let mut redraw = false;
        while let Some(msg) = bus.pop() {
            match msg.view() {
                MessageView::Eos(_) => {
                    // Always rewind to the start; when looping is off, also pause
                    // there so the clip ends ready to be replayed rather than
                    // restarting on its own.
                    self.send_seek(gst::ClockTime::ZERO, gst::SeekFlags::FLUSH);
                    if !self.loop_enabled {
                        self.set_paused(true);
                    }
                    redraw = true;
                }
                MessageView::DurationChanged(_) => {
                    self.duration = self.pipeline.query_duration::<gst::ClockTime>();
                    redraw = true;
                }
                MessageView::StateChanged(_) | MessageView::AsyncDone(_) => redraw = true,
                MessageView::Error(err) => {
                    eprintln!(
                        "gst error from {:?}: {} ({:?})",
                        err.src().map(|s| s.path_string()),
                        err.error(),
                        err.debug()
                    );
                }
                _ => {}
            }
        }
        redraw
    }

    /// Borrow the active source's latest decoded frame for upload.
    pub fn map_active_frame<R>(&self, f: impl FnOnce(FrameRef) -> R) -> Option<R> {
        let src = self.sources.get(self.active)?;
        let guard = src.latest.lock().ok()?;
        let sample = guard.as_ref()?;
        let buffer = sample.buffer()?;
        let caps = sample.caps()?;
        let info = gst_video::VideoInfo::from_caps(caps).ok()?;
        let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).ok()?;
        let data = frame.plane_data(0).ok()?;
        Some(f(FrameRef {
            width: info.width() as i32,
            height: info.height() as i32,
            stride: info.stride()[0],
            data,
        }))
    }

    pub fn snapshot(&self) -> Snapshot {
        let (mut width, mut height, mut fps) = (None, None, None);
        let mut media = MediaInfo::default();
        if let Some(src) = self.sources.get(self.active) {
            if let Ok(guard) = src.info.lock() {
                if let Some(mi) = guard.as_ref() {
                    media = mi.clone();
                }
            }
            if let Ok(guard) = src.latest.lock() {
                if let Some(caps) = guard.as_ref().and_then(|s| s.caps()) {
                    if let Ok(info) = gst_video::VideoInfo::from_caps(caps) {
                        width = Some(info.width() as i32);
                        height = Some(info.height() as i32);
                        let f = info.fps();
                        if f.denom() != 0 && f.numer() != 0 {
                            fps = Some(f.numer() as f64 / f.denom() as f64);
                        }
                    }
                }
            }
        }
        Snapshot {
            position: clock_to_secs(self.position),
            duration: self.duration.map(clock_to_secs).unwrap_or(0.0),
            paused: self.paused,
            path: self.tabs.get(self.active).cloned(),
            width,
            height,
            fps,
            media,
        }
    }

    /// Per-tab data for the UI strip and tooltip.
    pub fn tabs_for_ui(&self) -> Vec<TabUiData> {
        self.tabs
            .iter()
            .enumerate()
            .map(|(i, path)| {
                let src = &self.sources[i];

                let info = src
                    .latest
                    .lock()
                    .ok()
                    .and_then(|g| {
                        let caps = g.as_ref()?.caps()?;
                        let vi = gst_video::VideoInfo::from_caps(caps).ok()?;
                        let f = vi.fps();
                        Some(if f.denom() != 0 && f.numer() != 0 {
                            format!(
                                "{}×{} · {} fps",
                                vi.width(),
                                vi.height(),
                                trim_fps(f.numer() as f64 / f.denom() as f64)
                            )
                        } else {
                            format!("{}×{}", vi.width(), vi.height())
                        })
                    })
                    .unwrap_or_default();

                let video_codec = src
                    .info
                    .lock()
                    .ok()
                    .and_then(|g| g.as_ref().map(|mi| mi.video_codec.clone()))
                    .unwrap_or_default();

                let filesize = if !path.contains("://") {
                    std::fs::metadata(path)
                        .ok()
                        .map(|m| human_bytes(m.len()))
                        .unwrap_or_default()
                } else {
                    String::new()
                };

                TabUiData {
                    label: basename(path),
                    active: i == self.active,
                    starred: self.starred.get(i).copied().unwrap_or(false),
                    path: path.clone(),
                    info,
                    video_codec,
                    filesize,
                }
            })
            .collect()
    }

    fn query_position(&self) -> Option<gst::ClockTime> {
        if self.tabs.is_empty() {
            return None;
        }
        self.pipeline.query_position::<gst::ClockTime>()
    }

    fn active_frame_duration_ns(&self) -> Option<u64> {
        let src = self.sources.get(self.active)?;
        let guard = src.latest.lock().ok()?;
        let caps = guard.as_ref()?.caps()?;
        let info = gst_video::VideoInfo::from_caps(caps).ok()?;
        let f = info.fps();
        if f.numer() == 0 {
            return None;
        }
        Some((1_000_000_000u64 * f.denom() as u64) / f.numer() as u64)
    }
}

impl Drop for Players {
    fn drop(&mut self) {
        let _ = self.seek_tx.send(SeekReq::Exit);
        if let Some(handle) = self.seek_thread.take() {
            let _ = handle.join();
        }
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn seek_thread(pipeline: gst::Pipeline, rx: std::sync::mpsc::Receiver<SeekReq>) {
    use std::iter;
    loop {
        let Ok(first) = rx.recv() else { break };
        let mut last: Option<(gst::ClockTime, gst::SeekFlags)> = None;
        for req in iter::once(first).chain(rx.try_iter()) {
            match req {
                SeekReq::Seek { pos, flags } => last = Some((pos, flags)),
                SeekReq::Exit => return,
            }
        }
        if let Some((pos, flags)) = last {
            let _ = pipeline.seek_simple(flags, pos);
        }
    }
}

fn build_source(
    path: &str,
    latest: Arc<Mutex<Option<gst::Sample>>>,
    wake: &Wake,
    dirty: Arc<AtomicBool>,
) -> Result<gst::Element, String> {
    let caps = gst::Caps::builder("video/x-raw")
        .field("format", "RGBA")
        .build();

    let store = {
        let latest = latest.clone();
        let wake = wake.clone();
        move |sample: gst::Sample| {
            if let Ok(mut slot) = latest.lock() {
                *slot = Some(sample);
            }
            // A freshly decoded frame is new content to show; flag a redraw.
            dirty.store(true, Ordering::Release);
            (wake)();
        }
    };
    let store_preroll = store.clone();

    let appsink = gst_app::AppSink::builder()
        .caps(&caps)
        .max_buffers(1)
        .drop(true)
        .sync(true)
        .build();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                store(sample);
                Ok(gst::FlowSuccess::Ok)
            })
            .new_preroll(move |sink| {
                let sample = sink.pull_preroll().map_err(|_| gst::FlowError::Eos)?;
                store_preroll(sample);
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    let playbin = gst::ElementFactory::make("playbin3")
        .build()
        .map_err(|e| format!("playbin3: {e}"))?;
    let uri = to_uri(path)?;
    playbin.set_property("uri", &uri);
    playbin.set_property("video-sink", appsink.upcast_ref::<gst::Element>());

    let flags = playbin.property_value("flags");
    if let Some(flags_class) = glib::FlagsClass::with_type(flags.type_()) {
        if let Some(builder) = flags_class.builder_with_value(flags) {
            if let Some(new_flags) = builder
                .unset_by_nick("text")
                .unset_by_nick("deinterlace")
                .build()
            {
                playbin.set_property_from_value("flags", &new_flags);
            }
        }
    }
    playbin.set_property("mute", true);

    Ok(playbin)
}

fn to_uri(path: &str) -> Result<String, String> {
    if path.contains("://") {
        return Ok(path.to_string());
    }
    glib::filename_to_uri(path, None)
        .map(|s| s.to_string())
        .map_err(|e| format!("bad path {path}: {e}"))
}

fn secs_to_clock(secs: f64) -> gst::ClockTime {
    gst::ClockTime::from_nseconds((secs.max(0.0) * 1e9) as u64)
}

fn clock_to_secs(t: gst::ClockTime) -> f64 {
    t.nseconds() as f64 / 1e9
}

fn basename(path: &str) -> String {
    let trimmed = path.split('?').next().unwrap_or(path);
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(path)
        .to_string()
}

fn trim_fps(f: f64) -> String {
    let s = format!("{f:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}
