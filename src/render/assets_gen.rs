use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::core::{Interface, PCWSTR, Result};
use windows_numerics::Vector2;
use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Graphics::Direct2D::Common::{D2D1_FIGURE_BEGIN_FILLED, D2D1_FIGURE_END_CLOSED, D2D_RECT_F};
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::{DWRITE_FONT_WEIGHT, DWRITE_FONT_WEIGHT_BOLD, DWRITE_FONT_WEIGHT_NORMAL};
use windows::Win32::Graphics::Imaging::{
    GUID_WICPixelFormat32bppBGRA, WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom,
    WICDecodeMetadataCacheOnDemand,
};

use super::engine::{parse_hex, Engine, Rgba};
use super::vector::{draw, Style, VectorSpec};

pub const PRESET_COLORS: [(&str, &str, Style, u32, bool); 7] = [
    ("plus", "Pro Plus", Style::Plus, 0x00FF66, false),
    ("dot", "Precision Dot", Style::Dot, 0x00E5FF, false),
    ("ring", "Tracking Ring", Style::Ring, 0xFFFFFF, false),
    ("circle_dot", "Ring + Dot", Style::Ring, 0x38FF9C, true),
    ("triangle", "Triangle", Style::Triangle, 0xFF5C8D, false),
    ("peak", "Chevron", Style::Peak, 0xFFD600, false),
    ("tsight", "Open T-Cross", Style::TSight, 0xFFE600, true),
];

fn draw_style_icon(ctx: &ID2D1DeviceContext5, size: f32, spec: &VectorSpec) -> Result<()> {
    let center = Vector2 { X: size * 0.5, Y: size * 0.5 };
    let scale = size / 128.0;
    let mut local = spec.clone();
    local.size = spec.size * scale;
    local.thickness = (spec.thickness * scale).max(1.0);
    local.gap = spec.gap * scale;
    local.dot_size = spec.dot_size * scale;
    local.outline_width = (spec.outline_width * scale).max(1.0);
    draw(ctx, center, &local)
}

