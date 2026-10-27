//! Shared vector reconstruction of the organism visible in the reference JPEG.
//!
//! The supervising task supplied pixel measurements and a contour description.
//! These paths are a reviewable reconstruction, not recovered original artwork.
//! Draw only for the existing application-level owner after its safe-area check.
//! This module does not create companions, state, timers, speech or input owners.

pub const SIZE: f32 = 64.0;
pub const VISIBLE_BOUNDS: [f32; 4] = [1.0, 6.0, 62.0, 51.0];
pub const AWAKE_COLOR: [u8; 4] = [111, 215, 161, 255];
pub const SPEECH_SURFACE: [u8; 4] = [22, 33, 51, 248];
pub const SPEECH_BORDER: [u8; 4] = [63, 83, 119, 255];
pub const SPEECH_RADIUS: f32 = 10.0;

#[derive(Clone, Copy, Debug)]
pub enum Command {
    Move(f32, f32),
    Line(f32, f32),
    Cubic(f32, f32, f32, f32, f32, f32),
    Close,
}

#[derive(Clone, Copy, Debug)]
pub struct Shape {
    pub path: &'static [Command],
    pub fill: Option<[u8; 4]>,
    pub stroke: Option<[u8; 4]>,
    pub stroke_width: f32,
}

use Command::{Close, Cubic as C, Move as M};

const BODY: &[Command] = &[
    M(16., 25.),
    C(14., 18., 16., 10., 21., 10.),
    C(25., 10., 27., 15., 29., 17.),
    C(31., 16., 34., 16., 36., 17.),
    C(39., 13., 41., 8., 45., 10.),
    C(50., 12., 50., 19., 48., 25.),
    C(53., 29., 54., 36., 51., 43.),
    C(48., 51., 40., 55., 32., 55.),
    C(23., 55., 15., 51., 12., 43.),
    C(9., 36., 11., 29., 16., 25.),
    Close,
];
const ORBIT_BACK: &[Command] = &[
    M(5., 47.),
    C(-5., 40., 13., 26., 31., 21.),
    C(48., 16., 63., 15., 61., 22.),
    C(60., 25., 57., 28., 53., 31.),
];
const ORBIT_FRONT: &[Command] = &[
    M(10., 31.),
    C(-4., 41., 3., 53., 24., 50.),
    C(42., 47., 60., 36., 62., 28.),
    C(63., 24., 61., 21., 57., 20.),
];
const EAR_LOOP: &[Command] = &[
    M(20., 19.),
    C(14., 9., 26., 3., 32., 12.),
    C(39., 22., 47., 20., 47., 13.),
    C(47., 5., 35., 8., 33., 16.),
];
const LEFT_EYE: &[Command] = &[
    M(27.5, 33.),
    C(27.5, 36., 23.5, 36., 23.5, 33.),
    C(23.5, 30., 27.5, 30., 27.5, 33.),
    Close,
];
const RIGHT_EYE: &[Command] = &[
    M(42., 33.),
    C(42., 36., 38., 36., 38., 33.),
    C(38., 30., 42., 30., 42., 33.),
    Close,
];
const SMILE: &[Command] = &[M(28., 41.), C(30.5, 44., 34.5, 44., 37., 40.5)];

/// Ordered back-to-front. Translucent broad strokes supply a restrained halo
/// without renderer-specific blur kernels. Use round caps and joins.
pub const SHAPES: &[Shape] = &[
    Shape {
        path: ORBIT_BACK,
        fill: None,
        stroke: Some([91, 147, 255, 18]),
        stroke_width: 7.0,
    },
    Shape {
        path: BODY,
        fill: None,
        stroke: Some([91, 147, 255, 16]),
        stroke_width: 8.0,
    },
    Shape {
        path: BODY,
        fill: None,
        stroke: Some([112, 169, 255, 32]),
        stroke_width: 4.0,
    },
    Shape {
        path: ORBIT_BACK,
        fill: None,
        stroke: Some([112, 167, 255, 175]),
        stroke_width: 1.1,
    },
    Shape {
        path: BODY,
        fill: Some([19, 36, 66, 255]),
        stroke: Some([161, 199, 255, 240]),
        stroke_width: 1.3,
    },
    Shape {
        path: EAR_LOOP,
        fill: None,
        stroke: Some([172, 207, 255, 210]),
        stroke_width: 1.2,
    },
    Shape {
        path: LEFT_EYE,
        fill: Some([223, 241, 255, 255]),
        stroke: None,
        stroke_width: 0.0,
    },
    Shape {
        path: RIGHT_EYE,
        fill: Some([223, 241, 255, 255]),
        stroke: None,
        stroke_width: 0.0,
    },
    Shape {
        path: SMILE,
        fill: None,
        stroke: Some([194, 223, 255, 255]),
        stroke_width: 1.5,
    },
    Shape {
        path: ORBIT_FRONT,
        fill: None,
        stroke: Some([102, 167, 255, 20]),
        stroke_width: 5.0,
    },
    Shape {
        path: ORBIT_FRONT,
        fill: None,
        stroke: Some([184, 217, 255, 225]),
        stroke_width: 1.15,
    },
];

/// Optional SVG transport for native SVG-capable image widgets. Curve data is
/// the same as SHAPES; native path renderers should use SHAPES directly.
pub fn svg() -> String {
    use std::fmt::Write;
    fn color(rgba: [u8; 4]) -> String {
        format!(
            "rgba({},{},{},{:.5})",
            rgba[0],
            rgba[1],
            rgba[2],
            rgba[3] as f32 / 255.
        )
    }
    let mut out = String::from("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 64 64\" width=\"64\" height=\"64\">");
    for shape in SHAPES {
        let mut path = String::new();
        for command in shape.path {
            match *command {
                Command::Move(x, y) => write!(&mut path, "M{x} {y} ").unwrap(),
                Command::Line(x, y) => write!(&mut path, "L{x} {y} ").unwrap(),
                Command::Cubic(a, b, c, d, e, f) => {
                    write!(&mut path, "C{a} {b} {c} {d} {e} {f} ").unwrap()
                }
                Command::Close => path.push('Z'),
            }
        }
        write!(&mut out, "<path d=\"{path}\" fill=\"{}\" stroke=\"{}\" stroke-width=\"{}\" stroke-linecap=\"round\" stroke-linejoin=\"round\"/>",
            shape.fill.map(color).unwrap_or_else(|| "none".into()),
            shape.stroke.map(color).unwrap_or_else(|| "none".into()), shape.stroke_width).unwrap();
    }
    out.push_str("</svg>");
    out
}
