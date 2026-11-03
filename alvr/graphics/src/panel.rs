//! The welcome panel of the lobby: AirFrameXR header, connection status and the height offset
//! (minus / plus / reset), drawn on the CPU into the lobby's HUD texture. It replaces the plain text
//! HUD; the lobby still draws it as four quads around the user, and `panel_hit` tells which of the
//! three buttons a pointing ray lands on, in the same geometry.
//!
//! The static part (card, header, mark, labels) is rasterised once; every change of the state draws
//! the dynamic part (status text, numbers, button states) over a copy of it.

use alvr_common::glam::{Quat, Vec2, Vec3};
use glyph_brush_layout::{
    ab_glyph::{Font, FontRef, ScaleFont},
    FontId, GlyphPositioner, HorizontalAlign, Layout, SectionGeometry, SectionText, VerticalAlign,
};
use std::f32::consts::FRAC_PI_2;

pub(crate) const HUD_DIST: f32 = 5.0;
pub(crate) const HUD_SIDE: f32 = 3.5;
pub(crate) const HUD_TEXTURE_SIDE: usize = 1024;

const SIDE: i32 = HUD_TEXTURE_SIDE as i32;
const MARK_SIDE: i32 = 192;
const MARK_RGBA: &[u8] = include_bytes!("../resources/airframexr_mark.rgba");
const _: () = assert!(MARK_RGBA.len() == (MARK_SIDE * MARK_SIDE * 4) as usize);

// Hit targets get this much slack around the drawn button (texture pixels).
const HIT_SLACK: f32 = 14.0;

// Brand colours (branding/brand-guide/tokens.css)
type Rgba = [u8; 4];
const NAVY: Rgba = [7, 20, 42, 240];
const CARD: Rgba = [11, 35, 66, 255];
const LINE: Rgba = [27, 58, 99, 255];
const ELECTRIC: Rgba = [20, 117, 255, 255];
const CYAN: Rgba = [0, 230, 245, 255];
const WHITE: Rgba = [244, 250, 255, 255];
const MUTED: Rgba = [157, 180, 203, 255];
const BUTTON: Rgba = [18, 52, 92, 255];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PanelButton {
    Minus,
    Plus,
    Reset,
}