fn mix(a: Rgba, b: Rgba, t: f32) -> Rgba {
    Rgba {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

fn rect_f(x: f32, y: f32, w: f32, h: f32) -> D2D_RECT_F {
    D2D_RECT_F { left: x, top: y, right: x + w, bottom: y + h }
}

fn fill(ctx: &ID2D1DeviceContext5, x: f32, y: f32, w: f32, h: f32, color: Rgba) -> Result<()> {
    unsafe {
        let brush = ctx.CreateSolidColorBrush(&color.to_d2d(), None)?;
        ctx.FillRectangle(&rect_f(x, y, w, h), &brush);
    }
    Ok(())
}

fn line(ctx: &ID2D1DeviceContext5, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, color: Rgba) -> Result<()> {
    unsafe {
        let brush = ctx.CreateSolidColorBrush(&color.to_d2d(), None)?;
        ctx.DrawLine(Vector2 { X: x0, Y: y0 }, Vector2 { X: x1, Y: y1 }, &brush, width, None);
    }
    Ok(())
}

fn ellipse(ctx: &ID2D1DeviceContext5, cx: f32, cy: f32, rx: f32, ry: f32, color: Rgba) -> Result<()> {
    unsafe {
        let brush = ctx.CreateSolidColorBrush(&color.to_d2d(), None)?;
        ctx.FillEllipse(&D2D1_ELLIPSE { point: Vector2 { X: cx, Y: cy }, radiusX: rx, radiusY: ry }, &brush);
    }
    Ok(())
}

fn stroke_ellipse(ctx: &ID2D1DeviceContext5, cx: f32, cy: f32, rx: f32, ry: f32, width: f32, color: Rgba) -> Result<()> {
    unsafe {
        let brush = ctx.CreateSolidColorBrush(&color.to_d2d(), None)?;
        ctx.DrawEllipse(&D2D1_ELLIPSE { point: Vector2 { X: cx, Y: cy }, radiusX: rx, radiusY: ry }, &brush, width, None);
    }
    Ok(())
}

fn polygon(ctx: &ID2D1DeviceContext5, points: &[Vector2], color: Rgba) -> Result<()> {
    unsafe {
        let factory: ID2D1Factory1 = ctx.GetFactory()?.cast()?;
        let geometry = factory.CreatePathGeometry()?;
        let sink = geometry.Open()?;
        sink.BeginFigure(points[0], D2D1_FIGURE_BEGIN_FILLED);
        for point in &points[1..] {
            sink.AddLine(*point);
        }
        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
        sink.Close()?;
        let brush = ctx.CreateSolidColorBrush(&color.to_d2d(), None)?;
        ctx.FillGeometry(&geometry, &brush, None::<&ID2D1Brush>);
    }
    Ok(())
}

/// A crisp, symmetrical tactical-range target. No character silhouette and
/// no soft decoration: an octagonal head plate, chamfered steel torso,
/// center-mass rings, and a visible support post.
fn draw_range_target(ctx: &ID2D1DeviceContext5, day: bool) -> Result<()> {
    let (steel, steel_dark, edge, ring) = if day {
        (Rgba::rgb(0x455157), Rgba::rgb(0x20282D), Rgba::rgb(0xB6C6CB), Rgba::rgb(0xDDE8EA))
    } else {
        (Rgba::rgb(0x10171D), Rgba::rgb(0x080C10), Rgba::rgb(0x6A8295), Rgba::rgb(0x9CB4C6))
    };
    let accent = if day { Rgba::rgb(0xE6534E) } else { Rgba::rgb(0x9F3F43) };

    // Support post and base are drawn first so the torso sits in front.
    fill(ctx, 625.0, 458.0, 30.0, 142.0, steel_dark)?;
    line(ctx, 625.0, 458.0, 625.0, 600.0, 1.5, edge.alpha(0.52))?;
    fill(ctx, 566.0, 590.0, 148.0, 20.0, steel_dark)?;
    line(ctx, 566.0, 590.0, 714.0, 590.0, 2.0, edge.alpha(0.50))?;
    line(ctx, 578.0, 610.0, 702.0, 610.0, 2.0, edge.alpha(0.20))?;

    // Octagonal head plate.
    let head = [
        Vector2 { X: 640.0, Y: 122.0 },
        Vector2 { X: 681.0, Y: 140.0 },
        Vector2 { X: 700.0, Y: 181.0 },
        Vector2 { X: 700.0, Y: 232.0 },
        Vector2 { X: 681.0, Y: 272.0 },
        Vector2 { X: 640.0, Y: 289.0 },
        Vector2 { X: 599.0, Y: 272.0 },
        Vector2 { X: 580.0, Y: 232.0 },
        Vector2 { X: 580.0, Y: 181.0 },
        Vector2 { X: 599.0, Y: 140.0 },
    ];
    polygon(ctx, &head, steel)?;
    for pair in head.windows(2) {
        line(ctx, pair[0].X, pair[0].Y, pair[1].X, pair[1].Y, 2.0, edge)?;
    }
    line(ctx, 610.0, 154.0, 670.0, 154.0, 2.0, edge.alpha(0.55))?;
    line(ctx, 596.0, 250.0, 684.0, 250.0, 1.5, edge.alpha(0.30))?;

    // Short neck plate ties the head to the torso without a floating gap.
    fill(ctx, 612.0, 278.0, 56.0, 24.0, steel_dark)?;
    line(ctx, 612.0, 278.0, 668.0, 278.0, 1.5, edge.alpha(0.35))?;

    // Chamfered steel torso with a broad, readable center mass.
    let torso = [
        Vector2 { X: 588.0, Y: 292.0 },
        Vector2 { X: 692.0, Y: 292.0 },
        Vector2 { X: 744.0, Y: 334.0 },
        Vector2 { X: 722.0, Y: 468.0 },
        Vector2 { X: 558.0, Y: 468.0 },
        Vector2 { X: 536.0, Y: 334.0 },
    ];
    polygon(ctx, &torso, steel)?;
    for pair in torso.windows(2) {
        line(ctx, pair[0].X, pair[0].Y, pair[1].X, pair[1].Y, 2.0, edge.alpha(0.72))?;
    }
    line(ctx, 640.0, 292.0, 640.0, 468.0, 1.5, edge.alpha(0.22))?;
    line(ctx, 558.0, 410.0, 722.0, 410.0, 1.2, edge.alpha(0.18))?;

    // Center-mass bullseye: crisp rings, a small accent center, no glow.
    stroke_ellipse(ctx, 640.0, 379.0, 74.0, 74.0, 2.0, ring.alpha(0.55))?;
    stroke_ellipse(ctx, 640.0, 379.0, 54.0, 54.0, 2.0, ring.alpha(0.72))?;
    stroke_ellipse(ctx, 640.0, 379.0, 34.0, 34.0, 2.0, ring)?;
    stroke_ellipse(ctx, 640.0, 379.0, 14.0, 14.0, 2.0, accent)?;
    line(ctx, 640.0, 365.0, 640.0, 393.0, 1.5, accent.alpha(0.75))?;
    line(ctx, 626.0, 379.0, 654.0, 379.0, 1.5, accent.alpha(0.75))?;

    // Side chamfers and support-post brackets keep the silhouette mechanical.
    line(ctx, 588.0, 292.0, 606.0, 330.0, 1.2, edge.alpha(0.32))?;
    line(ctx, 692.0, 292.0, 674.0, 330.0, 1.2, edge.alpha(0.32))?;
    line(ctx, 558.0, 458.0, 590.0, 470.0, 2.0, edge.alpha(0.35))?;
    line(ctx, 722.0, 458.0, 690.0, 470.0, 2.0, edge.alpha(0.35))?;
    Ok(())
}

fn draw_scene_common(ctx: &ID2D1DeviceContext5, day: bool) -> Result<()> {
    let (top, horizon, ground_a, ground_b) = if day {
        (Rgba::rgb(0x6FA8CC), Rgba::rgb(0xEAF3F6), Rgba::rgb(0x8F9A9D), Rgba::rgb(0x535D62))
    } else {
        (Rgba::rgb(0x03050A), Rgba::rgb(0x182536), Rgba::rgb(0x101419), Rgba::rgb(0x07090C))
    };
    for i in 0..18 {
        let t = i as f32 / 17.0;
        let y = 0.0 + t * 396.0;
        let c = mix(top, horizon, t.powf(1.25));
        fill(ctx, 0.0, y, 1280.0, 24.0, c)?;
    }
    fill(ctx, 0.0, 396.0, 1280.0, 324.0, ground_a)?;
    for i in 0..9 {
        let t = i as f32 / 8.0;
        fill(ctx, 0.0, 396.0 + t * 324.0, 1280.0, 40.0, mix(ground_a, ground_b, t))?;
    }

    // Keep the night sky clear: a crisp target beside the moon, not a halo of
    // concentric glow rings competing with the crosshair.
    if !day {
        ellipse(ctx, 778.0, 132.0, 46.0, 46.0, Rgba::rgb(0xD7E2F2).alpha(0.85))?;
        ellipse(ctx, 794.0, 120.0, 42.0, 42.0, Rgba::rgb(0x0A1220).alpha(0.90))?;
        fill(ctx, 474.0, 590.0, 332.0, 26.0, Rgba::rgb(0x020305).alpha(0.42))?;
    }

    // Flanking beveled concrete cover walls.
    let wall = if day { Rgba::rgb(0x5E696E) } else { Rgba::rgb(0x171C22) };
    let wall_dark = if day { Rgba::rgb(0x3D464B) } else { Rgba::rgb(0x0E1216) };
    let edge = if day { Rgba::rgb(0xB9C6C9).alpha(0.45) } else { Rgba::rgb(0x6D8299).alpha(0.28) };
    fill(ctx, 0.0, 268.0, 214.0, 452.0, wall_dark)?;
    fill(ctx, 30.0, 300.0, 160.0, 420.0, wall)?;
    fill(ctx, 190.0, 330.0, 36.0, 390.0, wall_dark)?;
    fill(ctx, 1066.0, 268.0, 214.0, 452.0, wall_dark)?;
    fill(ctx, 1090.0, 300.0, 160.0, 420.0, wall)?;
    fill(ctx, 1054.0, 330.0, 36.0, 390.0, wall_dark)?;
    line(ctx, 30.0, 300.0, 190.0, 300.0, 3.0, edge)?;
    line(ctx, 1090.0, 300.0, 1250.0, 300.0, 3.0, edge)?;
    line(ctx, 30.0, 300.0, 30.0, 720.0, 2.0, edge.alpha(edge.a * 0.55))?;
    line(ctx, 1250.0, 300.0, 1250.0, 720.0, 2.0, edge.alpha(edge.a * 0.55))?;
    for y in (336..690).step_by(42) {
        line(ctx, 34.0, y as f32, 188.0, y as f32 + 3.0, 1.0, edge.alpha(0.12))?;
        line(ctx, 1092.0, y as f32, 1246.0, y as f32 + 3.0, 1.0, edge.alpha(0.12))?;
    }

    // Concrete crates keep real shadow detail in the night scene.
    let crate_col = if day { Rgba::rgb(0x7E898D) } else { Rgba::rgb(0x1E232A) };
    let crate_dark = if day { Rgba::rgb(0x5E6A6F) } else { Rgba::rgb(0x151A20) };
    for (x, y, s) in [(398.0, 548.0, 126.0), (758.0, 558.0, 146.0), (96.0, 590.0, 110.0)] {
        fill(ctx, x, y, s, s * 0.64, crate_dark)?;
        fill(ctx, x + 4.0, y + 4.0, s - 8.0, s * 0.64 - 8.0, crate_col)?;
        line(ctx, x + 8.0, y + 8.0, x + s - 8.0, y + 8.0, 2.0, edge.alpha(0.24))?;
        line(ctx, x + s * 0.5, y + 8.0, x + s * 0.5, y + s * 0.64 - 8.0, 1.2, edge.alpha(0.16))?;
    }

    // Perspective ground lines converge behind the target.
    for i in 0..14 {
        let x = i as f32 / 13.0 * 1280.0;
        line(ctx, 640.0, 398.0, x, 720.0, 1.0, edge.alpha(0.14))?;
    }
    for i in 1..7 {
        let t = i as f32 / 7.0;
        let y = 400.0 + t * t * 320.0;
        line(ctx, 0.0, y, 1280.0, y, 1.0, edge.alpha(0.10))?;
    }
    // Distant haze band above the floor.
    fill(ctx, 0.0, 380.0, 1280.0, 18.0, if day { Rgba::rgb(0xF2F7F8).alpha(0.28) } else { Rgba::rgb(0x283748).alpha(0.20) })?;
    Ok(())
}

fn write_asset(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).map_err(|err| {
        windows::core::Error::new(
            windows::Win32::Foundation::E_FAIL,
            format!("cannot write {}: {err}", path.display()),
        )
    })
}

// Measured from the supplied 1376x768 target plates. The faint aiming circle
// sits above the geometric centre; these normalized coordinates keep the
// normalizer focused on the target rather than the middle of the whole scene.
const SCENE_FOCUS: [(&str, f32, f32); 2] = [
    ("scene_day.png", 0.5, 0.358_072_92),
    ("scene_night.png", 0.499_216_73, 0.357_841_41),
];

fn scene_focus(name: &str) -> (f32, f32) {
    SCENE_FOCUS
        .iter()
        .find(|(file, _, _)| *file == name)
        .map(|(_, x, y)| (*x, *y))
        .unwrap_or((0.5, 0.5))
}

fn decode_png_bgra(engine: &Engine, path: &Path) -> Result<(u32, u32, Vec<u8>)> {
    unsafe {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let decoder = engine.factory.CreateDecoderFromFilename(
            PCWSTR(wide.as_ptr()),
            None,
            GENERIC_READ,
            WICDecodeMetadataCacheOnDemand,
        )?;
        let frame = decoder.GetFrame(0)?;
        let mut width = 0u32;
        let mut height = 0u32;
        frame.GetSize(&mut width, &mut height)?;
        if width == 0 || height == 0 {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_FAIL,
                format!("{} has no pixels", path.display()),
            ));
        }
        let converter = engine.factory.CreateFormatConverter()?;
        converter.Initialize(
            &frame,
            &GUID_WICPixelFormat32bppBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )?;
        let mut pixels = vec![0u8; width as usize * height as usize * 4];
        converter.CopyPixels(std::ptr::null(), width * 4, &mut pixels)?;
        Ok((width, height, pixels))
    }
}

