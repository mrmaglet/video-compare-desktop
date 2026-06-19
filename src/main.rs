//! Video Compare Desktop — side-by-side video comparison with instant tab switching.
//!
//! The GStreamer engine ([`player`]) drives playback; each decoded frame is
//! uploaded into a GL texture in the renderer context and handed to the UI as
//! the `video-frame` Image.
//!
//! Two things make the switch instant and cheap:
//!
//!  * We upload through GL (a driver DMA copy), never CPU-reading the decoded
//!    frame — that read is pathologically slow (~50 MB/s) on the write-combined
//!    memory hardware decoders hand back.
//!  * We ping-pong between two textures so the `video-frame` property's value
//!    genuinely changes every present. Slint only repaints when a tracked
//!    property changes; a single texture (constant id) mutated underneath looked
//!    unchanged to Slint, so paused switches weren't drawn until some unrelated
//!    event dirtied the scene hundreds of milliseconds later. Alternating the id
//!    makes every new frame and every tab switch a real change → instant repaint.
//!
//! We present only when the engine flags new content (`take_dirty`) or on a user
//! action, so an idle paused window still stays quiet.

slint::include_modules!();

mod media;
mod player;
mod video_texture;

use std::cell::{Cell, RefCell};
use std::ffi::CString;
use std::rc::Rc;
use std::sync::Arc;