#[derive(Clone, Copy)]
struct Rect {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

const fn rect(x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
    Rect { x0, y0, x1, y1 }
}

const MINUS_RECT: Rect = rect(96.0, 668.0, 276.0, 828.0);
const PLUS_RECT: Rect = rect(748.0, 668.0, 928.0, 828.0);
const RESET_RECT: Rect = rect(392.0, 846.0, 632.0, 898.0);

fn button_rect(button: PanelButton) -> Rect {
    match button {
        PanelButton::Minus => MINUS_RECT,
        PanelButton::Plus => PLUS_RECT,
        PanelButton::Reset => RESET_RECT,
    }
}

/// Everything the panel shows. Compare two states to know if the texture has to be redrawn.
#[derive(Clone, PartialEq, Default)]
pub struct PanelState {
    /// "AirFrameXR client <version>\nhostname: ..\nIP: .." (the lines before the status)
    pub info: String,
    pub status: String,
    /// Height of the headset above the floor as the headset reports it, in cm
    pub headset_height_cm: Option<i32>,
    pub offset_cm: i32,
    pub hovered: Option<PanelButton>,
    pub pressed: Option<PanelButton>,
}

impl PanelState {
    /// The connection message has the info lines, a blank line, then the status text.
    pub fn set_hud_message(&mut self, message: &str) {
        match message.split_once("\n\n") {
            Some((info, status)) => {
                self.info = info.to_owned();
                self.status = status.to_owned();
            }
            None => {
                self.info.clear();
                self.status = message.to_owned();
            }
        }
    }
}

pub struct PanelHit {
    pub point: Vec3,
    pub distance: f32,
    pub button: Option<PanelButton>,
}

/// Where a ray (in the lobby's reference space) meets the panel, as the four HUD quads are placed:
/// rotated by 90 degrees steps around the user, `HUD_DIST` away, resting on the floor.
pub fn panel_hit(origin: Vec3, dir: Vec3) -> Option<PanelHit> {
    let dir = dir.normalize_or_zero();
    let mut best: Option<PanelHit> = None;

    for i in 0..4 {
        let inverse = Quat::from_rotation_y(FRAC_PI_2 * i as f32).inverse();
        let o = inverse * origin;
        let d = inverse * dir;

        if d.z >= -1e-4 {
            continue;
        }
        let t = (-HUD_DIST - o.z) / d.z;
        if t <= 0.0 {
            continue;
        }
        let p = o + d * t;
        if p.x.abs() > HUD_SIDE / 2.0 || p.y < 0.0 || p.y > HUD_SIDE {
            continue;
        }

        let px = Vec2::new(
            (p.x / HUD_SIDE + 0.5) * HUD_TEXTURE_SIDE as f32,
            (0.5 - (p.y - HUD_SIDE / 2.0) / HUD_SIDE) * HUD_TEXTURE_SIDE as f32,
        );
        if best.as_ref().is_none_or(|b| t < b.distance) {
            best = Some(PanelHit {
                point: origin + dir * t,
                distance: t,
                button: pick_button(px),
            });
        }
    }

    best
}

fn pick_button(px: Vec2) -> Option<PanelButton> {
    [PanelButton::Minus, PanelButton::Plus, PanelButton::Reset]
        .into_iter()
        .find(|b| {
            let r = button_rect(*b);
            px.x >= r.x0 - HIT_SLACK
                && px.x <= r.x1 + HIT_SLACK
                && px.y >= r.y0 - HIT_SLACK
                && px.y <= r.y1 + HIT_SLACK
        })
}

struct Canvas {
    px: Vec<u8>,
}

impl Canvas {
    // Straight-alpha "over" of a colour with an extra coverage factor
    fn blend(&mut self, x: i32, y: i32, color: Rgba, coverage: f32) {
        if x < 0 || y < 0 || x >= SIDE || y >= SIDE {
            return;
        }
        let sa = color[3] as f32 / 255.0 * coverage;
        if sa <= 0.0 {
            return;
        }
        let i = ((y * SIDE + x) * 4) as usize;
        let da = self.px[i + 3] as f32 / 255.0;
        let oa = sa + da * (1.0 - sa);
        for k in 0..3 {
            let dst = self.px[i + k] as f32;
            self.px[i + k] = ((color[k] as f32 * sa + dst * da * (1.0 - sa)) / oa).round() as u8;
        }
        self.px[i + 3] = (oa * 255.0).round() as u8;
    }

    fn fill_round_rect(&mut self, r: Rect, radius: f32, color: Rgba) {
        let (cx, cy) = ((r.x0 + r.x1) / 2.0, (r.y0 + r.y1) / 2.0);
        let (hx, hy) = ((r.x1 - r.x0) / 2.0, (r.y1 - r.y0) / 2.0);
        let radius = radius.min(hx).min(hy);

        for y in (r.y0.floor() as i32).max(0)..(r.y1.ceil() as i32).min(SIDE) {
            for x in (r.x0.floor() as i32).max(0)..(r.x1.ceil() as i32).min(SIDE) {
                let qx = (x as f32 + 0.5 - cx).abs() - (hx - radius);
                let qy = (y as f32 + 0.5 - cy).abs() - (hy - radius);
                let dist = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - radius;
                self.blend(x, y, color, (0.5 - dist).clamp(0.0, 1.0));
            }
        }
    }

    // A filled round rectangle with an outline of `border` pixels
    fn framed_round_rect(&mut self, r: Rect, radius: f32, border: f32, fill: Rgba, frame: Rgba) {
        self.fill_round_rect(r, radius, frame);
        self.fill_round_rect(
            rect(r.x0 + border, r.y0 + border, r.x1 - border, r.y1 - border),
            (radius - border).max(0.0),
            fill,
        );
    }

