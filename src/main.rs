#![windows_subsystem = "windows"]

mod config;
mod mcp;
mod profile;
mod update;
mod usage;
mod web;

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use windows::core::{w, Result, HRESULT, HSTRING};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows_numerics::Vector2;

const WM_DATA: u32 = WM_APP + 1;
const WM_TRAY: u32 = WM_APP + 2;
const ID_REFRESH: usize = 1;
const ID_CONFIG: usize = 2;
const ID_MOVE: usize = 3;
const ID_QUIT: usize = 4;
const ID_MCP: usize = 5;
const ID_PROFILES: usize = 6;
const ID_UPDATE: usize = 7;
const ID_STARTUP: usize = 8;

const RUN_KEY: windows::core::PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: windows::core::PCWSTR = w!("ClaudeUsageWidget");

fn startup_enabled() -> bool {
    use windows::Win32::System::Registry::*;
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, KEY_READ, &mut key).is_err() {
            return false;
        }
        let r = RegQueryValueExW(key, RUN_VALUE, None, None, None, None);
        let _ = RegCloseKey(key);
        r.is_ok()
    }
}

fn set_startup(enable: bool) {
    use windows::Win32::System::Registry::*;
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, KEY_SET_VALUE, &mut key).is_err() {
            return;
        }
        if enable {
            if let Ok(exe) = std::env::current_exe() {
                let cmd = format!("\"{}\"", exe.display());
                let wide: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
                let bytes = std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
                let _ = RegSetValueExW(key, RUN_VALUE, None, REG_SZ, Some(bytes));
            }
        } else {
            let _ = RegDeleteValueW(key, RUN_VALUE);
        }
        let _ = RegCloseKey(key);
    }
}

// Layout, in DIPs.
const PAD: f32 = 8.0;
const CELL_W: f32 = 62.0;
const R_OUT: f32 = 16.0;
const R_IN: f32 = 10.0;
const STROKE: f32 = 4.0;
const CELL_H: f32 = 2.0 * (R_OUT + STROKE / 2.0) + 28.0;

#[derive(Clone)]
struct ProfileState {
    name: String,
    /// Used percent (0..=100); None while loading or on error.
    session_used: Option<f32>,
    weekly_used: Option<f32>,
    /// Local date the weekly window resets, e.g. "Oct 29".
    weekly_reset: Option<String>,
    error: Option<String>,
}

static STATE: Mutex<Vec<ProfileState>> = Mutex::new(Vec::new());
static REFRESH_TX: OnceLock<Sender<()>> = OnceLock::new();
/// When unlocked the window catches the mouse so it can be dragged.
static UNLOCKED: AtomicBool = AtomicBool::new(false);
/// Set while an update check or install is running, so a second click is ignored.
static UPDATING: AtomicBool = AtomicBool::new(false);
/// Tray icon handle, drawn at startup.
static TRAY_ICON: AtomicIsize = AtomicIsize::new(0);

