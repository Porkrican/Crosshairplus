use windows::core::Result;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_FIGURE_BEGIN_FILLED, D2D1_FIGURE_END_CLOSED, D2D1_FIGURE_END_OPEN,
};
use windows::Win32::Graphics::Direct2D::{
    ID2D1DeviceContext5, ID2D1Factory, ID2D1PathGeometry, ID2D1StrokeStyle,
    D2D1_CAP_STYLE_FLAT, D2D1_DASH_STYLE_SOLID, D2D1_ELLIPSE, D2D1_LINE_JOIN_MITER,
    D2D1_STROKE_STYLE_PROPERTIES,
};
use windows_numerics::{Matrix3x2, Vector2};

use super::engine::{parse_hex, Rgba};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Plus,
    Dot,
    Ring,
    TSight,
    Triangle,
    Peak,
}

impl Style {
    // Used by the app's crosshair config; generator links the shared module directly.
    #[allow(dead_code)]
    pub fn from_str(value: &str) -> Self {
        match value {
            "dot" => Self::Dot,
            "ring" => Self::Ring,
            "tsight" => Self::TSight,
            "triangle" => Self::Triangle,
            "peak" => Self::Peak,
            // Removed built-ins migrate to the closest surviving shape.
            "casual" | "hairline" => Self::Plus,
            _ => Self::Plus,
        }
    }
}

#[derive(Clone, Debug)]
pub struct VectorSpec {
    pub style: Style,
    pub size: f32,
    pub thickness: f32,
    pub gap: f32,
    pub rotation: f32,
    pub dot: bool,
    pub dot_size: f32,
    pub outline: bool,
    pub outline_width: f32,
    pub opacity: f32,
    pub color: String,
}