    fn draw_mark(&mut self, x: i32, y: i32) {
        for my in 0..MARK_SIDE {
            for mx in 0..MARK_SIDE {
                let i = ((my * MARK_SIDE + mx) * 4) as usize;
                let a = MARK_RGBA[i + 3];
                if a != 0 {
                    self.blend(
                        x + mx,
                        y + my,
                        [MARK_RGBA[i], MARK_RGBA[i + 1], MARK_RGBA[i + 2], a],
                        1.0,
                    );
                }
            }
        }
    }

    #[expect(clippy::too_many_arguments)]
    fn draw_text(
        &mut self,
        font: &FontRef,
        text: &str,
        size: f32,
        color: Rgba,
        position: (f32, f32),
        h_align: HorizontalAlign,
        v_align: VerticalAlign,
    ) {
        let glyphs = Layout::default()
            .h_align(h_align)
            .v_align(v_align)
            .calculate_glyphs(
                &[font],
                &SectionGeometry {
                    screen_position: position,
                    ..Default::default()
                },
                &[SectionText {
                    text,
                    scale: size.into(),
                    font_id: FontId(0),
                }],
            );

        let scaled_font = font.as_scaled(size);
        for section_glyph in glyphs {
            if let Some(outlined) = scaled_font.outline_glyph(section_glyph.glyph) {
                let bounds = outlined.px_bounds();
                outlined.draw(|x, y, coverage| {
                    self.blend(
                        x as i32 + bounds.min.x as i32,
                        y as i32 + bounds.min.y as i32,
                        color,
                        coverage,
                    );
                });
            }
        }
    }

    fn draw_button(&mut self, font: &FontRef, button: PanelButton, state: &PanelState) {
        let r = button_rect(button);
        let (fill, frame, ink) = if state.pressed == Some(button) {
            (CYAN, CYAN, NAVY)
        } else if state.hovered == Some(button) {
            (ELECTRIC, ICE_ISH, WHITE)
        } else {
            (BUTTON, ELECTRIC, WHITE)
        };
        self.framed_round_rect(r, 26.0, 4.0, fill, frame);

        let (cx, cy) = ((r.x0 + r.x1) / 2.0, (r.y0 + r.y1) / 2.0);
        // Minus and plus are drawn as bars, not glyphs: the same weight at any font
        let bar = |canvas: &mut Canvas, w: f32, h: f32| {
            canvas.fill_round_rect(
                rect(cx - w / 2.0, cy - h / 2.0, cx + w / 2.0, cy + h / 2.0),
                h / 2.0,
                ink,
            );
        };
        match button {
            PanelButton::Minus => bar(self, 76.0, 16.0),
            PanelButton::Plus => {
                bar(self, 76.0, 16.0);
                bar(self, 16.0, 76.0);
            }
            PanelButton::Reset => self.draw_text(
                font,
                "Reset",
                38.0,
                ink,
                (cx, cy),
                HorizontalAlign::Center,
                VerticalAlign::Center,
            ),
        }
    }
}

const ICE_ISH: Rgba = [116, 247, 255, 255];

fn font() -> FontRef<'static> {
    FontRef::try_from_slice(include_bytes!("../resources/Ubuntu-Medium.ttf")).unwrap()
}

