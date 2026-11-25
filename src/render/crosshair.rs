use std::path::Path;
use std::time::{Duration, Instant};

use windows::core::Result;
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::Direct2D::{
    ID2D1DeviceContext5, D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
};
use windows_numerics::{Matrix3x2, Vector2};

use crate::config::CrosshairConfig;

use super::decode::{self, ImageAsset, ImageControls};
use super::engine::{Engine, Rgba};
use super::vector::{self, Style, VectorSpec};

pub struct Renderer {
    pub engine: Engine,
    asset: Option<ImageAsset>,
    key: String,
    /// Path and time of the last filesystem check. An animated preview syncs
    /// once per frame; a stat per second is enough to notice a replaced file
    /// without paying for one 30 times a second.
    stat_path: String,
    stat_at: Option<Instant>,
    stat_mtime: u128,
}

impl Renderer {
    pub fn new() -> Result<Self> {
        Ok(Self {
            engine: Engine::new()?,
            asset: None,
            key: String::new(),
            stat_path: String::new(),
            stat_at: None,
            stat_mtime: 0,
        })
    }

    pub fn sync_image(&mut self, cfg: &CrosshairConfig) {
        let Some(path) = cfg.image.as_deref().filter(|p| !p.trim().is_empty()) else {
            self.asset = None;
            self.key.clear();
            self.stat_path.clear();
            self.stat_at = None;
            return;
        };
        let path_text = path.to_string();
        let path = Path::new(path);
        let stale = self.stat_path != path_text
            || self.stat_at.map_or(true, |at| at.elapsed() >= Duration::from_secs(1));
        if stale {
            self.stat_mtime = std::fs::metadata(path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis())
                .unwrap_or(0);
            self.stat_path = path_text;
            self.stat_at = Some(Instant::now());
        }
        let mtime = self.stat_mtime;
        let controls = ImageControls {
            invert: cfg.image_invert,
            solid: cfg.image_solid,
            tint: cfg.image_tint,
            solid_color: cfg.image_solid_color.clone(),
            opacity: cfg.opacity,
            gif_speed: cfg.gif_speed,
        };
        let key = format!(
            "{}|{}|{}|{}|{}|{}|{}|{}",
            path.display(),
            mtime,
            controls.invert,
            controls.solid,
            controls.tint,
            controls.solid_color,
            controls.opacity,
            controls.gif_speed
        );
        if key == self.key && self.asset.is_some() {
            return;
        }
        match decode::decode(&self.engine, path, &controls) {
            Ok(asset) => {
                crate::logging::log(&format!("crosshair: loaded {} ({}x{}, {} frame(s), {})", path.display(), asset.width, asset.height, asset.frames.len(), asset.source_kind));
                self.asset = Some(asset);
                self.key = key;
            }
            Err(err) => {
                crate::logging::log(&format!("crosshair: cannot decode {}: {err}", path.display()));
                self.asset = None;
                self.key.clear();
            }
        }
    }

    pub fn draw(&self, ctx: &ID2D1DeviceContext5, cfg: &CrosshairConfig, center: Vector2, elapsed_ms: u64) -> Result<()> {
        if let Some(asset) = &self.asset {
            let scale = (cfg.image_scale / 100.0).clamp(0.05, 8.0);
            let width = asset.width as f32 * scale;
            let height = asset.height as f32 * scale;
            let rotation = cfg.image_rotation + cfg.rotation;
            let transform = vector::rotation_matrix(center, rotation);
            unsafe {
                ctx.SetTransform(&transform);
                let dest = D2D_RECT_F {
                    left: center.X - width * 0.5,
                    top: center.Y - height * 0.5,
                    right: center.X + width * 0.5,
                    bottom: center.Y + height * 0.5,
                };
                let frame = frame_at(asset, cfg, elapsed_ms);
                ctx.DrawBitmap(
                    &frame.bitmap,
                    Some(&dest),
                    1.0,
                    D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
                    None,
                    None,
                );
                ctx.SetTransform(&Matrix3x2::identity());
            }
            return Ok(());
        }
        vector::draw(ctx, center, &vector_spec(cfg))
    }