struct App {
    factory: ID2D1Factory,
    stroke: ID2D1StrokeStyle,
    fmt_big: IDWriteTextFormat,
    fmt_name: IDWriteTextFormat,
    fmt_small: IDWriteTextFormat,
    fmt_tiny: IDWriteTextFormat,
    rt: ID2D1DCRenderTarget,
    brush: ID2D1SolidColorBrush,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn rgba(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

/// Colour by percent used: red above 90%, orange above 60%, otherwise green.
fn level_color(used: f32) -> D2D1_COLOR_F {
    if used > 90.0 {
        rgba(0.92, 0.33, 0.33, 1.0)
    } else if used > 60.0 {
        rgba(0.98, 0.58, 0.16, 1.0)
    } else {
        rgba(0.30, 0.78, 0.45, 1.0)
    }
}

fn make_format(dw: &IDWriteFactory, size: f32, weight: DWRITE_FONT_WEIGHT) -> Result<IDWriteTextFormat> {
    let f = unsafe {
        dw.CreateTextFormat(
            w!("Segoe UI"),
            None,
            weight,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size,
            w!("en-us"),
        )?
    };
    unsafe {
        f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
        f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
    }
    Ok(f)
}

/// A single horizontal row: one column per profile.
fn grid(count: usize) -> (usize, usize) {
    (count.max(1), 1)
}

fn window_size(count: usize, dpi: u32) -> (i32, i32) {
    let scale = dpi as f32 / 96.0;
    let (cols, rows) = grid(count);
    let w = (PAD * 2.0 + CELL_W * cols as f32) * scale;
    let h = (PAD * 2.0 + CELL_H * rows as f32) * scale;
    (w.round() as i32, h.round() as i32)
}

impl App {
    fn new() -> Result<Self> {
        unsafe {
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let props = D2D1_STROKE_STYLE_PROPERTIES {
                startCap: D2D1_CAP_STYLE_ROUND,
                endCap: D2D1_CAP_STYLE_ROUND,
                dashCap: D2D1_CAP_STYLE_ROUND,
                lineJoin: D2D1_LINE_JOIN_ROUND,
                miterLimit: 10.0,
                dashStyle: D2D1_DASH_STYLE_SOLID,
                dashOffset: 0.0,
            };
            let stroke = factory.CreateStrokeStyle(&props, None)?;
            let rt_props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 0.0,
                dpiY: 0.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let rt = factory.CreateDCRenderTarget(&rt_props)?;
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            let brush = rt.CreateSolidColorBrush(&rgba(1.0, 1.0, 1.0, 1.0), None)?;
            Ok(App {
                stroke,
                fmt_big: make_format(&dw, 8.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)?,
                fmt_name: make_format(&dw, 9.5, DWRITE_FONT_WEIGHT_SEMI_BOLD)?,
                fmt_small: make_format(&dw, 8.0, DWRITE_FONT_WEIGHT_NORMAL)?,
                fmt_tiny: make_format(&dw, 7.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)?,
                factory,
                rt,
                brush,
            })
        }
    }

    /// Draws an arc starting at 12 o'clock, clockwise, covering `fraction` of the circle.
    fn arc(&self, center: Vector2, radius: f32, fraction: f32) -> Result<()> {
        self.arc_w(center, radius, fraction, STROKE)
    }

    fn arc_w(&self, center: Vector2, radius: f32, fraction: f32, width: f32) -> Result<()> {
        unsafe {
            if fraction >= 0.999 {
                let e = D2D1_ELLIPSE { point: center, radiusX: radius, radiusY: radius };
                self.rt.DrawEllipse(&e, &self.brush, width, &self.stroke);
                return Ok(());
            }
            if fraction <= 0.001 {
                return Ok(());
            }
            let angle = -std::f32::consts::FRAC_PI_2 + fraction * std::f32::consts::TAU;
            let start = Vector2 { X: center.X, Y: center.Y - radius };
            let end = Vector2 {
                X: center.X + radius * angle.cos(),
                Y: center.Y + radius * angle.sin(),
            };
            let geo = self.factory.CreatePathGeometry()?;
            let sink = geo.Open()?;
            sink.BeginFigure(start, D2D1_FIGURE_BEGIN_HOLLOW);
            sink.AddArc(&D2D1_ARC_SEGMENT {
                point: end,
                size: D2D_SIZE_F { width: radius, height: radius },
                rotationAngle: 0.0,
                sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
                arcSize: if fraction > 0.5 { D2D1_ARC_SIZE_LARGE } else { D2D1_ARC_SIZE_SMALL },
            });
            sink.EndFigure(D2D1_FIGURE_END_OPEN);
            sink.Close()?;
            self.rt.DrawGeometry(&geo, &self.brush, width, &self.stroke);
        }
        Ok(())
    }

    /// Text with a soft drop shadow so it stays readable on any background.
    fn text(&self, fmt: &IDWriteTextFormat, s: &str, rect: D2D_RECT_F, c: D2D1_COLOR_F) {
        let wide: Vec<u16> = s.encode_utf16().collect();
        let draw = |r: D2D_RECT_F, c: D2D1_COLOR_F| unsafe {
            self.brush.SetColor(&c);
            self.rt.DrawText(
                &wide,
                fmt,
                &r,
                &self.brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        };
        draw(
            D2D_RECT_F { left: rect.left + 1.0, top: rect.top + 1.0, right: rect.right + 1.0, bottom: rect.bottom + 1.0 },
            rgba(0.0, 0.0, 0.0, 0.75),
        );
        draw(rect, c);
    }

    fn set_brush(&self, c: D2D1_COLOR_F) {
        unsafe { self.brush.SetColor(&c) };
    }

    fn draw(&self, w: i32, h: i32, dpi: u32) -> Result<()> {
        let profiles = STATE.lock().unwrap().clone();
        let (cols, _) = grid(profiles.len());
        let track = rgba(0.55, 0.57, 0.62, 0.35);
        let fg = rgba(0.96, 0.97, 0.98, 1.0);
        let dim = rgba(0.82, 0.84, 0.88, 1.0);
        let scale = dpi as f32 / 96.0;

        unsafe {
            self.rt.BeginDraw();
            self.rt.Clear(Some(&rgba(0.0, 0.0, 0.0, 0.0)));

            // While unlocked, show a light backdrop so the window is visible and draggable.
            if UNLOCKED.load(Ordering::Relaxed) {
                let rr = D2D1_ROUNDED_RECT {
                    rect: D2D_RECT_F { left: 0.5, top: 0.5, right: w as f32 / scale - 0.5, bottom: h as f32 / scale - 0.5 },
                    radiusX: 8.0,
                    radiusY: 8.0,
                };
                self.set_brush(rgba(0.10, 0.11, 0.13, 0.55));
                self.rt.FillRoundedRectangle(&rr, &self.brush);
                self.set_brush(rgba(0.30, 0.78, 0.45, 0.9));
                self.rt.DrawRoundedRectangle(&rr, &self.brush, 1.0, None);
            }
        }

        for (i, p) in profiles.iter().enumerate() {
            let (col, row) = (i % cols, i / cols);
            let cx = PAD + CELL_W * (col as f32 + 0.5);
            let cy = PAD + CELL_H * row as f32 + R_OUT + STROKE / 2.0;
            let center = Vector2 { X: cx, Y: cy };

            // Tracks, then fills (outer = weekly, inner = session).
            self.set_brush(track);
            self.arc(center, R_OUT, 1.0)?;
            self.arc(center, R_IN, 1.0)?;
            if let Some(wk) = p.weekly_used {
                self.set_brush(level_color(wk));
                self.arc(center, R_OUT, wk / 100.0)?;
            }
            if let Some(s) = p.session_used {
                self.set_brush(level_color(s));
                self.arc(center, R_IN, s / 100.0)?;
            }

            // Weekly percent as a small pill sitting on the outer ring at 12 o'clock.
            if let Some(wk) = p.weekly_used {
                let pill = D2D_RECT_F { left: cx - 10.0, top: cy - R_OUT - 4.5, right: cx + 10.0, bottom: cy - R_OUT + 4.5 };
                unsafe {
                    self.set_brush(rgba(0.08, 0.09, 0.11, 0.85));
                    self.rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: pill, radiusX: 4.5, radiusY: 4.5 }, &self.brush);
                }
                self.text(&self.fmt_tiny, &format!("{:.0}%", wk), pill, level_color(wk));
            }

            let inner = R_IN - STROKE / 2.0;
            let center_rect = D2D_RECT_F { left: cx - inner, top: cy - inner, right: cx + inner, bottom: cy + inner };
            let label = match (&p.error, p.session_used) {
                (Some(_), _) => "!".to_string(),
                (None, Some(s)) => format!("{:.0}", s),
                (None, None) => "…".to_string(),
            };
            self.text(&self.fmt_big, &label, center_rect, fg);

            let top = cy + R_OUT + STROKE / 2.0 + 1.0;
            let half = CELL_W / 2.0;
            let name_rect = D2D_RECT_F { left: cx - half, top, right: cx + half, bottom: top + 13.0 };
            self.text(&self.fmt_name, &p.name, name_rect, fg);

            let sub = match (&p.error, &p.weekly_reset) {
                (Some(e), _) => e.clone(),
                (None, Some(date)) => format!("Resets {date}"),
                (None, None) => String::new(),
            };
            let sub_rect = D2D_RECT_F { left: cx - half, top: top + 12.0, right: cx + half, bottom: top + 23.0 };
            self.text(&self.fmt_small, &sub, sub_rect, dim);
        }

        unsafe { self.rt.EndDraw(None, None) }
    }

    /// Draws the app icon (dark disc with a double ring) at `size` px and wraps it in an HICON.
    fn make_icon(&self, size: i32) -> Result<HICON> {
        unsafe {
            let screen = GetDC(None);
            let mem = CreateCompatibleDC(Some(screen));
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: size,
                    biHeight: -size,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let color = CreateDIBSection(Some(mem), &bmi, DIB_RGB_COLORS, &mut bits, None, 0);
            let icon = color.and_then(|color| {
                let old = SelectObject(mem, color.into());
                self.rt.SetDpi(96.0, 96.0);
                let s = size as f32;
                let c = Vector2 { X: s / 2.0, Y: s / 2.0 };
                let drawn = self.rt.BindDC(mem, &RECT { left: 0, top: 0, right: size, bottom: size }).and_then(|_| {
                    self.rt.BeginDraw();
                    self.rt.Clear(Some(&rgba(0.0, 0.0, 0.0, 0.0)));
                    self.set_brush(rgba(0.08, 0.09, 0.11, 1.0));
                    self.rt.FillEllipse(&D2D1_ELLIPSE { point: c, radiusX: s / 2.0, radiusY: s / 2.0 }, &self.brush);
                    let w = s * 0.14;
                    let (r_out, r_in) = (s * 0.375, s * 0.2);
                    self.set_brush(rgba(0.35, 0.37, 0.42, 1.0));
                    self.arc_w(c, r_out, 1.0, w)?;
                    self.arc_w(c, r_in, 1.0, w)?;
                    self.set_brush(rgba(0.98, 0.58, 0.16, 1.0));
                    self.arc_w(c, r_out, 0.72, w)?;
                    self.set_brush(rgba(0.30, 0.78, 0.45, 1.0));
                    self.arc_w(c, r_in, 0.55, w)?;
                    self.rt.EndDraw(None, None)
                });
                SelectObject(mem, old);
                let icon = drawn.and_then(|_| {
                    let mask = CreateBitmap(size, size, 1, 1, None);
                    let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
                    let icon = CreateIconIndirect(&info);
                    let _ = DeleteObject(mask.into());
                    icon
                });
                let _ = DeleteObject(color.into());
                icon
            });
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);
            icon
        }
    }

