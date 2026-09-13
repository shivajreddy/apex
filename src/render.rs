//! Direct2D + DirectWrite rendering. All layout is in logical DIPs; the
//! render target is DPI-aware so drawing scales per-monitor automatically.
//!
//! The scene is drawn into a 32-bit premultiplied DIB and copied into the
//! window in `WM_PAINT` (see [`Renderer::blit`]). DWM honours the DIB's
//! alpha (see `window::enable_backdrop`), so wherever the frame is
//! transparent the compositor's live blur shows through - when translucent
//! the background is just a tint over it. Software Direct2D into a DIB
//! creates no swap chain, so it is cheaper than an hwnd target.

use std::collections::HashMap;
use std::sync::Arc;

use windows::Win32::Foundation::{D2DERR_RECREATE_TARGET, HWND, RECT};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::*;
use windows::core::*;

use crate::editor::{FormEntry, TextField};
use crate::plugin::{Action, Icon, ResultItem};

// ---- layout (logical DIPs) ----
pub const WINDOW_WIDTH: f32 = 680.0;
pub const INPUT_H: f32 = 58.0;
pub const ROW_H: f32 = 44.0;
pub const LIST_PAD: f32 = 6.0;
const PAD_X: f32 = 20.0;
const SEL_MARGIN: f32 = 8.0;
const ICON_SIZE: f32 = 26.0;
const ICON_GAP: f32 = 12.0;
const BAR_H: f32 = 34.0;
pub const HEADER_H: f32 = 26.0;
// Gap between the title and the dimmed category that follows it.
const CATEGORY_GAP: f32 = 8.0;
// Alias pill drawn immediately after a result's title (and category).
const BADGE_H: f32 = 20.0;
const BADGE_PAD_X: f32 = 8.0;
const BADGE_GAP: f32 = 9.0;
/// Downward nudge for alias-pill text so it sits at its optical centre.
const BADGE_TEXT_DY: f32 = 1.5;
const PANEL_W: f32 = 300.0;
const PANEL_ROW: f32 = 32.0;
const PANEL_PAD: f32 = 6.0;
const PANEL_MARGIN: f32 = 8.0;
/// Width of the text caret.
const CARET_W: f32 = 2.0;
// Scrollbar: a rounded thumb on a faint track at the right edge of the list,
// Raycast-style - shown only when the list overflows.
const SCROLLBAR_W: f32 = 6.0;
const SCROLLBAR_MARGIN: f32 = 4.0;
const SCROLLBAR_PAD: f32 = 4.0;
const SCROLLBAR_MIN_THUMB: f32 = 36.0;
/// The thumb's halo, in the opposite polarity to the thumb, so it stays
/// legible whether the backdrop behind it is light or dark.
const SCROLLBAR_HALO: f32 = 1.0;

// Forms stack a dim label over an editable value, so they need more width
// than the actions panel - links in particular are long.
const FORM_W: f32 = 460.0;
const FORM_ROW: f32 = 50.0;
const FORM_TITLE_H: f32 = 28.0;
const FORM_LABEL_H: f32 = 18.0;

/// Rows the list viewport is tall enough to show at once. The window is a
/// fixed size (see [`content_height`]): the list always occupies this many
/// rows' worth of space, scrolling when there are more and leaving the lower
/// part empty when there are fewer, so the window never jumps around as
/// results come and go.
pub const MAX_VISIBLE_ROWS: usize = 8;

/// The fixed height of the list viewport - always this, regardless of how
/// many rows there are.
pub const LIST_VIEWPORT_H: f32 = LIST_PAD * 2.0 + MAX_VISIBLE_ROWS as f32 * ROW_H;

/// Full height of the list content, headers included. May exceed the
/// viewport, in which case the list scrolls.
pub fn list_content_height(rows: usize, headers: usize) -> f32 {
    LIST_PAD * 2.0 + headers as f32 * HEADER_H + rows as f32 * ROW_H
}

/// Height given to the list on screen: always the fixed viewport.
pub fn list_viewport_height(_rows: usize, _headers: usize) -> f32 {
    LIST_VIEWPORT_H
}

/// Offset of row `index` within the list content.
///
/// A section header occupies `HEADER_H` immediately above the row it starts
/// at, so a row's offset depends on how many headers precede it.
pub fn row_offset(sections: &[(usize, &'static str)], index: usize) -> f32 {
    let headers = sections.iter().filter(|(start, _)| *start <= index).count();
    LIST_PAD + headers as f32 * HEADER_H + index as f32 * ROW_H
}

/// The window's content height. Fixed: input, separator, the full list
/// viewport, and the bottom bar - the same whether the list is full, has one
/// row, or is empty. A fixed window stops the window resizing as you type.
pub fn content_height(_rows: usize, _headers: usize) -> f32 {
    INPUT_H + 1.0 + LIST_VIEWPORT_H + BAR_H
}

fn form_panel_height(fields: usize) -> f32 {
    FORM_TITLE_H + fields as f32 * FORM_ROW + PANEL_PAD * 2.0
}

/// Window height needed to show a form with `fields` rows without clipping.
///
/// The overlay draws inside the existing window, so a form opened over a
/// short result list would otherwise be cut off at the bottom.
pub fn form_window_height(fields: usize) -> f32 {
    INPUT_H + 1.0 + PANEL_MARGIN * 2.0 + form_panel_height(fields) + BAR_H
}

/// Everything the renderer needs for one frame.
pub struct Frame<'a> {
    pub query: &'a TextField,
    pub caret_visible: bool,
    pub results: &'a [ResultItem],
    pub selected: usize,
    /// `(first row index, heading)`, ascending. Empty for a typed query.
    pub sections: &'a [(usize, &'static str)],
    /// How far the list is scrolled, in DIPs.
    pub scroll: f32,
    pub panel: Option<PanelView<'a>>,
}