/// Normalize a user-supplied scene to the exact embedded size. An image that is
/// already 1280x720 is left byte-for-byte untouched. Otherwise a 16:9 crop is
/// centred on the target plate and scaled with the same high-quality cubic
/// filtering used by the renderer, placing the aiming circle at (640, 360).
fn normalize_scene(engine: &Engine, path: &Path, name: &str) -> Result<()> {
    let (source_w, source_h, pixels) = decode_png_bgra(engine, path)?;
    if source_w == 1280 && source_h == 720 {
        println!("[gen_assets] {name}: preserving existing 1280x720 scene");
        return Ok(());
    }

    let (focus_x, focus_y) = scene_focus(name);
    let focus_x = focus_x * source_w as f32;
    let focus_y = focus_y * source_h as f32;
    let mut crop_h = (source_h as f32)
        .min(2.0 * focus_y)
        .min(2.0 * (source_h as f32 - focus_y))
        .min(source_w as f32 * 9.0 / 16.0);
    if crop_h < 2.0 {
        crop_h = source_h as f32;
    }
    let crop_w = (crop_h * 16.0 / 9.0).min(source_w as f32);
    let left = (focus_x - crop_w * 0.5).clamp(0.0, source_w as f32 - crop_w);
    let top = (focus_y - crop_h * 0.5).clamp(0.0, source_h as f32 - crop_h);
    let source = D2D_RECT_F { left, top, right: left + crop_w, bottom: top + crop_h };
    let destination = D2D_RECT_F { left: 0.0, top: 0.0, right: 1280.0, bottom: 720.0 };
    let bitmap = engine.create_bitmap(source_w, source_h, &pixels)?;
    let png = engine.render_png(1280, 720, Rgba::rgb(0x000000), |ctx| unsafe {
        ctx.DrawBitmap(
            &bitmap,
            Some(&destination),
            1.0,
            D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
            Some(&source),
            None,
        );
        Ok(())
    })?;
    write_asset(path, &png)?;
    println!(
        "[gen_assets] {name}: normalized {source_w}x{source_h} -> 1280x720 (target focus {focus_x:.1},{focus_y:.1})"
    );
    Ok(())
}

