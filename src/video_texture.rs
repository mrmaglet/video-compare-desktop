//! Uploads decoded RGBA frames into GL textures that Slint composites as an
//! `Image`, and hands their ids to Slint via a borrowed-texture handle.
//!
//! Why two textures (ping-pong): Slint only repaints when a property it tracks
//! actually changes value, and it treats a `BorrowedOpenGLTexture` with the same
//! GL id + size as unchanged. Mutating a single texture's contents underneath is
//! therefore invisible to Slint — a paused tab switch wouldn't repaint. By
//! alternating between two texture ids each present, the `video-frame` property
//! genuinely changes, so Slint repaints at once. Uploading goes through GL (a
//! driver-side DMA copy), so we never CPU-read the decoded frame — that read is
//! pathologically slow on the write-combined memory hardware decoders hand back.
//!
//! The textures live in (and are only ever touched from) Slint's GL context, via
//! the rendering notifier.

use std::num::NonZeroU32;
use std::sync::Arc;

use glow::HasContext;

pub struct VideoTexture {
    gl: Arc<glow::Context>,
    tex: [glow::Texture; 2],
    size: [(i32, i32); 2],
}

impl VideoTexture {
    pub fn new(gl: Arc<glow::Context>) -> Result<Self, String> {
        unsafe {
            let make = || -> Result<glow::Texture, String> {
                let tex = gl.create_texture()?;
                gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                for (p, v) in [
                    (glow::TEXTURE_MIN_FILTER, glow::LINEAR),
                    (glow::TEXTURE_MAG_FILTER, glow::LINEAR),
                    (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                    (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
                ] {
                    gl.tex_parameter_i32(glow::TEXTURE_2D, p, v as i32);
                }
                gl.bind_texture(glow::TEXTURE_2D, None);
                Ok(tex)
            };
            let tex = [make()?, make()?];
            Ok(Self {
                gl,
                tex,
                size: [(0, 0), (0, 0)],
            })
        }
    }

    /// The raw GL texture name for slot `idx`, to hand to Slint.
    pub fn id(&self, idx: usize) -> NonZeroU32 {
        self.tex[idx].0
    }

    /// Upload an RGBA frame into slot `idx`. `stride` is the row length in bytes
    /// (may exceed `width * 4` due to alignment padding).
    pub fn upload(&mut self, idx: usize, width: i32, height: i32, stride: i32, data: &[u8]) {
        if width <= 0 || height <= 0 {
            return;
        }
        let gl = &self.gl;
        let row_px = stride / 4;
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(self.tex[idx]));
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            if row_px != width {
                gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, row_px);
            }
            if (width, height) != self.size[idx] {
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA8 as i32,
                    width,
                    height,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    Some(data),
                );
                self.size[idx] = (width, height);
            } else {
                gl.tex_sub_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    0,
                    0,
                    width,
                    height,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(data),
                );
            }
            // Restore the pixel-store state Slint's renderer expects.
            if row_px != width {
                gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
            }
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
    }

    /// Release the GL textures. Must be called while the GL context is current
    /// (i.e. from the rendering notifier's `RenderingTeardown`).
    pub fn delete(&self) {
        unsafe {
            for t in self.tex {
                self.gl.delete_texture(t);
            }
        }
    }
}