/// Overlay state (actions panel or text input), anchored bottom-right.
pub enum PanelView<'a> {
    Actions {
        actions: &'a [Action],
        selected: usize,
    },
    TextInput {
        prompt: &'a str,
        buffer: &'a TextField,
    },
    Form {
        title: &'a str,
        fields: &'a [FormEntry],
        focused: usize,
    },
}

/// The frame's size on screen, in physical pixels.
pub struct Placement {
    pub width: u32,
    pub height: u32,
    pub dpi: f32,
    /// Opacity of everything drawn, tint included, `0..=1`; the summon
    /// animation ramps it, per pixel.
    pub opacity: f32,
    /// Uniform scale of the scene, `0 < scale <= 1`, drawn into the DIB's
    /// top-left corner. Always 1 today: the window is painted in place.
    pub scale: f32,
}

// ---- theme ----
const fn rgba(rgb: u32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((rgb >> 16) & 0xFF) as f32 / 255.0,
        g: ((rgb >> 8) & 0xFF) as f32 / 255.0,
        b: (rgb & 0xFF) as f32 / 255.0,
        a,
    }
}

/// The colours of one appearance. Two exist, following Windows' light/dark
/// setting for apps; `[appearance] theme` pins either.
pub struct Palette {
    /// Window background colour. Painted opaque when there is no live blur,
    /// and as a tint at `[appearance] opacity` over the blur otherwise, so
    /// only the RGB matters here - the alpha is set at brush creation.
    background: D2D1_COLOR_F,
    text: D2D1_COLOR_F,
    dim: D2D1_COLOR_F,
    /// Hairline separators.
    faint: D2D1_COLOR_F,
    /// The highlighted row.
    select: D2D1_COLOR_F,
    panel: D2D1_COLOR_F,
    badge_bg: D2D1_COLOR_F,
    badge_fg: D2D1_COLOR_F,
    /// Selected text.
    selection: D2D1_COLOR_F,
    /// Hairline rim just inside the window edge - the "glass edge" that
    /// reads as frosted even over a dark backdrop, where the blur alone is
    /// invisible.
    border: D2D1_COLOR_F,
    /// The scrollbar thumb, its faint track, and the thumb's halo (opposite
    /// polarity, so the thumb reads over a light or a dark backdrop).
    scrollbar: D2D1_COLOR_F,
    scrollbar_track: D2D1_COLOR_F,
    scrollbar_halo: D2D1_COLOR_F,
}

// Over a dark desktop a dark tint on a dark blur is just dark, so the rim
// below is what keeps the window reading as glass rather than a solid slab.
const DARK: Palette = Palette {
    background: rgba(0x121216, 0.50),
    text: rgba(0xF4F4F6, 1.0),
    dim: rgba(0xF4F4F6, 0.42),
    faint: rgba(0xFFFFFF, 0.08),
    select: rgba(0xFFFFFF, 0.10),
    panel: rgba(0x26262B, 0.94),
    badge_bg: rgba(0xFFFFFF, 0.13),
    badge_fg: rgba(0xF4F4F6, 0.65),
    selection: rgba(0x4C8DFF, 0.45),
    border: rgba(0xFFFFFF, 0.12),
    scrollbar: rgba(0xFFFFFF, 0.50),
    scrollbar_track: rgba(0xFFFFFF, 0.06),
    scrollbar_halo: rgba(0x000000, 0.22),
};

const LIGHT: Palette = Palette {
    background: rgba(0xF6F6F8, 0.55),
    text: rgba(0x1B1B1F, 1.0),
    dim: rgba(0x1B1B1F, 0.50),
    faint: rgba(0x000000, 0.09),
    select: rgba(0x000000, 0.07),
    panel: rgba(0xF0F0F3, 0.96),
    badge_bg: rgba(0x000000, 0.09),
    badge_fg: rgba(0x1B1B1F, 0.70),
    selection: rgba(0x3B82F6, 0.35),
    border: rgba(0x000000, 0.14),
    scrollbar: rgba(0x000000, 0.42),
    scrollbar_track: rgba(0x000000, 0.06),
    scrollbar_halo: rgba(0xFFFFFF, 0.30),
};

pub struct Renderer {
    dwrite: IDWriteFactory,
    d2d: ID2D1Factory,
    fmt_input: IDWriteTextFormat,
    fmt_title: IDWriteTextFormat,
    fmt_subtitle: IDWriteTextFormat,
    fmt_subtitle_left: IDWriteTextFormat,
    fmt_panel: IDWriteTextFormat,
    fmt_badge: IDWriteTextFormat,
    fmt_header: IDWriteTextFormat,
    palette: &'static Palette,
    /// The palette's background: the solid fill when there is no blur, and
    /// the RGB of the tint over it when there is.
    background: D2D1_COLOR_F,
    /// Whether the compositor is providing a live blur behind the window.
    /// When true the background is a tint over it; when false, solid.
    translucent: bool,
    /// Alpha of that tint, `[appearance] opacity`.
    tint: f32,
    target: Option<Target>,
}