    /// Renders into a 32-bit DIB and pushes it to the layered window (per-pixel alpha).
    fn paint(&self, hwnd: HWND) -> Result<()> {
        unsafe {
            let dpi = GetDpiForWindow(hwnd);
            let count = STATE.lock().unwrap().len();
            let (w, h) = window_size(count, dpi);

            let screen = GetDC(None);
            let mem = CreateCompatibleDC(Some(screen));
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let bmp = CreateDIBSection(Some(mem), &bmi, DIB_RGB_COLORS, &mut bits, None, 0);
            let result = bmp.and_then(|bmp| {
                let old = SelectObject(mem, bmp.into());
                self.rt.SetDpi(dpi as f32, dpi as f32);
                let r = self
                    .rt
                    .BindDC(mem, &RECT { left: 0, top: 0, right: w, bottom: h })
                    .and_then(|_| self.draw(w, h, dpi))
                    .and_then(|_| {
                        let size = SIZE { cx: w, cy: h };
                        let src = POINT { x: 0, y: 0 };
                        let blend = BLENDFUNCTION {
                            BlendOp: AC_SRC_OVER as u8,
                            BlendFlags: 0,
                            SourceConstantAlpha: 255,
                            AlphaFormat: AC_SRC_ALPHA as u8,
                        };
                        UpdateLayeredWindow(
                            hwnd,
                            Some(screen),
                            None,
                            Some(&size),
                            Some(mem),
                            Some(&src),
                            Default::default(),
                            Some(&blend),
                            ULW_ALPHA,
                        )
                    });
                SelectObject(mem, old);
                let _ = DeleteObject(bmp.into());
                r
            });
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);
            result
        }
    }
}

