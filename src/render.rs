//! Direct2D + DirectWrite rendering. All layout is in logical DIPs; the
//! render target is DPI-aware so drawing scales per-monitor automatically.

use std::collections::HashMap;
use std::sync::Arc;

use windows::Win32::Foundation::{D2DERR_RECREATE_TARGET, HWND, RECT};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;
use windows::core::*;

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
const PANEL_W: f32 = 300.0;
const PANEL_ROW: f32 = 32.0;
const PANEL_PAD: f32 = 6.0;
const PANEL_MARGIN: f32 = 8.0;

/// Total window height for `n` result rows.
pub fn content_height(n: usize) -> f32 {
    if n == 0 {
        INPUT_H
    } else {
        INPUT_H + 1.0 + LIST_PAD * 2.0 + ROW_H * n as f32 + BAR_H
    }
}

/// Everything the renderer needs for one frame.
pub struct Frame<'a> {
    pub query: &'a str,
    pub caret_visible: bool,
    pub results: &'a [ResultItem],
    pub selected: usize,
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
        buffer: &'a str,
    },
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

const COL_BG: D2D1_COLOR_F = rgba(0x1E1E20, 1.0);
const COL_TEXT: D2D1_COLOR_F = rgba(0xF2F2F2, 1.0);
const COL_DIM: D2D1_COLOR_F = rgba(0xF2F2F2, 0.35);
const COL_FAINT: D2D1_COLOR_F = rgba(0xFFFFFF, 0.07);
const COL_SELECT: D2D1_COLOR_F = rgba(0xFFFFFF, 0.08);
const COL_PANEL: D2D1_COLOR_F = rgba(0x2A2A2E, 1.0);

pub struct Renderer {
    dwrite: IDWriteFactory,
    d2d: ID2D1Factory,
    fmt_input: IDWriteTextFormat,
    fmt_title: IDWriteTextFormat,
    fmt_subtitle: IDWriteTextFormat,
    fmt_subtitle_left: IDWriteTextFormat,
    fmt_panel: IDWriteTextFormat,
    target: Option<Target>,
}

/// Device-dependent resources, recreated if the target is lost.
struct Target {
    rt: ID2D1HwndRenderTarget,
    text: ID2D1SolidColorBrush,
    dim: ID2D1SolidColorBrush,
    faint: ID2D1SolidColorBrush,
    select: ID2D1SolidColorBrush,
    panel: ID2D1SolidColorBrush,
    /// D2D copies of plugin icons, keyed by the Arc's data address.
    /// Dropped with the target (i.e. every hide), so it stays small.
    icons: HashMap<usize, ID2D1Bitmap>,
}

impl Renderer {
    pub fn new() -> Result<Self> {
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

            let fmt_input = make(20.0, DWRITE_FONT_WEIGHT_NORMAL)?;
            fmt_input.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            let fmt_title = make(15.0, DWRITE_FONT_WEIGHT_NORMAL)?;
            fmt_title.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            let fmt_subtitle = make(12.0, DWRITE_FONT_WEIGHT_NORMAL)?;
            fmt_subtitle.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            fmt_subtitle.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_TRAILING)?;
            let fmt_subtitle_left = make(12.0, DWRITE_FONT_WEIGHT_NORMAL)?;
            fmt_subtitle_left.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            let fmt_panel = make(14.0, DWRITE_FONT_WEIGHT_NORMAL)?;
            fmt_panel.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