/// Device-dependent resources, recreated if the target is lost or the window
/// changes size.
struct Target {
    rt: ID2D1DCRenderTarget,
    dib: Dib,
    dpi: f32,
    brushes: Brushes,
    /// D2D copies of plugin icons, keyed by the Arc's data address.
    /// Dropped with the target (i.e. every hide), so it stays small.
    icons: HashMap<usize, ID2D1Bitmap>,
}

struct Brushes {
    text: ID2D1SolidColorBrush,
    dim: ID2D1SolidColorBrush,
    faint: ID2D1SolidColorBrush,
    select: ID2D1SolidColorBrush,
    panel: ID2D1SolidColorBrush,
    badge_bg: ID2D1SolidColorBrush,
    badge_fg: ID2D1SolidColorBrush,
    selection: ID2D1SolidColorBrush,
    border: ID2D1SolidColorBrush,
    scrollbar: ID2D1SolidColorBrush,
    scrollbar_track: ID2D1SolidColorBrush,
    scrollbar_halo: ID2D1SolidColorBrush,
    /// The tint over the live blur when translucent.
    bg_tint: ID2D1SolidColorBrush,
    /// The opaque window fill, used only when the blur is off.
    bg_solid: ID2D1SolidColorBrush,
}

/// A 32bpp top-down DIB selected into a memory DC: the pixels the window is
/// made of. Physical pixels.
struct Dib {
    hdc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    width: u32,
    height: u32,
}

impl Dib {
    unsafe fn new(width: u32, height: u32) -> Result<Self> {
        unsafe {
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width as i32,
                    biHeight: -(height as i32), // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let bitmap = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
            let hdc = CreateCompatibleDC(None);
            if hdc.is_invalid() {
                let _ = DeleteObject(bitmap.into());
                return Err(Error::from_thread());
            }
            let previous = SelectObject(hdc, bitmap.into());
            Ok(Self {
                hdc,
                bitmap,
                previous,
                width,
                height,
            })
        }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.hdc, self.previous);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.hdc);
        }
    }
}