use player::{Players, TabUiData};
use video_texture::VideoTexture;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    gstreamer::init()?;
    // The rendering notifier (our hook for GL texture upload) needs the GL
    // renderer; pin the winit+femtovg backend before the window is created.
    if std::env::var_os("SLINT_BACKEND").is_none() {
        std::env::set_var("SLINT_BACKEND", "winit-femtovg");
    }

    let app = MainWindow::new()?;

    // Fix HiDPI: pin the window to 1120×660 physical pixels so it appears the
    // same size on 1x, 2K/2x, and 4K/3x screens. Pass the DPI scale factor to
    // the UI so logical dimensions (e.g. min tab width) can compensate.
    {
        let scale = app.window().scale_factor();
        app.set_display_scale(scale);
        app.window().set_size(slint::WindowSize::Physical(slint::PhysicalSize::new(1120, 660)));
    }

    let weak = app.as_weak();

    // The appsink (on a GStreamer thread) flags new content and asks the UI to
    // redraw when a new frame is ready. `upgrade_in_event_loop` marshals back
    // onto the Slint UI thread, where the next tick presents it.
    let wake: player::Wake = {
        let weak = weak.clone();
        Arc::new(move || {
            let _ = weak.upgrade_in_event_loop(|app| app.window().request_redraw());
        })
    };

    let players = Rc::new(RefCell::new(Players::new(wake)));
    for path in std::env::args().skip(1) {
        players.borrow_mut().open(&path);
    }

    // ---- Shared GL-present state -------------------------------------------
    // The texture pair lives in Slint's GL context (created in the rendering
    // notifier). `shown` is the slot Slint currently displays; `pending` is the
    // slot the next render must upload the active frame into. `present` sets the
    // property to the *other* slot (a real change → Slint schedules a redraw) and
    // records it in `pending`; the notifier does the actual GL upload into that
    // slot just before Slint composites it.
    let render: Rc<RefCell<Option<VideoTexture>>> = Rc::new(RefCell::new(None));
    let shown = Rc::new(Cell::new(0usize));
    let pending = Rc::new(Cell::new(None::<usize>));

    let present: Rc<dyn Fn()> = {
        let players = players.clone();
        let weak = weak.clone();
        let render = render.clone();
        let shown = shown.clone();
        let pending = pending.clone();
        Rc::new(move || {
            let Some(app) = weak.upgrade() else { return };
            // Size comes from the active source's caps (cheap — no buffer map);
            // the upload in the notifier uses the matching frame.
            let snap = players.borrow().snapshot();
            let (Some(w), Some(h)) = (snap.width, snap.height) else {
                return;
            };
            let next = 1 - shown.get();
            let id = match render.borrow().as_ref() {
                Some(vt) => vt.id(next),
                None => return, // renderer not up yet; the dirty flag retries
            };
            let img = unsafe {
                slint::BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(
                    id,
                    euclid::default::Size2D::new(w as u32, h as u32),
                )
                .origin(slint::BorrowedOpenGLTextureOrigin::TopLeft)
                .build()
            };
            app.set_video_frame(img);
            pending.set(Some(next));
            shown.set(next);
            if let Some(t) = players.borrow_mut().take_switch_pending() {
                eprintln!("[switch] presented {:?} after switch", t.elapsed());
            }
        })
    };

    // ---- Rendering notifier: create textures, do the deferred upload --------
    {
        let players = players.clone();
        let render = render.clone();
        let pending = pending.clone();
        app.window()
            .set_rendering_notifier(move |state, api| match state {
                slint::RenderingState::RenderingSetup => {
                    if let slint::GraphicsAPI::NativeOpenGL { get_proc_address } = api {
                        let gl = unsafe {
                            glow::Context::from_loader_function(|s| {
                                CString::new(s)
                                    .map(|c| get_proc_address(&c))
                                    .unwrap_or(std::ptr::null())
                            })
                        };
                        match VideoTexture::new(Arc::new(gl)) {
                            Ok(vt) => *render.borrow_mut() = Some(vt),
                            Err(e) => eprintln!("video texture init failed: {e}"),
                        }
                    }
                }
                slint::RenderingState::BeforeRendering => {
                    // Upload the active frame into the slot `present` chose, just
                    // before Slint composites it. GL upload only — no CPU read.
                    if let Some(idx) = pending.take() {
                        if let Some(vt) = render.borrow_mut().as_mut() {
                            players.borrow().map_active_frame(|f| {
                                vt.upload(idx, f.width, f.height, f.stride, f.data)
                            });
                        }
                    }
                }
                slint::RenderingState::RenderingTeardown => {
                    if let Some(vt) = render.borrow_mut().take() {
                        vt.delete();
                    }
                }
                _ => {}
            })?;
    }

    // ---- Transport / tab callbacks → the engine ----------------------------
    // Each user action presents straight away so the result shows this instant,
    // not on the next timer tick.
    {
        let p = players.clone();
        let present = present.clone();
        app.on_toggle_play(move || {
            p.borrow_mut().toggle_pause();
            present();
        });
    }
    {
        let p = players.clone();
        let weak = weak.clone();
        app.on_toggle_loop(move || {
            let on = p.borrow_mut().toggle_loop();
            if let Some(app) = weak.upgrade() {
                app.set_loop_enabled(on);
            }
        });
    }
    {
        let p = players.clone();
        let present = present.clone();
        app.on_select_tab(move |i| {
            p.borrow_mut().switch_to(i.max(0) as usize);
            present();
        });
    }
    {
        let p = players.clone();
        let weak = weak.clone();
        let present = present.clone();
        app.on_close_tab(move |i| {
            p.borrow_mut().close(i.max(0) as usize);
            if p.borrow().tabs_for_ui().is_empty() {
                if let Some(app) = weak.upgrade() {
                    app.set_video_frame(slint::Image::default());
                }
            } else {
                present();
            }
        });
    }
    {
        let p = players.clone();
        let present = present.clone();
        app.on_toggle_star(move |i| {
            p.borrow_mut().toggle_star(i.max(0) as usize);
            present();
        });
    }
    {
        let p = players.clone();
        let present = present.clone();
        app.on_reorder_tab(move |from, to| {
            p.borrow_mut().reorder(from.max(0) as usize, to.max(0) as usize);
            present();
        });
    }
    {
        let p = players.clone();
        let present = present.clone();
        app.on_reopen_closed(move || {
            p.borrow_mut().reopen_closed();
            present();
        });
    }
    {
        let p = players.clone();
        let present = present.clone();
        app.on_step(move |d| {
            p.borrow_mut().frame_step(d);
            present();
        });
    }
    {
        let p = players.clone();
        let present = present.clone();
        app.on_keyframe(move |d| {
            p.borrow_mut().seek_keyframe(d);
            present();
        });
    }
    {
        let p = players.clone();
        let present = present.clone();
        app.on_seek(move |f| {
            let dur = p.borrow().snapshot().duration;
            p.borrow_mut().seek_absolute(f as f64 * dur);
            present();
        });
    }
    {
        let p = players.clone();
        let present = present.clone();
        app.on_seek_relative(move |secs| {
            p.borrow_mut().seek_relative(secs as f64);
            present();
        });
    }
    app.on_request_quit(|| {
        let _ = slint::quit_event_loop();
    });
    {
        let p = players.clone();
        let weak = weak.clone();
        let present = present.clone();
        app.on_open_file(move || {
            if let Some(files) = rfd::FileDialog::new()
                .set_title("Open video")
                .pick_files()
            {
                for f in files {
                    if let Some(s) = f.to_str() {
                        p.borrow_mut().open(s);
                    }
                }
            }
            // The dialog stole keyboard focus; give it back so shortcuts work.
            if let Some(app) = weak.upgrade() {
                app.invoke_refocus();
            }
            present();
        });
    }

    // Drive the shared clock + bus and mirror engine state into the UI props.
    let timer = slint::Timer::default();
    {
        let players = players.clone();
        let weak = weak.clone();
        let present = present.clone();
        // Remember the last tab list so we only rebuild the Slint model when it
        // actually changes. Rebuilding it every tick destroyed and recreated the
        // tab elements ~30×/s, faster than a click completes — so press and release
        // landed on different element instances and `clicked` never fired.
        let mut last_tabs: Vec<TabUiData> = Vec::new();
        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(33),
            move || {
                let (snap, tabs, dirty) = {
                    let mut pl = players.borrow_mut();
                    pl.tick();
                    let _ = pl.pump_events();
                    (pl.snapshot(), pl.tabs_for_ui(), pl.take_dirty())
                };
                // Present new frames (playing, or a just-seeked/just-switched
                // paused frame); an idle paused window leaves `dirty` clear and so
                // doesn't repaint.
                if dirty {
                    present();
                }
                let Some(app) = weak.upgrade() else { return };
                app.set_playing(!snap.paused);
                app.set_time_text(
                    format!("{} / {}", fmt_time(snap.position), fmt_time(snap.duration)).into(),
                );
                let frame = snap
                    .fps
                    .filter(|f| *f > 0.0)
                    .map(|f| (snap.position * f).round() as i64);
                app.set_frame_text(
                    match frame {
                        Some(n) => format!("#{n}"),
                        None => "#-".to_string(),
                    }
                    .into(),
                );
                app.set_progress(if snap.duration > 0.0 {
                    (snap.position / snap.duration) as f32
                } else {
                    0.0
                });
                app.set_filename(snap.path.clone().unwrap_or_else(|| "(no file)".into()).into());
                let info = match (snap.width, snap.height, snap.fps) {
                    (Some(w), Some(h), Some(f)) => format!("{w}×{h} · {} fps", trim_fps(f)),
                    (Some(w), Some(h), None) => format!("{w}×{h}"),
                    _ => "—".to_string(),
                };
                app.set_info_text(info.into());
                let size = snap
                    .path
                    .as_deref()
                    .filter(|p| !p.contains("://"))
                    .and_then(|p| std::fs::metadata(p).ok())
                    .map(|m| human_size(m.len()))
                    .unwrap_or_default();
                app.set_size_text(size.into());
                // Static codec/stream facts for the info bar (Ctrl+J / Ctrl+I).
                app.set_info_container(snap.media.container.clone().into());
                app.set_info_video(snap.media.video.clone().into());
                app.set_info_audio(snap.media.audio.clone().into());
                app.set_info_encoder(snap.media.encoder.clone().into());
                app.set_video_codec(snap.media.video_codec.clone().into());
                app.set_audio_codec(snap.media.audio_codec.clone().into());
                app.set_active_tab(players.borrow().active_index() as i32);
                app.set_loop_enabled(players.borrow().loop_enabled());
                if tabs != last_tabs {
                    let model: Vec<TabData> = tabs
                        .iter()
                        .map(|t| TabData {
                            label: t.label.clone().into(),
                            active: t.active,
                            starred: t.starred,
                            path: t.path.clone().into(),
                            info: t.info.clone().into(),
                            video_codec: t.video_codec.clone().into(),
                            filesize: t.filesize.clone().into(),
                        })
                        .collect();
                    app.set_tabs(slint::ModelRc::new(slint::VecModel::from(model)));
                    last_tabs = tabs;
                }
            },
        );
    }

    app.run()?;
    Ok(())
}

fn fmt_time(secs: f64) -> String {
    let t = secs.max(0.0) as u64;
    let (h, m, s) = (t / 3600, (t % 3600) / 60, t % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn trim_fps(f: f64) -> String {
    let s = format!("{f:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn human_size(bytes: u64) -> String {
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