    pub fn content_half_size(&self, cfg: &CrosshairConfig) -> (f32, f32) {
        if let Some(asset) = &self.asset {
            let scale = (cfg.image_scale / 100.0).clamp(0.05, 8.0);
            (asset.width as f32 * scale * 0.5, asset.height as f32 * scale * 0.5)
        } else {
            let bounds = vector_spec(cfg).px_bounds();
            (bounds, bounds)
        }
    }

    /// The animation frame this configuration shows at `elapsed_ms`.
    pub fn preview_frame(&self, cfg: &CrosshairConfig, elapsed_ms: u64) -> usize {
        super::gif::frame_index(&self.frame_delays(), elapsed_ms, cfg.gif_playing)
    }

    /// Milliseconds until the frame changes, so the preview can wake on the
    /// source's own cadence instead of a fixed timer. None when static.
    pub fn preview_due_ms(&self, cfg: &CrosshairConfig, elapsed_ms: u64) -> Option<u64> {
        super::gif::ms_until_next(&self.frame_delays(), elapsed_ms, cfg.gif_playing)
    }

    fn frame_delays(&self) -> Vec<u32> {
        self.asset
            .as_ref()
            .map(|asset| asset.frames.iter().map(|frame| frame.delay_ms).collect())
            .unwrap_or_default()
    }

    pub fn preview_png(&self, cfg: &CrosshairConfig, elapsed_ms: u64) -> Result<Vec<u8>> {
        let (w, h) = (640u32, 360u32);
        // The preview is displayed inside a roughly 400px stage. Geometry is
        // magnified so the preview shows the reticle at its on-screen pixel size.
        let mut preview = cfg.clone();
        let scale = 1.5;
        preview.size *= scale;
        preview.thickness *= scale;
        preview.gap *= scale;
        preview.dot_size *= scale;
        preview.outline_width *= scale;
        preview.image_scale *= scale;
        let center = Vector2 {
            X: w as f32 * 0.5 + preview.offset_x as f32 * scale,
            Y: h as f32 * 0.5 + preview.offset_y as f32 * scale,
        };
        self.engine.render_png(w, h, Rgba::rgb(0x000000).alpha(0.0), |ctx| self.draw(ctx, &preview, center, elapsed_ms))
    }
}

fn frame_at<'a>(asset: &'a ImageAsset, cfg: &CrosshairConfig, elapsed_ms: u64) -> &'a decode::Frame {
    let delays: Vec<u32> = asset.frames.iter().map(|frame| frame.delay_ms).collect();
    let index = super::gif::frame_index(&delays, elapsed_ms, cfg.gif_playing);
    &asset.frames[index.min(asset.frames.len() - 1)]
}

pub fn vector_spec(cfg: &CrosshairConfig) -> VectorSpec {
    VectorSpec {
        style: Style::from_str(&cfg.style),
        size: cfg.size,
        thickness: cfg.thickness,
        gap: cfg.gap,
        rotation: cfg.rotation,
        dot: cfg.dot,
        dot_size: cfg.dot_size,
        outline: cfg.outline,
        outline_width: cfg.outline_width,
        opacity: cfg.opacity,
        color: cfg.color.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_spec_matches_config() {
        let cfg = CrosshairConfig { style: "ring".into(), size: 24.0, thickness: 3.0, dot: true, color: "#FF0000".into(), ..Default::default() };
        let spec = vector_spec(&cfg);
        assert_eq!(spec.style, Style::Ring);
        assert_eq!(spec.size, 24.0);
        assert!(spec.dot);
        assert!(spec.px_bounds() >= 14.0, "including the stroke and outline");
    }
}