impl Renderer {
    /// `translucent` means the compositor is blurring behind the window, so
    /// the background is painted as a tint at `tint` (`[appearance]
    /// opacity`, default 0.5) over it; otherwise it is painted solid. `dark`
    /// picks the palette.
    pub fn new(translucent: bool, dark: bool, tint: Option<f32>) -> Result<Self> {
        unsafe {
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;

            let make = |size: f32, weight: DWRITE_FONT_WEIGHT| -> Result<IDWriteTextFormat> {
                dwrite.CreateTextFormat(
                    w!("Segoe UI"),
                    None,
                    weight,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    w!("en-us"),
                )
            };

            // Editable fields never wrap: text longer than the field scrolls
            // horizontally to keep the caret in view instead.
            let fmt_input = make(20.0, DWRITE_FONT_WEIGHT_NORMAL)?;
            fmt_input.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            fmt_input.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            let fmt_title = make(15.0, DWRITE_FONT_WEIGHT_NORMAL)?;
            fmt_title.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            let fmt_subtitle = make(12.0, DWRITE_FONT_WEIGHT_NORMAL)?;
            fmt_subtitle.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            fmt_subtitle.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_TRAILING)?;
            let fmt_subtitle_left = make(12.0, DWRITE_FONT_WEIGHT_NORMAL)?;
            fmt_subtitle_left.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            let fmt_panel = make(14.0, DWRITE_FONT_WEIGHT_NORMAL)?;
            fmt_panel.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            fmt_panel.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            // Centred both ways so the text sits in the middle of the pill.
            let fmt_badge = make(11.5, DWRITE_FONT_WEIGHT_NORMAL)?;
            fmt_badge.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            fmt_badge.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            let fmt_header = make(11.5, DWRITE_FONT_WEIGHT_SEMI_BOLD)?;
            fmt_header.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

            let palette = if dark { &DARK } else { &LIGHT };
            Ok(Self {
                d2d,
                dwrite,
                fmt_input,
                fmt_title,
                fmt_subtitle,
                fmt_subtitle_left,
                fmt_panel,
                fmt_badge,
                fmt_header,
                palette,
                background: palette.background,
                translucent,
                tint: tint.unwrap_or(0.5).clamp(0.0, 1.0),
                target: None,
            })
        }
    }

    /// Make sure a target of the right size exists.
    ///
    /// The DIB is the window's size in physical pixels, so any change of
    /// height (results coming and going) or DPI rebuilds it. That is cheap -
    /// one allocation and a handful of brushes - and far simpler than
    /// keeping an oversized bitmap around.
    unsafe fn ensure_target(&mut self, width: u32, height: u32, dpi: f32) -> Result<()> {
        if let Some(t) = &self.target
            && t.dib.width == width
            && t.dib.height == height
            && t.dpi == dpi
        {
            return Ok(());
        }
        self.target = None;
        unsafe {
            let dib = Dib::new(width.max(1), height.max(1))?;
            // Software rasterizer: our scene is trivial (one small window,
            // redraws only on input) and skipping D3D/DXGI device creation
            // saves ~50MB of process memory. CPU cost per frame is <1ms.
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 0.0,
                dpiY: 0.0,
                usage: D2D1_RENDER_TARGET_USAGE_GDI_COMPATIBLE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let rt = self.d2d.CreateDCRenderTarget(&props)?;
            let bounds = RECT {
                left: 0,
                top: 0,
                right: dib.width as i32,
                bottom: dib.height as i32,
            };
            rt.BindDC(dib.hdc, &bounds)?;
            rt.SetDpi(dpi, dpi);

            let brush = |c: &D2D1_COLOR_F| -> Result<ID2D1SolidColorBrush> {
                rt.CreateSolidColorBrush(c, None)
            };
            let p = self.palette;
            let bg = self.background;
            let tint = self.tint;
            self.target = Some(Target {
                brushes: Brushes {
                    text: brush(&p.text)?,
                    dim: brush(&p.dim)?,
                    faint: brush(&p.faint)?,
                    select: brush(&p.select)?,
                    panel: brush(&p.panel)?,
                    badge_bg: brush(&p.badge_bg)?,
                    badge_fg: brush(&p.badge_fg)?,
                    selection: brush(&p.selection)?,
                    border: brush(&p.border)?,
                    scrollbar: brush(&p.scrollbar)?,
                    scrollbar_track: brush(&p.scrollbar_track)?,
                    scrollbar_halo: brush(&p.scrollbar_halo)?,
                    bg_tint: brush(&D2D1_COLOR_F { a: tint, ..bg })?,
                    bg_solid: brush(&D2D1_COLOR_F { a: 1.0, ..bg })?,
                },
                icons: HashMap::new(),
                dib,
                dpi,
                rt,
            });
            Ok(())
        }
    }

    /// Byte offset in the query nearest to a click at `x` DIPs from the
    /// window's left edge, for placing the caret with the mouse.
    pub fn hit_test_input(&self, field: &TextField, x: f32) -> Option<usize> {
        let text = field.text();
        let caret_x = measure(&self.dwrite, &self.fmt_input, &text[..field.caret()]);
        let shift = field_shift(caret_x, WINDOW_WIDTH - PAD_X * 2.0);
        let local = x - PAD_X + shift;
        unsafe {
            let units: Vec<u16> = text.encode_utf16().collect();
            let layout = self
                .dwrite
                .CreateTextLayout(&units, &self.fmt_input, 8192.0, INPUT_H)
                .ok()?;
            let mut trailing = BOOL(0);
            let mut inside = BOOL(0);
            let mut metrics = DWRITE_HIT_TEST_METRICS::default();
            layout
                .HitTestPoint(local, INPUT_H / 2.0, &mut trailing, &mut inside, &mut metrics)
                .ok()?;
            let mut units_left = metrics.textPosition as usize
                + if trailing.as_bool() {
                    metrics.length as usize
                } else {
                    0
                };
            // UTF-16 index back to a byte offset.
            let mut bytes = 0;
            for c in text.chars() {
                let len16 = c.len_utf16();
                if len16 > units_left {
                    break;
                }
                units_left -= len16;
                bytes += c.len_utf8();
            }
            Some(bytes)
        }
    }

    /// Draw `frame` and put it on screen at `place`.
    pub fn present(&mut self, hwnd: HWND, frame: &Frame, place: &Placement) {
        unsafe {
            if let Err(e) = self.ensure_target(place.width, place.height, place.dpi) {
                crate::dlog!("present: target creation failed: {e}");
                return;
            }
            let end = self.draw(frame, place);
            if let Err(e) = &end {
                crate::dlog!("present: EndDraw failed: {e}");
            }
            if matches!(&end, Err(e) if e.code() == D2DERR_RECREATE_TARGET) {
                self.target = None;
                return;
            }
            if self.target.is_none() {
                return;
            }
            // GDI-compatible target: make sure the DIB bits are final before
            // the window reads them.
            let _ = GdiFlush();
            // Mark the window dirty; the caller pumps the paint (see
            // `window::repaint`) once it no longer holds the App, since
            // WM_PAINT fetches it again.
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    /// Copy the last drawn frame into `hdc`, the window's paint DC. The DIB
    /// is premultiplied BGRA and `BitBlt` copies its alpha verbatim, which DWM
    /// honours for this window (see `window::enable_backdrop`).
    pub fn blit(&self, hdc: HDC) {
        if let Some(t) = &self.target {
            unsafe {
                let _ = BitBlt(
                    hdc,
                    0,
                    0,
                    t.dib.width as i32,
                    t.dib.height as i32,
                    Some(t.dib.hdc),
                    0,
                    0,
                    SRCCOPY,
                );
            }
        }
    }

    unsafe fn draw(&mut self, frame: &Frame, place: &Placement) -> Result<()> {
        let Frame {
            query,
            caret_visible,
            results,
            selected,
            sections,
            scroll,
            panel,
        } = frame;
        let (query, caret_visible, selected, scroll) = (*query, *caret_visible, *selected, *scroll);
        let translucent = self.translucent;
        let Self {
            dwrite,
            fmt_input,
            fmt_title,
            fmt_subtitle,
            fmt_subtitle_left,
            fmt_panel,
            fmt_badge,
            fmt_header,
            target,
            ..
        } = self;
        let t = target.as_mut().unwrap();
        let rt = &t.rt;
        let b = &t.brushes;
        unsafe {
            // `scale` is 1 today (the summon animation only fades); a lower
            // value would draw the scene at a proportionally lower DPI,
            // shrinking it uniformly into the DIB's top-left corner.
            let dpi = t.dpi * place.scale;
            rt.SetDpi(dpi, dpi);
            // And fades it by drawing everything, tint included, with less
            // opacity. Brushes keep their opacity, so it is reset each frame.
            let opacity = place.opacity.clamp(0.0, 1.0);
            for brush in [
                &b.text,
                &b.dim,
                &b.faint,
                &b.select,
                &b.panel,
                &b.badge_bg,
                &b.badge_fg,
                &b.selection,
                &b.border,
                &b.scrollbar,
                &b.scrollbar_track,
                &b.scrollbar_halo,
                &b.bg_tint,
                // Not bg_solid: without a backdrop the window's alpha is
                // ignored, so a faded fill would show as a darkened slab
                // rather than a fade. The foreground fades in over it.
            ] {
                brush.SetOpacity(opacity);
            }

            let width = t.dib.width as f32 * 96.0 / t.dpi;
            let height = t.dib.height as f32 * 96.0 / t.dpi;
            let full = D2D_RECT_F {
                left: 0.0,
                top: 0.0,
                right: width,
                bottom: height,
            };

            rt.BeginDraw();
            rt.Clear(None);
            if translucent {
                // The compositor's live blur shows through wherever the frame
                // is transparent; this is the tint over it.
                rt.FillRectangle(&full, &b.bg_tint);
            } else {
                rt.FillRectangle(&full, &b.bg_solid);
            }

            // Search input (query text or placeholder).
            let input_rect = D2D_RECT_F {
                left: PAD_X,
                top: 0.0,
                right: width - PAD_X,
                bottom: INPUT_H,
            };
            if query.is_empty() {
                draw_text(rt, "Search apps and commands…", fmt_input, &input_rect, &b.dim);
            }
            draw_field(
                rt,
                dwrite,
                b,
                query,
                fmt_input,
                &input_rect,
                panel.is_none().then_some(Focus {
                    caret_visible,
                    caret_half: 12.0,
                }),
            );

            // The list region and bottom bar are always drawn - the window is
            // a fixed size, so an empty result set leaves the list area blank
            // (with a hint) rather than collapsing the window to the input.
            {
                // Separator under the input.
                rt.FillRectangle(
                    &D2D_RECT_F {
                        left: 0.0,
                        top: INPUT_H,
                        right: width,
                        bottom: INPUT_H + 1.0,
                    },
                    &b.faint,
                );

                let list_top = INPUT_H + 1.0;
                let view_h = list_viewport_height(results.len(), sections.len());
                // Clip so scrolled rows cannot bleed over the input or bar.
                rt.PushAxisAlignedClip(
                    &D2D_RECT_F {
                        left: 0.0,
                        top: list_top,
                        right: width,
                        bottom: list_top + view_h,
                    },
                    D2D1_ANTIALIAS_MODE_ALIASED,
                );

                for (start, title) in sections.iter() {
                    // A header sits directly above the row it introduces.
                    let hy = list_top + row_offset(sections, *start) - HEADER_H - scroll;
                    if hy + HEADER_H < list_top || hy > list_top + view_h {
                        continue;
                    }
                    draw_text(
                        rt,
                        title,
                        fmt_header,
                        &D2D_RECT_F {
                            left: PAD_X,
                            top: hy,
                            right: width - PAD_X,
                            bottom: hy + HEADER_H,
                        },
                        &b.dim,
                    );
                }

                for (i, item) in results.iter().enumerate() {
                    let y = list_top + row_offset(sections, i) - scroll;
                    // Cull rows outside the viewport: the default list can
                    // hold every installed app.
                    if y + ROW_H < list_top || y > list_top + view_h {
                        continue;
                    }
                    if i == selected {
                        rt.FillRoundedRectangle(
                            &D2D1_ROUNDED_RECT {
                                rect: D2D_RECT_F {
                                    left: SEL_MARGIN,
                                    top: y,
                                    right: width - SEL_MARGIN,
                                    bottom: y + ROW_H,
                                },
                                radiusX: 8.0,
                                radiusY: 8.0,
                            },
                            &b.select,
                        );
                    }
                    if let Some(icon) = &item.icon
                        && let Some(bmp) = icon_bitmap(rt, &mut t.icons, icon)
                    {
                        let top = y + (ROW_H - ICON_SIZE) / 2.0;
                        rt.DrawBitmap(
                            &bmp,
                            Some(&D2D_RECT_F {
                                left: PAD_X,
                                top,
                                right: PAD_X + ICON_SIZE,
                                bottom: top + ICON_SIZE,
                            }),
                            opacity,
                            D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                            None,
                        );
                    }
                    let title_rect = D2D_RECT_F {
                        left: PAD_X + ICON_SIZE + ICON_GAP,
                        top: y,
                        right: width - PAD_X,
                        bottom: y + ROW_H,
                    };
                    draw_text(rt, &item.title, fmt_title, &title_rect, &b.text);

                    // Walk a cursor along the row: title, then the dimmed
                    // category, then the alias pill - the Raycast order.
                    let mut cursor =
                        title_rect.left + measure(dwrite, fmt_title, &item.title);

                    // Category, in the same face as the title but dimmed.
                    if !item.category.is_empty() {
                        cursor += CATEGORY_GAP;
                        let cat_rect = D2D_RECT_F {
                            left: cursor,
                            top: y,
                            right: width - PAD_X,
                            bottom: y + ROW_H,
                        };
                        draw_text(rt, &item.category, fmt_title, &cat_rect, &b.dim);
                        cursor += measure(dwrite, fmt_title, &item.category);
                    }

                    // Alias pill, after the title (and category).
                    if let Some(badge) = &item.badge {
                        let badge_w = measure(dwrite, fmt_badge, badge);
                        let left = cursor + BADGE_GAP;
                        let pill = D2D_RECT_F {
                            left,
                            top: y + (ROW_H - BADGE_H) / 2.0,
                            right: left + badge_w + BADGE_PAD_X * 2.0,
                            bottom: y + (ROW_H + BADGE_H) / 2.0,
                        };
                        rt.FillRoundedRectangle(
                            &D2D1_ROUNDED_RECT {
                                rect: pill,
                                radiusX: 6.0,
                                radiusY: 6.0,
                            },
                            &b.badge_bg,
                        );
                        // Nudge the glyphs down to their optical centre:
                        // DirectWrite centres the line box, but alias text is
                        // all no-descender letters (c, vs, tm...), so the ink
                        // otherwise floats high in the pill.
                        let text_rect = D2D_RECT_F {
                            top: pill.top + BADGE_TEXT_DY,
                            bottom: pill.bottom + BADGE_TEXT_DY,
                            ..pill
                        };
                        draw_text(rt, badge, fmt_badge, &text_rect, &b.badge_fg);
                    }

                    if !item.subtitle.is_empty() {
                        let row = D2D_RECT_F {
                            left: PAD_X,
                            top: y,
                            right: width - PAD_X,
                            bottom: y + ROW_H,
                        };
                        draw_text(rt, &item.subtitle, fmt_subtitle, &row, &b.dim);
                    }
                }
                rt.PopAxisAlignedClip();

                // Scrollbar, when the list is taller than the viewport. A
                // thin rounded thumb on the right edge, sized and positioned
                // by how far through the content the scroll is - Raycast-style.
                let content_h = list_content_height(results.len(), sections.len());
                if content_h > view_h {
                    let track_top = list_top + SCROLLBAR_PAD;
                    let track_h = view_h - SCROLLBAR_PAD * 2.0;
                    let thumb_h = (view_h / content_h * track_h).max(SCROLLBAR_MIN_THUMB);
                    let travel = (track_h - thumb_h).max(0.0);
                    let progress = (scroll / (content_h - view_h)).clamp(0.0, 1.0);
                    let thumb_top = track_top + travel * progress;
                    let x1 = width - SCROLLBAR_MARGIN;
                    let x0 = x1 - SCROLLBAR_W;
                    let r = SCROLLBAR_W / 2.0;
                    let pill = |rect: D2D_RECT_F, radius: f32| D2D1_ROUNDED_RECT {
                        rect,
                        radiusX: radius,
                        radiusY: radius,
                    };
                    // Faint track the whole viewport tall: the gutter reads
                    // as one even when the thumb is small.
                    rt.FillRoundedRectangle(
                        &pill(
                            D2D_RECT_F {
                                left: x0,
                                top: track_top,
                                right: x1,
                                bottom: track_top + track_h,
                            },
                            r,
                        ),
                        &b.scrollbar_track,
                    );
                    let thumb = D2D_RECT_F {
                        left: x0,
                        top: thumb_top,
                        right: x1,
                        bottom: thumb_top + thumb_h,
                    };
                    rt.FillRoundedRectangle(
                        &pill(
                            D2D_RECT_F {
                                left: thumb.left - SCROLLBAR_HALO,
                                top: thumb.top - SCROLLBAR_HALO,
                                right: thumb.right + SCROLLBAR_HALO,
                                bottom: thumb.bottom + SCROLLBAR_HALO,
                            },
                            r + SCROLLBAR_HALO,
                        ),
                        &b.scrollbar_halo,
                    );
                    rt.FillRoundedRectangle(&pill(thumb, r), &b.scrollbar);
                }

                // Bottom bar with key hints.
                let hint = match panel {
                    Some(PanelView::Form { .. }) => "Save \u{21b5}      Field Tab",
                    Some(PanelView::TextInput { .. }) => "Confirm \u{21b5}",
                    Some(PanelView::Actions { .. }) => "Run \u{21b5}",
                    None => "Open \u{21b5}      Actions Ctrl+K",
                };
                let bar_top = height - BAR_H;
                rt.FillRectangle(
                    &D2D_RECT_F {
                        left: 0.0,
                        top: bar_top,
                        right: width,
                        bottom: bar_top + 1.0,
                    },
                    &b.faint,
                );
                let bar_rect = D2D_RECT_F {
                    left: PAD_X,
                    top: bar_top,
                    right: width - PAD_X,
                    bottom: height,
                };
                draw_text(rt, "Apex", fmt_subtitle_left, &bar_rect, &b.dim);
                draw_text(rt, hint, fmt_subtitle, &bar_rect, &b.dim);
            }

            // Overlay panel (actions / text input / form), bottom-right.
            // Outside the results block: a form can be taller than a short
            // result list, and the window is sized to fit it.
            if let Some(view) = panel {
                let (panel_w, panel_h) = match view {
                    PanelView::Actions { actions, .. } => (
                        PANEL_W,
                        actions.len().max(1) as f32 * PANEL_ROW + PANEL_PAD * 2.0,
                    ),
                    PanelView::TextInput { .. } => (PANEL_W, PANEL_ROW + PANEL_PAD * 2.0),
                    PanelView::Form { fields, .. } => (FORM_W, form_panel_height(fields.len())),
                };
                let rect = D2D_RECT_F {
                    left: width - PANEL_MARGIN - panel_w,
                    top: height - BAR_H - PANEL_MARGIN - panel_h,
                    right: width - PANEL_MARGIN,
                    bottom: height - BAR_H - PANEL_MARGIN,
                };
                rt.FillRoundedRectangle(
                    &D2D1_ROUNDED_RECT {
                        rect,
                        radiusX: 10.0,
                        radiusY: 10.0,
                    },
                    &b.panel,
                );

                match view {
                    PanelView::Actions { actions, selected } => {
                        let mut ay = rect.top + PANEL_PAD;
                        for (i, action) in actions.iter().enumerate() {
                            let row = D2D_RECT_F {
                                left: rect.left + PANEL_PAD,
                                top: ay,
                                right: rect.right - PANEL_PAD,
                                bottom: ay + PANEL_ROW,
                            };
                            if i == *selected {
                                rt.FillRoundedRectangle(
                                    &D2D1_ROUNDED_RECT {
                                        rect: row,
                                        radiusX: 6.0,
                                        radiusY: 6.0,
                                    },
                                    &b.select,
                                );
                            }
                            let label_rect = D2D_RECT_F {
                                left: row.left + 10.0,
                                right: row.right - 10.0,
                                ..row
                            };
                            draw_text(rt, &action.label, fmt_panel, &label_rect, &b.text);
                            ay += PANEL_ROW;
                        }
                    }
                    PanelView::TextInput { prompt, buffer } => {
                        let row = D2D_RECT_F {
                            left: rect.left + PANEL_PAD + 10.0,
                            top: rect.top + PANEL_PAD,
                            right: rect.right - PANEL_PAD - 10.0,
                            bottom: rect.bottom - PANEL_PAD,
                        };
                        let prompt_text = format!("{prompt}: ");
                        draw_text(rt, &prompt_text, fmt_panel, &row, &b.dim);
                        let prompt_w = measure(dwrite, fmt_panel, &prompt_text);
                        let field_rect = D2D_RECT_F {
                            left: row.left + prompt_w + 8.0,
                            ..row
                        };
                        draw_field(
                            rt,
                            dwrite,
                            b,
                            buffer,
                            fmt_panel,
                            &field_rect,
                            Some(Focus {
                                caret_visible,
                                caret_half: 9.0,
                            }),
                        );
                    }
                    PanelView::Form {
                        title,
                        fields,
                        focused,
                    } => {
                        let title_rect = D2D_RECT_F {
                            left: rect.left + PANEL_PAD + 10.0,
                            top: rect.top + PANEL_PAD,
                            right: rect.right - PANEL_PAD - 10.0,
                            bottom: rect.top + PANEL_PAD + FORM_TITLE_H,
                        };
                        draw_text(rt, title, fmt_panel, &title_rect, &b.dim);

                        let mut fy = rect.top + PANEL_PAD + FORM_TITLE_H;
                        for (i, field) in fields.iter().enumerate() {
                            let row = D2D_RECT_F {
                                left: rect.left + PANEL_PAD,
                                top: fy,
                                right: rect.right - PANEL_PAD,
                                bottom: fy + FORM_ROW,
                            };
                            if i == *focused {
                                rt.FillRoundedRectangle(
                                    &D2D1_ROUNDED_RECT {
                                        rect: row,
                                        radiusX: 6.0,
                                        radiusY: 6.0,
                                    },
                                    &b.select,
                                );
                            }
                            let label_rect = D2D_RECT_F {
                                left: row.left + 10.0,
                                top: row.top + 3.0,
                                right: row.right - 10.0,
                                bottom: row.top + 3.0 + FORM_LABEL_H,
                            };
                            draw_text(rt, &field.label, fmt_subtitle_left, &label_rect, &b.dim);

                            let value_rect = D2D_RECT_F {
                                left: row.left + 10.0,
                                top: row.top + FORM_LABEL_H,
                                right: row.right - 10.0,
                                bottom: row.bottom,
                            };
                            draw_field(
                                rt,
                                dwrite,
                                b,
                                &field.value,
                                fmt_panel,
                                &value_rect,
                                (i == *focused).then_some(Focus {
                                    caret_visible,
                                    caret_half: 8.0,
                                }),
                            );
                            fy += FORM_ROW;
                        }
                    }
                }
            }

            // Glass-edge rim, drawn last so it sits crisply at the very edge
            // over everything. Inset half the stroke so the 1px line stays
            // inside the window and traces the DWM rounded corners (~8 DIP).
            rt.DrawRoundedRectangle(
                &D2D1_ROUNDED_RECT {
                    rect: D2D_RECT_F {
                        left: 0.5,
                        top: 0.5,
                        right: width - 0.5,
                        bottom: height - 0.5,
                    },
                    radiusX: 8.0,
                    radiusY: 8.0,
                },
                &b.border,
                1.0,
                None,
            );

            rt.EndDraw(None, None)
        }
    }
}

/// How far a field's content is shifted left so the caret stays in view.
fn field_shift(caret_x: f32, avail: f32) -> f32 {
    (caret_x + CARET_W - avail).max(0.0)
}

/// How a focused field shows its caret. `None` means the field is not the
/// one being edited: no caret, no selection, no scrolling.
#[derive(Clone, Copy)]
struct Focus {
    /// Off during the blink's dark phase.
    caret_visible: bool,
    /// Caret extent above and below the field's middle, in DIPs.
    caret_half: f32,
}

/// Paint one editable field inside `rect`: selection highlight, text, and -
/// when focused - the caret.
///
/// Text wider than the field scrolls left so the caret stays visible; the
/// field is clipped to its rect so the overflow never shows.
unsafe fn draw_field(
    rt: &ID2D1RenderTarget,
    dwrite: &IDWriteFactory,
    brushes: &Brushes,
    field: &TextField,
    format: &IDWriteTextFormat,
    rect: &D2D_RECT_F,
    focus: Option<Focus>,
) {
    let text = field.text();
    let caret_x = measure(dwrite, format, &text[..field.caret()]);
    let shift = if focus.is_some() {
        field_shift(caret_x, rect.right - rect.left)
    } else {
        0.0
    };
    let left = rect.left - shift;
    let mid = (rect.top + rect.bottom) / 2.0;
    unsafe {
        rt.PushAxisAlignedClip(rect, D2D1_ANTIALIAS_MODE_ALIASED);
        if let Some(focus) = focus
            && let Some((a, b)) = field.selection()
        {
            let x0 = left + measure(dwrite, format, &text[..a]);
            let x1 = left + measure(dwrite, format, &text[..b]);
            rt.FillRoundedRectangle(
                &D2D1_ROUNDED_RECT {
                    rect: D2D_RECT_F {
                        left: x0,
                        top: mid - focus.caret_half - 1.0,
                        right: x1,
                        bottom: mid + focus.caret_half + 1.0,
                    },
                    radiusX: 3.0,
                    radiusY: 3.0,
                },
                &brushes.selection,
            );
        }
        draw_text(
            rt,
            text,
            format,
            &D2D_RECT_F {
                left,
                top: rect.top,
                // No wrapping, so the right edge only needs to be far away.
                right: left + 1.0e5,
                bottom: rect.bottom,
            },
            &brushes.text,
        );
        if let Some(focus) = focus
            && focus.caret_visible
        {
            let cx = left + caret_x + 1.0;
            rt.FillRectangle(
                &D2D_RECT_F {
                    left: cx,
                    top: mid - focus.caret_half,
                    right: cx + CARET_W,
                    bottom: mid + focus.caret_half,
                },
                &brushes.text,
            );
        }
        rt.PopAxisAlignedClip();
    }
}

/// Width of `text` in DIPs when rendered with `format`.
fn measure(dwrite: &IDWriteFactory, format: &IDWriteTextFormat, text: &str) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    unsafe {
        let units: Vec<u16> = text.encode_utf16().collect();
        let Ok(layout) = dwrite.CreateTextLayout(&units, format, 8192.0, 512.0) else {
            return 0.0;
        };
        let mut m = DWRITE_TEXT_METRICS::default();
        if layout.GetMetrics(&mut m).is_err() {
            return 0.0;
        }
        m.widthIncludingTrailingWhitespace
    }
}