fn repaint(hwnd: HWND) {
    APP.with(|a| {
        if let Some(app) = a.borrow().as_ref() {
            let _ = app.paint(hwnd);
        }
    });
}

fn tray_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: HICON(TRAY_ICON.load(Ordering::Relaxed) as *mut _),
        ..Default::default()
    };
    for (dst, src) in nid.szTip.iter_mut().zip("Usage Rings for Claude Code".encode_utf16()) {
        *dst = src;
    }
    nid
}

fn set_click_through(hwnd: HWND, through: bool) {
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let flag = WS_EX_TRANSPARENT.0 as isize;
        let ex = if through { ex | flag } else { ex & !flag };
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex);
    }
}

/// Opens one page of the local manager UI in the default browser.
fn open_page(page: &str) {
    if let Some(url) = web::url(page) {
        unsafe {
            ShellExecuteW(None, w!("open"), &HSTRING::from(url), None, None, SW_SHOWNORMAL);
        }
    }
}

fn message(text: &str, flags: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    unsafe { MessageBoxW(None, &HSTRING::from(text), w!("Usage Rings for Claude Code"), flags | MB_TOPMOST) }
}

/// Checks GitHub for a newer release and, if the user agrees, installs it and restarts.
fn check_for_updates(hwnd: HWND) {
    if UPDATING.swap(true, Ordering::SeqCst) {
        return;
    }
    let hwnd = hwnd.0 as isize;
    std::thread::spawn(move || {
        let current = env!("CARGO_PKG_VERSION");
        let outcome = (|| -> std::result::Result<bool, String> {
            let release = update::fetch_latest()?;
            if !update::is_newer(&release.tag, current) {
                message(&format!("You're up to date (v{current})."), MB_OK | MB_ICONINFORMATION);
                return Ok(false);
            }
            let ask = format!("Update to {}? The app will restart.", release.tag);
            if message(&ask, MB_YESNO | MB_ICONQUESTION) != IDYES {
                return Ok(false);
            }
            let ready = update::download_and_verify(&release)?;
            update::start_replace_helper(&ready)?;
            Ok(true)
        })();
        match outcome {
            Ok(true) => unsafe {
                // Exit so the helper can swap the exe; it relaunches the new one.
                let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
                return;
            },
            Ok(false) => {}
            Err(e) => {
                message(&format!("Update failed: {e}"), MB_OK | MB_ICONERROR);
            }
        }
        UPDATING.store(false, Ordering::SeqCst);
    });
}

