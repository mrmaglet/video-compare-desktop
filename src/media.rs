//! Static media facts for the VLC-style info bar.
//!
//! Run once per file (off the UI thread) via the GStreamer `Discoverer`, which
//! probes the container and elementary streams without us having to wire extra
//! pads into the live pipeline. We surface only *facts* — codec, dimensions,
//! frame rate, sample rate, channels, nominal bitrate — never live values like
//! the instantaneous stream bitrate.

use gstreamer as gst;
use gstreamer_pbutils as pbutils;
use pbutils::prelude::*;

/// The facts shown in the info bar for one open file. Empty fields are simply
/// omitted from the bar, so a file with no audio track shows no audio row.
#[derive(Clone, Default, PartialEq)]
pub struct MediaInfo {
    pub container: String,
    pub video: String,
    pub video_codec: String,
    pub audio: String,
    pub audio_codec: String,
    pub encoder: String,
}

/// Probe `uri` and distil the static facts. Returns `None` if the discoverer
/// can't analyse the file (unsupported, unreachable, timed out).
pub fn discover(uri: &str) -> Option<MediaInfo> {
    let disc = pbutils::Discoverer::new(gst::ClockTime::from_seconds(5)).ok()?;
    let info = disc.discover_uri(uri).ok()?;

    let mut mi = MediaInfo::default();

    // Container — the top-level stream's caps (e.g. "Quicktime", "Matroska").
    // For a raw elementary stream the top level *is* the video codec, so we
    // dedupe against the video row below.
    if let Some(top) = info.stream_info() {
        if let Some(caps) = top.caps() {
            mi.container = pbutils::pb_utils_get_codec_description(&caps).to_string();
        }
    }

    // Video — first stream only (these are comparison clips, not multi-angle).
    if let Some(v) = info.video_streams().into_iter().next() {
        let mut parts: Vec<String> = Vec::new();
        if let Some(caps) = v.caps() {
            mi.video_codec = short_codec(&pbutils::pb_utils_get_codec_description(&caps));
        }
        let (w, h) = (v.width(), v.height());
        if w > 0 && h > 0 {
            parts.push(format!("{w}×{h}"));
        }
        let fr = v.framerate();
        if fr.numer() > 0 && fr.denom() > 0 {
            parts.push(format!("{} fps", trim(fr.numer() as f64 / fr.denom() as f64)));
        }
        if v.depth() > 0 {
            parts.push(format!("{}-bit", v.depth()));
        }
        if let Some(br) = nominal_kbps(v.bitrate(), v.max_bitrate()) {
            parts.push(br);
        }
        mi.video = parts.join("  ·  ");
    }

    // Audio — first stream only.
    if let Some(a) = info.audio_streams().into_iter().next() {
        let mut parts: Vec<String> = Vec::new();
        if let Some(caps) = a.caps() {
            mi.audio_codec = short_codec(&pbutils::pb_utils_get_codec_description(&caps));
        }
        if a.sample_rate() > 0 {
            parts.push(format!("{} kHz", trim(a.sample_rate() as f64 / 1000.0)));
        }
        if a.channels() > 0 {
            parts.push(channel_label(a.channels()));
        }
        if let Some(br) = nominal_kbps(a.bitrate(), a.max_bitrate()) {
            parts.push(br);
        }
        mi.audio = parts.join("  ·  ");
    }

    // Encoder — a static tag when the muxer recorded one (e.g. "x264").
    if let Some(tags) = info.tags() {
        if let Some(enc) = tags.get::<gst::tags::Encoder>() {
            mi.encoder = enc.get().to_string();
        }
    }

    // A raw elementary stream reports the same codec as both container and
    // video; drop the redundant container label in that case.
    if !mi.container.is_empty() && mi.video_codec.starts_with(&mi.container) {
        mi.container.clear();
    }

    Some(mi)
}

/// Strip profile/level annotations from a GStreamer codec description so the
/// chip shows just "H.265" instead of "H.265 (Main)" etc.
fn short_codec(desc: &str) -> String {
    desc.split('(').next().unwrap_or(desc).trim().to_string()
}

/// The declared (nominal) bitrate as "N kb/s". Prefers the stream's nominal/max
/// rate — the static figure muxers record (e.g. AAC's 192) — over the average
/// the discoverer measures while probing, which drifts below it on short clips.
fn nominal_kbps(bitrate: u32, max_bitrate: u32) -> Option<String> {
    let b = if max_bitrate > 0 { max_bitrate } else { bitrate };
    (b > 0).then(|| format!("{} kb/s", b / 1000))
}

fn channel_label(ch: u32) -> String {
    match ch {
        1 => "Mono".to_string(),
        2 => "Stereo".to_string(),
        6 => "5.1".to_string(),
        8 => "7.1".to_string(),
        n => format!("{n} ch"),
    }
}

/// Format a float without trailing zeros (23.976 → "23.976", 48 → "48").
fn trim(f: f64) -> String {
    let s = format!("{f:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}