/// Get (or create) the per-target D2D bitmap for an icon.
unsafe fn icon_bitmap(
    rt: &ID2D1RenderTarget,
    cache: &mut HashMap<usize, ID2D1Bitmap>,
    icon: &Arc<Icon>,
) -> Option<ID2D1Bitmap> {
    let key = Arc::as_ptr(icon) as usize;
    if let Some(b) = cache.get(&key) {
        return Some(b.clone());
    }
    unsafe {
        let props = D2D1_BITMAP_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
        };
        let bmp = rt
            .CreateBitmap(
                D2D_SIZE_U {
                    width: icon.width,
                    height: icon.height,
                },
                Some(icon.bgra.as_ptr() as *const core::ffi::c_void),
                icon.width * 4,
                &props,
            )
            .ok()?;
        cache.insert(key, bmp.clone());
        Some(bmp)
    }
}

unsafe fn draw_text(
    rt: &ID2D1RenderTarget,
    text: &str,
    format: &IDWriteTextFormat,
    rect: &D2D_RECT_F,
    brush: &ID2D1SolidColorBrush,
) {
    let units: Vec<u16> = text.encode_utf16().collect();
    unsafe {
        rt.DrawText(
            &units,
            format,
            rect,
            brush,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
            DWRITE_MEASURING_MODE_NATURAL,
        );
    }
}