fn show_menu(hwnd: HWND) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let unlocked = UNLOCKED.load(Ordering::Relaxed);
        let _ = AppendMenuW(menu, MF_STRING, ID_REFRESH, w!("Refresh now"));
        let _ = AppendMenuW(
            menu,
            if unlocked { MF_STRING | MF_CHECKED } else { MF_STRING },
            ID_MOVE,
            w!("Unlock to move"),
        );
        let _ = AppendMenuW(menu, MF_STRING, ID_PROFILES, w!("Profiles"));
        let _ = AppendMenuW(menu, MF_STRING, ID_MCP, w!("MCP servers"));
        let _ = AppendMenuW(menu, MF_STRING, ID_CONFIG, w!("Settings"));
        let startup = startup_enabled();
        let _ = AppendMenuW(
            menu,
            if startup { MF_STRING | MF_CHECKED } else { MF_STRING },
            ID_STARTUP,
            w!("Run at Windows startup"),
        );
        let _ = AppendMenuW(menu, MF_STRING, ID_UPDATE, w!("Check for updates"));
        let _ = AppendMenuW(menu, MF_STRING, ID_QUIT, w!("Quit"));

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, None, hwnd, None);
        let _ = DestroyMenu(menu);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));

        match cmd.0 as usize {
            ID_REFRESH => {
                if let Some(tx) = REFRESH_TX.get() {
                    let _ = tx.send(());
                }
            }
            ID_MOVE => {
                let now = !unlocked;
                UNLOCKED.store(now, Ordering::Relaxed);
                set_click_through(hwnd, !now);
                repaint(hwnd);
            }
            ID_PROFILES => open_page("/profiles"),
            ID_MCP => open_page("/mcp"),
            ID_CONFIG => open_page("/settings"),
            ID_STARTUP => set_startup(!startup),
            ID_UPDATE => check_for_updates(hwnd),
            ID_QUIT => {
                let _ = DestroyWindow(hwnd);
            }
            _ => {}
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => {
            if UNLOCKED.load(Ordering::Relaxed) {
                LRESULT(HTCAPTION as isize)
            } else {
                LRESULT(HTTRANSPARENT as isize)
            }
        }
        WM_DATA => {
            repaint(hwnd);
            LRESULT(0)
        }
        WM_DPICHANGED | WM_DISPLAYCHANGE => {
            repaint(hwnd);
            LRESULT(0)
        }
        WM_TRAY => {
            let event = (lparam.0 & 0xffff) as u32;
            if event == WM_RBUTTONUP || event == WM_LBUTTONUP {
                show_menu(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = Shell_NotifyIconW(NIM_DELETE, &tray_data(hwnd));
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Polls usage for every configured profile; config is re-read each cycle.
fn spawn_worker(hwnd: isize, rx: mpsc::Receiver<()>) {
    std::thread::spawn(move || loop {
        let cfg = config::load();
        let dirs: Vec<_> = cfg.profiles.iter().map(config::resolve_dir).collect();

        // Keep the previous reading for profiles that are still configured (matched by name),
        // so a refresh doesn't blank the widget and a 429 can fall back to it.
        {
            let mut st = STATE.lock().unwrap();
            let old = std::mem::take(&mut *st);
            *st = cfg
                .profiles
                .iter()
                .map(|p| {
                    old.iter().find(|o| o.name == p.name).cloned().unwrap_or_else(|| ProfileState {
                        name: p.name.clone(),
                        session_used: None,
                        weekly_used: None,
                        weekly_reset: None,
                        error: None,
                    })
                })
                .collect();
        }
        notify(hwnd);

        let handles: Vec<_> = dirs
            .into_iter()
            .enumerate()
            .map(|(i, dir)| {
                std::thread::spawn(move || {
                    let r = usage::fetch(&dir);
                    let mut st = STATE.lock().unwrap();
                    if let Some(s) = st.get_mut(i) {
                        match r {
                            Ok(u) => {
                                s.session_used = u.session_used.map(|x| x.clamp(0.0, 100.0));
                                s.weekly_used = u.weekly_used.map(|x| x.clamp(0.0, 100.0));
                                s.weekly_reset = u.weekly_reset;
                                s.error = None;
                            }
                            Err(e) => {
                                s.session_used = None;
                                s.weekly_used = None;
                                s.weekly_reset = None;
                                s.error = Some(e);
                            }
                        }
                    }
                    drop(st);
                    notify(hwnd);
                })
            })
            .collect();
        for h in handles {
            let _ = h.join();
        }

        let wait = Duration::from_secs(cfg.refresh_seconds.max(15));
        if let Err(mpsc::RecvTimeoutError::Disconnected) = rx.recv_timeout(wait) {
            return;
        }
    });
}

fn notify(hwnd: isize) {
    unsafe {
        let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_DATA, WPARAM(0), LPARAM(0));
    }
}

fn main() -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let hinst = GetModuleHandleW(None)?;
        let class = w!("ClaudeUsageWidget");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);

        APP.with(|a| *a.borrow_mut() = Some(App::new().expect("direct2d init")));

        let icon_size = GetSystemMetrics(SM_CXSMICON).max(16);
        let icon = APP.with(|a| a.borrow().as_ref().and_then(|app| app.make_icon(icon_size).ok()));
        if let Some(icon) = icon {
            TRAY_ICON.store(icon.0 as isize, Ordering::Relaxed);
        }

        let count = config::load().profiles.len();
        let (w, h) = window_size(count, 96);
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            class,
            w!("Usage Rings for Claude Code"),
            WS_POPUP,
            0,
            0,
            w,
            h,
            None,
            None,
            Some(hinst.into()),
            None,
        )?;

        // Park in the bottom-right of the work area at the real DPI.
        let (w, h) = window_size(count, GetDpiForWindow(hwnd));
        let mut work = RECT::default();
        let _ = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut work as *mut RECT as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        let _ = SetWindowPos(hwnd, None, work.right - w - 16, work.bottom - h - 16, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        repaint(hwnd);

        let _ = Shell_NotifyIconW(NIM_ADD, &tray_data(hwnd));

        let (tx, rx) = mpsc::channel();
        let _ = REFRESH_TX.set(tx);
        spawn_worker(hwnd.0 as isize, rx);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

#[allow(dead_code)]
const _RECREATE: HRESULT = HRESULT(0x8899000Cu32 as i32);