impl Default for VectorSpec {
    fn default() -> Self {
        Self {
            style: Style::Plus,
            size: 18.0,
            thickness: 2.0,
            gap: 4.0,
            rotation: 0.0,
            dot: false,
            dot_size: 2.0,
            outline: true,
            outline_width: 1.0,
            opacity: 100.0,
            color: "#38FF9C".into(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Segment {
    pub a: Vector2,
    pub b: Vector2,
}

#[derive(Clone, Debug)]
pub struct PathSpec {
    pub points: Vec<Vector2>,
    pub closed: bool,
}

pub fn rotation_matrix(center: Vector2, degrees: f32) -> Matrix3x2 {
    let rad = degrees.to_radians();
    let (sin, cos) = rad.sin_cos();
    Matrix3x2 {
        M11: cos,
        M12: sin,
        M21: -sin,
        M22: cos,
        M31: center.X - center.X * cos + center.Y * sin,
        M32: center.Y - center.X * sin - center.Y * cos,
    }
}

impl VectorSpec {
    #[allow(dead_code)]
    pub fn px_bounds(&self) -> f32 {
        let mut half = self.size.max(self.dot_size) * 0.5 + self.thickness * 0.5;
        if self.outline {
            half += self.outline_width + 1.0;
        }
        half + 4.0
    }

    fn outline_amount(&self) -> f32 {
        if self.outline {
            self.outline_width.clamp(0.0, 6.0)
        } else {
            0.0
        }
    }
}

fn unit(from: Vector2, to: Vector2) -> Vector2 {
    let dx = to.X - from.X;
    let dy = to.Y - from.Y;
    let len = (dx * dx + dy * dy).sqrt().max(0.0001);
    Vector2 { X: dx / len, Y: dy / len }
}

fn shifted(point: Vector2, direction: Vector2, amount: f32) -> Vector2 {
    Vector2 { X: point.X + direction.X * amount, Y: point.Y + direction.Y * amount }
}

/// The disconnected Plus / T-Sight arms, measured from the centre.
pub fn straight_arms(spec: &VectorSpec, center: Vector2) -> Vec<Segment> {
    let size = spec.size.clamp(2.0, 48.0);
    let half = size * 0.5;
    let gap = (spec.gap.clamp(0.0, 24.0) * 0.5).min(half);
    let mut arms = vec![
        Segment {
            a: Vector2 { X: center.X - half, Y: center.Y },
            b: Vector2 { X: center.X - gap, Y: center.Y },
        },
        Segment {
            a: Vector2 { X: center.X + gap, Y: center.Y },
            b: Vector2 { X: center.X + half, Y: center.Y },
        },
    ];
    if spec.style == Style::Plus {
        arms.push(Segment {
            a: Vector2 { X: center.X, Y: center.Y - half },
            b: Vector2 { X: center.X, Y: center.Y - gap },
        });
    }
    arms.push(Segment {
        a: Vector2 { X: center.X, Y: center.Y + gap },
        b: Vector2 { X: center.X, Y: center.Y + half },
    });
    arms
}

/// One continuous figure for Triangle (closed) and Peak/Chevron (open).
pub fn path_spec(spec: &VectorSpec, center: Vector2) -> Option<PathSpec> {
    let size = spec.size.clamp(2.0, 48.0);
    let half = size * 0.5;
    match spec.style {
        Style::Triangle => Some(PathSpec {
            points: vec![
                Vector2 { X: center.X, Y: center.Y - half },
                Vector2 { X: center.X - half, Y: center.Y + half },
                Vector2 { X: center.X + half, Y: center.Y + half },
            ],
            closed: true,
        }),
        Style::Peak => Some(PathSpec {
            points: vec![
                Vector2 { X: center.X - half * 1.45, Y: center.Y + half * 0.55 },
                Vector2 { X: center.X, Y: center.Y - half },
                Vector2 { X: center.X + half * 1.45, Y: center.Y + half * 0.55 },
            ],
            closed: false,
        }),
        _ => None,
    }
}

/// Extend both ends so the dark outline closes over flat line caps.
pub fn extend_segment(segment: Segment, amount: f32) -> Segment {
    if amount <= 0.0 {
        return segment;
    }
    let dir = unit(segment.a, segment.b);
    Segment {
        a: shifted(segment.a, dir, -amount),
        b: shifted(segment.b, dir, amount),
    }
}

/// Outline geometry for Peak extends its two base tips outward along each leg.
pub fn outline_path_spec(spec: &VectorSpec, center: Vector2) -> Option<PathSpec> {
    let mut path = path_spec(spec, center)?;
    let amount = spec.outline_amount();
    if amount <= 0.0 || path.closed || path.points.len() != 3 {
        return Some(path);
    }
    let apex = path.points[1];
    for index in [0usize, 2] {
        let dir = unit(apex, path.points[index]);
        path.points[index] = shifted(path.points[index], dir, amount);
    }
    Some(path)
}

pub fn dot_outer_radius(dot_size: f32, outline: f32) -> f32 {
    (dot_size.clamp(1.0, 16.0) * 0.5).max(0.5) + outline.max(0.0)
}

fn stroke_style(ctx: &ID2D1DeviceContext5) -> Result<ID2D1StrokeStyle> {
    unsafe {
        let factory: ID2D1Factory = ctx.GetFactory()?;
        let properties = D2D1_STROKE_STYLE_PROPERTIES {
            startCap: D2D1_CAP_STYLE_FLAT,
            endCap: D2D1_CAP_STYLE_FLAT,
            dashCap: D2D1_CAP_STYLE_FLAT,
            lineJoin: D2D1_LINE_JOIN_MITER,
            miterLimit: 10.0,
            dashStyle: D2D1_DASH_STYLE_SOLID,
            dashOffset: 0.0,
        };
        factory.CreateStrokeStyle(&properties, None)
    }
}

fn path_geometry(ctx: &ID2D1DeviceContext5, path: &PathSpec) -> Result<ID2D1PathGeometry> {
    unsafe {
        let factory: ID2D1Factory = ctx.GetFactory()?;
        let geometry = factory.CreatePathGeometry()?;
        let sink = geometry.Open()?;
        if let Some(first) = path.points.first() {
            sink.BeginFigure(*first, D2D1_FIGURE_BEGIN_FILLED);
            for point in &path.points[1..] {
                sink.AddLine(*point);
            }
            sink.EndFigure(if path.closed { D2D1_FIGURE_END_CLOSED } else { D2D1_FIGURE_END_OPEN });
        }
        sink.Close()?;
        Ok(geometry)
    }
}

fn draw_segments(
    ctx: &ID2D1DeviceContext5,
    segments: &[Segment],
    brush: &windows::Win32::Graphics::Direct2D::ID2D1SolidColorBrush,
    width: f32,
) -> Result<()> {
    unsafe {
        for segment in segments {
            ctx.DrawLine(segment.a, segment.b, brush, width, None);
        }
    }
    Ok(())
}

pub fn draw(ctx: &ID2D1DeviceContext5, center: Vector2, spec: &VectorSpec) -> Result<()> {
    unsafe {
        let opacity = (spec.opacity / 100.0).clamp(0.0, 1.0);
        let color = Rgba::rgb(parse_hex(&spec.color)).alpha(opacity);
        let dark = Rgba::rgb(0x000000).alpha(opacity * 0.92);
        let brush = ctx.CreateSolidColorBrush(&color.to_d2d(), None)?;
        let dark_brush = ctx.CreateSolidColorBrush(&dark.to_d2d(), None)?;
        let transform = rotation_matrix(center, spec.rotation);
        ctx.SetTransform(&transform);

        let size = spec.size.clamp(2.0, 48.0);
        let thickness = spec.thickness.clamp(1.0, 10.0);
        let half = size * 0.5;
        let outline = spec.outline_amount();

        match spec.style {
            Style::Plus | Style::TSight => {
                let arms = straight_arms(spec, center);
                if outline > 0.0 {
                    let outlined: Vec<Segment> =
                        arms.iter().map(|segment| extend_segment(*segment, outline)).collect();
                    draw_segments(ctx, &outlined, &dark_brush, thickness + outline * 2.0)?;
                }
                draw_segments(ctx, &arms, &brush, thickness)?;
            }
            Style::Dot => {
                if outline > 0.0 {
                    let ring = D2D1_ELLIPSE {
                        point: center,
                        radiusX: half + outline,
                        radiusY: half + outline,
                    };
                    ctx.FillEllipse(&ring, &dark_brush);
                }
                let dot = D2D1_ELLIPSE { point: center, radiusX: half, radiusY: half };
                ctx.FillEllipse(&dot, &brush);
            }
            Style::Ring => {
                if outline > 0.0 {
                    let dark_ring = D2D1_ELLIPSE { point: center, radiusX: half, radiusY: half };
                    ctx.DrawEllipse(&dark_ring, &dark_brush, thickness + outline * 2.0, None);
                }
                let ring = D2D1_ELLIPSE { point: center, radiusX: half, radiusY: half };
                ctx.DrawEllipse(&ring, &brush, thickness, None);
            }
            Style::Triangle | Style::Peak => {
                let style = stroke_style(ctx)?;
                if outline > 0.0 {
                    let outlined = outline_path_spec(spec, center).unwrap_or_else(|| {
                        path_spec(spec, center).expect("path styles always produce geometry")
                    });
                    let geometry = path_geometry(ctx, &outlined)?;
                    ctx.DrawGeometry(&geometry, &dark_brush, thickness + outline * 2.0, Some(&style));
                }
                let path = path_spec(spec, center).expect("path styles always produce geometry");
                let geometry = path_geometry(ctx, &path)?;
                ctx.DrawGeometry(&geometry, &brush, thickness, Some(&style));
            }
        }

        if spec.dot {
            let dot_r = dot_outer_radius(spec.dot_size, 0.0);
            if outline > 0.0 {
                let outer = D2D1_ELLIPSE {
                    point: center,
                    radiusX: dot_outer_radius(spec.dot_size, outline),
                    radiusY: dot_outer_radius(spec.dot_size, outline),
                };
                ctx.FillEllipse(&outer, &dark_brush);
            }
            let e = D2D1_ELLIPSE { point: center, radiusX: dot_r, radiusY: dot_r };
            ctx.FillEllipse(&e, &brush);
        }
        ctx.SetTransform(&Matrix3x2::identity());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(style: Style) -> VectorSpec {
        VectorSpec {
            style,
            size: 17.0,
            thickness: 2.0,
            gap: 8.0,
            outline: true,
            outline_width: 1.0,
            ..Default::default()
        }
    }

    fn center() -> Vector2 {
        Vector2 { X: 100.0, Y: 100.0 }
    }

    #[test]
    fn plus_has_four_arms_and_outline_covers_both_tips() {
        let arms = straight_arms(&spec(Style::Plus), center());
        assert_eq!(arms.len(), 4);
        let outlined: Vec<Segment> = arms.iter().map(|s| extend_segment(*s, 1.0)).collect();
        for (arm, extended) in arms.iter().zip(outlined.iter()) {
            let original = ((arm.b.X - arm.a.X).powi(2) + (arm.b.Y - arm.a.Y).powi(2)).sqrt();
            let grown = ((extended.b.X - extended.a.X).powi(2)
                + (extended.b.Y - extended.a.Y).powi(2))
            .sqrt();
            assert!((grown - (original + 2.0)).abs() < 0.001, "each tip must grow by outline width");
        }
        assert!(outlined[0].a.X < arms[0].a.X, "outer tip must extend outward");
        assert!(outlined[0].b.X > arms[0].b.X, "inner tip must extend toward the gap");
    }

    #[test]
    fn tsight_has_three_arms_shared_center_and_dot_clearance() {
        let spec = spec(Style::TSight);
        let arms = straight_arms(&spec, center());
        assert_eq!(arms.len(), 3);
        assert!(arms[0].a.Y == 100.0 && arms[1].a.Y == 100.0, "side arms sit on the centre line");
        assert!(arms[2].a.X == 100.0 && arms[2].b.X == 100.0, "the down arm is vertical");
        let gap_half = spec.gap * 0.5;
        let dot_outer = dot_outer_radius(spec.dot_size, spec.outline_width);
        assert!(
            gap_half > dot_outer + spec.outline_width,
            "the dot outline must clear the extended arm outline"
        );
    }

    #[test]
    fn triangle_is_one_closed_three_point_figure() {
        let path = path_spec(&spec(Style::Triangle), center()).unwrap();
        assert!(path.closed);
        assert_eq!(path.points.len(), 3);
        assert_eq!(path.points[1].Y, path.points[2].Y, "base corners stay symmetric");
        assert!(path.points[0].Y < path.points[1].Y, "apex is above the base");
    }

    #[test]
    fn peak_shares_one_apex_and_extends_only_its_base_for_the_outline() {
        let spec = spec(Style::Peak);
        let path = path_spec(&spec, center()).unwrap();
        assert!(!path.closed);
        assert_eq!(path.points.len(), 3);
        assert_eq!(path.points[1].X, 100.0, "the apex is a single shared point");
        let outline = outline_path_spec(&spec, center()).unwrap();
        assert_eq!(outline.points[1].X, path.points[1].X);
        assert_eq!(outline.points[1].Y, path.points[1].Y);
        assert!(outline.points[0].X < path.points[0].X, "left base tip extends outward");
        assert!(outline.points[2].X > path.points[2].X, "right base tip extends outward");
    }

    #[test]
    fn dot_outline_radius_is_larger_than_the_dot() {
        assert!(dot_outer_radius(4.0, 1.0) > 2.0);
        assert_eq!(dot_outer_radius(1.0, 0.0), 0.5);
    }
}