pub fn generate(engine: &Engine, root: &Path, overwrite_scenes: bool) -> Result<()> {
    std::fs::create_dir_all(root.join("presets")).map_err(|err| {
        windows::core::Error::new(
            windows::Win32::Foundation::E_FAIL,
            format!("cannot create {}: {err}", root.join("presets").display()),
        )
    })?;

    // Application icon: 512 PNG and a multi-size ICO, all rendered with the
    // DirectWrite color-font flag so the full-colour bow emoji stays crisp.
    println!("[gen_assets] engine ready");
    for (size, file) in [(512u32, "icon.png")] {
        println!("[gen_assets] text format {size}");
        let layout = engine.text("Segoe UI Emoji", size as f32 * 0.72, "\u{1F3F9}", Rgba::rgb(0xFFFFFF), size as f32, size as f32, DWRITE_FONT_WEIGHT_NORMAL)?;
        println!("[gen_assets] render png {size}");
        let png = engine.render_png(size, size, Rgba::rgb(0x000000).alpha(0.0), |_ctx| {
            engine.draw_text(&layout, Rgba::rgb(0xFFFFFF), 0.0, 0.0, true)
        })?;
        write_asset(&root.join(file), &png)?;
    }

    println!("[gen_assets] icon.png done");
    let ico_sizes = [16u32, 24, 32, 48, 64, 128, 256];
    let mut frames: Vec<(u32, Vec<u8>)> = Vec::new();
    for size in ico_sizes {
        let layout = engine.text("Segoe UI Emoji", size as f32 * 0.74, "\u{1F3F9}", Rgba::rgb(0xFFFFFF), size as f32, size as f32, DWRITE_FONT_WEIGHT_NORMAL)?;
        let png = engine.render_png(size, size, Rgba::rgb(0x000000).alpha(0.0), |_ctx| {
            engine.draw_text(&layout, Rgba::rgb(0xFFFFFF), 0.0, 0.0, true)
        })?;
        frames.push((size, png));
    }
    let mut ico = Vec::new();
    ico.extend_from_slice(&0u16.to_le_bytes());
    ico.extend_from_slice(&1u16.to_le_bytes());
    ico.extend_from_slice(&(frames.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * frames.len() as u32;
    for (size, png) in &frames {
        ico.push(if *size >= 256 { 0 } else { *size as u8 });
        ico.push(if *size >= 256 { 0 } else { *size as u8 });
        ico.push(0);
        ico.push(0);
        ico.extend_from_slice(&1u16.to_le_bytes());
        ico.extend_from_slice(&32u16.to_le_bytes());
        ico.extend_from_slice(&(png.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for (_, png) in &frames {
        ico.extend_from_slice(png);
    }
    write_asset(&root.join("icon.ico"), &ico)?;

    println!("[gen_assets] icon.ico done");
    // Preset swatches: exact colours, transparent 128px 4x renders.
    for (key, _label, style, color, dot) in PRESET_COLORS {
        // Icon geometry stays deliberately larger than the in-game preset so
        // every shape remains legible at 26 px in the preset picker.
        let icon_size = match style {
            Style::Dot => 48.0,
            Style::Ring => 84.0,
            Style::Triangle => 82.0,
            Style::Peak => 76.0,
            _ => 78.0,
        };
        let spec = VectorSpec {
            style,
            size: icon_size,
            thickness: 8.0,
            gap: if style == Style::TSight { 34.0 } else { 12.0 },
            rotation: 0.0,
            dot,
            dot_size: if dot && style == Style::TSight { 10.0 } else if dot { 12.0 } else { 10.0 },
            outline: true,
            outline_width: 4.0,
            opacity: 100.0,
            color: format!("#{color:06X}"),
        };
        let png = engine.render_png(128, 128, Rgba::rgb(0x000000).alpha(0.0), |ctx| draw_style_icon(ctx, 128.0, &spec))?;
        write_asset(&root.join("presets").join(format!("{key}.png")), &png)?;
    }

    println!("[gen_assets] preset icons done");
    for (name, day) in [("scene_day.png", true), ("scene_night.png", false)] {
        let scene_path = root.join(name);
        if !overwrite_scenes && scene_path.exists() {
            normalize_scene(engine, &scene_path, name)?;
            continue;
        }
        if !overwrite_scenes {
            println!("[gen_assets] {name}: missing, generating a new scene");
        }
        let target = engine.target(1280, 720)?;
        engine.begin(&target, Rgba::rgb(if day { 0x6FA8CC } else { 0x03050A }));
        draw_scene_common(&engine.ctx, day)?;
        draw_range_target(&engine.ctx, day)?;
        engine.end()?;
        let (straight, _) = engine.readback(&target)?;
        let mean = mean_luma(&straight);
        let shadow = region_luma(&straight, 1280, 40, 590, 150, 110);
        println!("[gen_assets] {name}: mean_luma={mean:.1} shadow_region={shadow:.1}");
        if day && mean < 90.0 {
            return Err(windows::core::Error::new(windows::Win32::Foundation::E_FAIL, format!("day scene too dark: {mean:.1}")));
        }
        if !day && (shadow < 255.0 * 0.03 || shadow > 255.0 * 0.12) {
            return Err(windows::core::Error::new(windows::Win32::Foundation::E_FAIL, format!("night shadow detail outside 3-12%: {shadow:.1}")));
        }
        let png = engine.encode_png(1280, 720, &straight)?;
        write_asset(&scene_path, &png)?;
    }
    Ok(())
}

fn mean_luma(bgra: &[u8]) -> f32 {
    let mut sum = 0f64;
    let mut count = 0u64;
    for px in bgra.chunks_exact(4) {
        let b = px[0] as f64;
        let g = px[1] as f64;
        let r = px[2] as f64;
        sum += 0.2126 * r + 0.7152 * g + 0.0722 * b;
        count += 1;
    }
    (sum / count.max(1) as f64) as f32
}

fn region_luma(bgra: &[u8], width: u32, x: u32, y: u32, w: u32, h: u32) -> f32 {
    let mut sum = 0f64;
    let mut count = 0u64;
    for yy in y..(y + h).min(720) {
        for xx in x..(x + w).min(width) {
            let i = ((yy * width + xx) * 4) as usize;
            let b = bgra[i] as f64;
            let g = bgra[i + 1] as f64;
            let r = bgra[i + 2] as f64;
            sum += 0.2126 * r + 0.7152 * g + 0.0722 * b;
            count += 1;
        }
    }
    (sum / count.max(1) as f64) as f32
}

#[allow(dead_code)]
pub fn scene_preset_colors() -> Vec<(String, u32)> {
    PRESET_COLORS.iter().map(|(_, label, _, color, _)| (label.to_string(), *color)).collect()
}

#[allow(dead_code)]
pub fn parse_color_hex(text: &str) -> u32 {
    parse_hex(text)
}

#[allow(dead_code)]
pub fn bold() -> DWRITE_FONT_WEIGHT {
    DWRITE_FONT_WEIGHT_BOLD
}