/// The part of the panel that never changes.
pub(crate) fn render_static() -> Vec<u8> {
    let font = font();
    let mut canvas = Canvas {
        px: vec![0; HUD_TEXTURE_SIDE * HUD_TEXTURE_SIDE * 4],
    };

    // Card
    canvas.framed_round_rect(rect(24.0, 24.0, 1000.0, 1000.0), 56.0, 4.0, NAVY, ELECTRIC);

    // Header
    canvas.draw_mark(52, 50);
    canvas.draw_text(
        &font,
        "AirFrameXR",
        92.0,
        WHITE,
        (262.0, 128.0),
        HorizontalAlign::Left,
        VerticalAlign::Center,
    );
    canvas.fill_round_rect(rect(60.0, 262.0, 964.0, 266.0), 2.0, LINE);

    // Status label
    canvas.fill_round_rect(rect(70.0, 292.0, 90.0, 312.0), 10.0, CYAN);
    canvas.draw_text(
        &font,
        "STATUS",
        32.0,
        MUTED,
        (106.0, 302.0),
        HorizontalAlign::Left,
        VerticalAlign::Center,
    );

    // Height card
    canvas.framed_round_rect(rect(60.0, 580.0, 964.0, 916.0), 40.0, 3.0, CARD, LINE);
    canvas.draw_text(
        &font,
        "Floor height",
        46.0,
        WHITE,
        (96.0, 624.0),
        HorizontalAlign::Left,
        VerticalAlign::Center,
    );
    canvas.draw_text(
        &font,
        "applies when the game starts",
        30.0,
        MUTED,
        (928.0, 626.0),
        HorizontalAlign::Right,
        VerticalAlign::Center,
    );

    // Footer
    canvas.draw_text(
        &font,
        "Starts by itself when the streamer connects",
        32.0,
        MUTED,
        (512.0, 948.0),
        HorizontalAlign::Center,
        VerticalAlign::Center,
    );

    canvas.px
}

/// The AirFrameXR Quest client version (Cargo.toml has 1.3.0, the panel shows 1.3).
const CLIENT_VERSION: &str = "1.3";

/// The whole panel for `state`, over the static layer.
pub(crate) fn render(static_layer: &[u8], state: &PanelState) -> Vec<u8> {
    let font = font();
    let mut canvas = Canvas {
        px: static_layer.to_vec(),
    };

    // Subtitle: the info line starts with "AirFrameXR client <ALVR protocol version>"; shown as "Quest client <ours> · ALVR <protocol>"
    let version = state
        .info
        .lines()
        .next()
        .and_then(|l| l.strip_prefix("AirFrameXR client "))
        .unwrap_or("");
    canvas.draw_text(
        &font,
        &format!("Quest client {CLIENT_VERSION} · ALVR {version}").replace("  ", " "),
        38.0,
        MUTED,
        (266.0, 206.0),
        HorizontalAlign::Left,
        VerticalAlign::Center,
    );

    // Status
    canvas.draw_text(
        &font,
        &state.status,
        50.0,
        WHITE,
        (512.0, 430.0),
        HorizontalAlign::Center,
        VerticalAlign::Center,
    );

    // Height
    canvas.draw_text(
        &font,
        &if state.offset_cm == 0 {
            "0 cm".to_owned()
        } else {
            format!("{:+} cm", state.offset_cm)
        },
        104.0,
        if state.offset_cm == 0 { WHITE } else { CYAN },
        (512.0, 738.0),
        HorizontalAlign::Center,
        VerticalAlign::Center,
    );
    canvas.draw_text(
        &font,
        &match state.headset_height_cm {
            Some(h) => format!(
                "headset {:.2} m  ·  game {:.2} m",
                h as f32 / 100.0,
                (h + state.offset_cm) as f32 / 100.0
            ),
            None => "headset height: waiting for tracking".to_owned(),
        },
        36.0,
        MUTED,
        (512.0, 812.0),
        HorizontalAlign::Center,
        VerticalAlign::Center,
    );
    for button in [PanelButton::Minus, PanelButton::Plus, PanelButton::Reset] {
        canvas.draw_button(&font, button, state);
    }

    // Small print: hostname and address (the info lines after the version)
    let small_print = state.info.lines().skip(1).collect::<Vec<_>>().join("  ·  ");
    canvas.draw_text(
        &font,
        &small_print,
        28.0,
        MUTED,
        (512.0, 982.0),
        HorizontalAlign::Center,
        VerticalAlign::Center,
    );

    canvas.px
}