            Ok(Self {
                d2d,
                dwrite,
                fmt_input,
                fmt_title,
                fmt_subtitle,
                fmt_subtitle_left,
                fmt_panel,
                target: None,
            })
        }
    }

    unsafe fn ensure_target(&mut self, hwnd: HWND) -> Result<()> {
        if self.target.is_some() {
            return Ok(());
        }
        unsafe {
            let mut rc = RECT::default();
            GetClientRect(hwnd, &mut rc)?;
            let size = D2D_SIZE_U {
                width: (rc.right - rc.left).max(1) as u32,
                height: (rc.bottom - rc.top).max(1) as u32,
            };
            // Software rasterizer: our scene is trivial (one small window,
            // redraws only on input) and skipping D3D/DXGI device creation
            // saves ~50MB of process memory. CPU cost per frame is <1ms.
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_IGNORE,
                },
                dpiX: 0.0,
                dpiY: 0.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
                hwnd,
                pixelSize: size,
                presentOptions: D2D1_PRESENT_OPTIONS_NONE,
            };
            let rt = self.d2d.CreateHwndRenderTarget(&props, &hwnd_props)?;
            let dpi = GetDpiForWindow(hwnd) as f32;
            rt.SetDpi(dpi, dpi);

            let brush = |c: &D2D1_COLOR_F| -> Result<ID2D1SolidColorBrush> {
                rt.CreateSolidColorBrush(c, None)
            };
            self.target = Some(Target {
                text: brush(&COL_TEXT)?,
                dim: brush(&COL_DIM)?,
                faint: brush(&COL_FAINT)?,
                select: brush(&COL_SELECT)?,
                panel: brush(&COL_PANEL)?,
                icons: HashMap::new(),
                rt,
            });
            Ok(())
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if let Some(t) = &self.target {
            unsafe {
                let _ = t.rt.Resize(&D2D_SIZE_U { width, height });
            }
        }
    }

    pub fn update_dpi(&mut self, dpi: f32) {
        if let Some(t) = &self.target {
            unsafe { t.rt.SetDpi(dpi, dpi) };
        }
    }

    /// Width of `text` in DIPs when rendered with the input font.
    fn measure_input(&self, text: &str) -> f32 {
        self.measure(&self.fmt_input, text)
    }

    /// Width of `text` in DIPs when rendered with `format`.
    fn measure(&self, format: &IDWriteTextFormat, text: &str) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        unsafe {
            let units: Vec<u16> = text.encode_utf16().collect();
            let Ok(layout) = self.dwrite.CreateTextLayout(&units, format, 8192.0, 512.0) else {
                return 0.0;
            };
            let mut m = DWRITE_TEXT_METRICS::default();
            if layout.GetMetrics(&mut m).is_err() {
                return 0.0;
            }
            m.widthIncludingTrailingWhitespace
        }
    }

    pub fn draw(&mut self, hwnd: HWND, frame: &Frame) {
        let Frame {
            query,
            caret_visible,
            results,
            selected,
            panel,
        } = frame;
        let (query, selected) = (*query, *selected);
        unsafe {
            if self.ensure_target(hwnd).is_err() {
                return;
            }
            let caret_x = PAD_X + self.measure_input(query) + 1.0;
            let text_input_caret = match panel {
                Some(PanelView::TextInput { prompt, buffer }) => Some((
                    self.measure(&self.fmt_title, prompt),
                    self.measure(&self.fmt_title, buffer),
                )),
                _ => None,
            };
            let t = self.target.as_mut().unwrap();
            let rt = &t.rt;

            rt.BeginDraw();
            rt.Clear(Some(&COL_BG));
            let size = rt.GetSize();
            let width = size.width;
            let height = size.height;

            // Search input (query text or placeholder).
            let input_rect = D2D_RECT_F {
                left: PAD_X,
                top: 0.0,
                right: width - PAD_X,
                bottom: INPUT_H,
            };
            if query.is_empty() {
                draw_text(rt, "Search apps and commands…", &self.fmt_input, &input_rect, &t.dim);
            } else {
                draw_text(rt, query, &self.fmt_input, &input_rect, &t.text);
            }

            // Caret.
            if *caret_visible && panel.is_none() {
                let mid = INPUT_H / 2.0;
                rt.FillRectangle(
                    &D2D_RECT_F {
                        left: caret_x,
                        top: mid - 12.0,
                        right: caret_x + 2.0,
                        bottom: mid + 12.0,
                    },
                    &t.text,
                );
            }

            if !results.is_empty() {
                // Separator under the input.
                rt.FillRectangle(
                    &D2D_RECT_F {
                        left: 0.0,
                        top: INPUT_H,
                        right: width,
                        bottom: INPUT_H + 1.0,
                    },
                    &t.faint,
                );

                let mut y = INPUT_H + 1.0 + LIST_PAD;
                for (i, item) in results.iter().enumerate() {
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
                            &t.select,
                        );
                    }
                    if let Some(icon) = &item.icon {
                        if let Some(bmp) = icon_bitmap(rt, &mut t.icons, icon) {
                            let top = y + (ROW_H - ICON_SIZE) / 2.0;
                            rt.DrawBitmap(
                                &bmp,
                                Some(&D2D_RECT_F {
                                    left: PAD_X,
                                    top,
                                    right: PAD_X + ICON_SIZE,
                                    bottom: top + ICON_SIZE,
                                }),
                                1.0,
                                D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                                None,
                            );
                        }
                    }
                    let title_rect = D2D_RECT_F {
                        left: PAD_X + ICON_SIZE + ICON_GAP,
                        top: y,
                        right: width - PAD_X,
                        bottom: y + ROW_H,
                    };
                    draw_text(rt, &item.title, &self.fmt_title, &title_rect, &t.text);
                    if !item.subtitle.is_empty() {
                        let row = D2D_RECT_F {
                            left: PAD_X,
                            top: y,
                            right: width - PAD_X,
                            bottom: y + ROW_H,
                        };
                        draw_text(rt, &item.subtitle, &self.fmt_subtitle, &row, &t.dim);
                    }
                    y += ROW_H;
                }

                // Bottom bar with key hints.
                let bar_top = height - BAR_H;
                rt.FillRectangle(
                    &D2D_RECT_F {
                        left: 0.0,
                        top: bar_top,
                        right: width,
                        bottom: bar_top + 1.0,
                    },
                    &t.faint,
                );
                let bar_rect = D2D_RECT_F {
                    left: PAD_X,
                    top: bar_top,
                    right: width - PAD_X,
                    bottom: height,
                };
                draw_text(rt, "Apex", &self.fmt_subtitle_left, &bar_rect, &t.dim);
                draw_text(
                    rt,
                    "Open \u{21b5}      Actions Ctrl+K",
                    &self.fmt_subtitle,
                    &bar_rect,
                    &t.dim,
                );

                // Overlay panel (actions / text input), bottom-right.
                if let Some(view) = panel {
                    let rows = match view {
                        PanelView::Actions { actions, .. } => actions.len().max(1) as f32,
                        PanelView::TextInput { .. } => 1.0,
                    };
                    let panel_h = rows * PANEL_ROW + PANEL_PAD * 2.0;
                    let rect = D2D_RECT_F {
                        left: width - PANEL_MARGIN - PANEL_W,
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
                        &t.panel,
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
                                        &t.select,
                                    );
                                }
                                let label_rect = D2D_RECT_F {
                                    left: row.left + 10.0,
                                    right: row.right - 10.0,
                                    ..row
                                };
                                draw_text(rt, &action.label, &self.fmt_panel, &label_rect, &t.text);
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
                            draw_text(rt, &prompt_text, &self.fmt_panel, &row, &t.dim);
                            if let Some((pw, bw)) = text_input_caret {
                                let text_rect = D2D_RECT_F {
                                    left: row.left + pw + 8.0,
                                    ..row
                                };
                                draw_text(rt, buffer, &self.fmt_panel, &text_rect, &t.text);
                                let cx = text_rect.left + bw + 1.0;
                                let mid = (row.top + row.bottom) / 2.0;
                                rt.FillRectangle(
                                    &D2D_RECT_F {
                                        left: cx,
                                        top: mid - 9.0,
                                        right: cx + 1.5,
                                        bottom: mid + 9.0,
                                    },
                                    &t.text,
                                );
                            }
                        }
                    }
                }
            }

            let end = rt.EndDraw(None, None);
            if let Err(e) = &end {
                crate::dlog!("draw: EndDraw failed: {e}");
            }
            let recreate = matches!(end, Err(e) if e.code() == D2DERR_RECREATE_TARGET);
            if recreate {
                self.target = None;
            }
        }
    }
}

/// Get (or create) the per-target D2D bitmap for an icon.
unsafe fn icon_bitmap(
    rt: &ID2D1HwndRenderTarget,
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
    rt: &ID2D1HwndRenderTarget,
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
