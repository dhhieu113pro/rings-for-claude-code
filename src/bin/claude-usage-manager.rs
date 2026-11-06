#![windows_subsystem = "windows"]
//! Native manager window. One process, one window: a second launch hands its page to the
//! running instance (named mutex + event + `manager-nav.txt`) and exits.
use claude_usage_widget::config;
use claude_usage_widget::manager::{self, ManagerState, Page, ProfileCard, Theme};
use claude_usage_widget::update;
use std::path::PathBuf;
use windows::core::{w, HSTRING};
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, SetEvent, WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, MessageBoxW, SetForegroundWindow, ShowWindow, MB_ICONERROR, MB_OK, SW_RESTORE,
};
use windows_reactor::*;

const TITLE: &str = "Usage Rings for Claude Code \u{2013} Manager";

/// The app icon (dark disc, grey double ring, orange outer and green inner arc) as straight-alpha
/// RGBA, drawn the same way as the widget's tray icon. 4x4 supersampling keeps the edges smooth.
fn icon_rgba(size: u32) -> Vec<u8> {
    let s = size as f32;
    let (c, w) = (s / 2.0, s * 0.14);
    let (r_out, r_in) = (s * 0.375, s * 0.2);
    // Colour of one sample point, or None when it is outside the disc.
    let sample = |x: f32, y: f32| -> Option<[f32; 3]> {
        let (dx, dy) = (x - c, y - c);
        let r = (dx * dx + dy * dy).sqrt();
        if r > s / 2.0 {
            return None;
        }
        let turn = dx.atan2(-dy).rem_euclid(std::f32::consts::TAU) / std::f32::consts::TAU;
        let ring = |radius: f32| (r - radius).abs() <= w / 2.0;
        Some(if ring(r_out) {
            if turn <= 0.72 { [0.98, 0.58, 0.16] } else { [0.35, 0.37, 0.42] }
        } else if ring(r_in) {
            if turn <= 0.55 { [0.30, 0.78, 0.45] } else { [0.35, 0.37, 0.42] }
        } else {
            [0.08, 0.09, 0.11]
        })
    };
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let (mut rgb, mut hit) = ([0.0f32; 3], 0.0f32);
            for sy in 0..4 {
                for sx in 0..4 {
                    let (x, y) = (px as f32 + (sx as f32 + 0.5) / 4.0, py as f32 + (sy as f32 + 0.5) / 4.0);
                    if let Some(col) = sample(x, y) {
                        hit += 1.0;
                        for i in 0..3 {
                            rgb[i] += col[i];
                        }
                    }
                }
            }
            let a = hit / 16.0;
            let div = hit.max(1.0);
            out.extend([0, 1, 2].map(|i| (rgb[i] / div * 255.0).round() as u8));
            out.push((a * 255.0).round() as u8);
        }
    }
    out
}

/// Wraps the icon at several sizes in an .ico file (BMP/DIB entries, bottom-up BGRA + empty mask).
fn ico_bytes(sizes: &[u32]) -> Vec<u8> {
    let mut images: Vec<Vec<u8>> = Vec::new();
    for &size in sizes {
        let rgba = icon_rgba(size);
        let mut img = Vec::new();
        img.extend(40u32.to_le_bytes()); // BITMAPINFOHEADER
        img.extend((size as i32).to_le_bytes());
        img.extend((size as i32 * 2).to_le_bytes()); // colour + mask
        img.extend(1u16.to_le_bytes());
        img.extend(32u16.to_le_bytes());
        img.extend([0u8; 24]);
        for row in (0..size).rev() {
            for col in 0..size {
                let i = ((row * size + col) * 4) as usize;
                img.extend([rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
            }
        }
        img.extend(vec![0u8; (size.div_ceil(32) * 4 * size) as usize]); // AND mask: all opaque
        images.push(img);
    }
    let mut out = Vec::new();
    out.extend([0u8, 0, 1, 0]);
    out.extend((sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (&size, img) in sizes.iter().zip(&images) {
        out.extend([size as u8, size as u8, 0, 0]);
        out.extend(1u16.to_le_bytes());
        out.extend(32u16.to_le_bytes());
        out.extend((img.len() as u32).to_le_bytes());
        out.extend(offset.to_le_bytes());
        offset += img.len() as u32;
    }
    for img in images {
        out.extend(img);
    }
    out
}

/// Windows' "app mode" setting, read once at startup. The window's own colour-scheme callback is
/// not used for this: the theme we set feeds back into it and flipped the page to light.
fn system_prefers_dark() -> bool {
    // Test hook: CLAUDE_MANAGER_THEME=light|dark overrides the system setting.
    match std::env::var("CLAUDE_MANAGER_THEME").as_deref() {
        Ok("light") => return false,
        Ok("dark") => return true,
        _ => {}
    }
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let mut value: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    // Missing value (older Windows) means light.
    status.is_ok() && value == 0
}

/// Writes the icon next to the config and returns its path for the window.
fn window_icon_path() -> Option<&'static str> {
    let path = config::config_path().with_file_name("manager.ico");
    std::fs::create_dir_all(path.parent()?).ok()?;
    std::fs::write(&path, ico_bytes(&[16, 24, 32, 48, 64])).ok()?;
    Some(Box::leak(path.to_string_lossy().into_owned().into_boxed_str()))
}

fn nav_file() -> PathBuf {
    config::config_path().with_file_name("manager-nav.txt")
}

#[derive(Clone, PartialEq)]
struct ManagerInput {
    first: Page,
}

#[derive(Clone)]
enum ManagerMsg {
    /// The user picked a page in the sidebar.
    Select(Page),
    /// Another launch asked this window to show a page.
    Widget(Page),
    /// The sidebar was collapsed or expanded.
    ToggleNav,
    /// The title-bar button: bring up the rings widget, starting it if it is not running.
    ShowRings,
    /// A selection event with nothing to act on.
    Ignore,
    /// The Settings page saved a new theme.
    SetTheme(Theme),
}

struct ManagerRoot {
    /// Saved choice; `System` follows `system_dark`.
    theme: Theme,
    system_dark: bool,
    nav_open: bool,
    /// When we last changed the page ourselves, to tell our own selection events from the user's.
    programmatic_at: Option<std::time::Instant>,
    icon: Option<&'static str>,
    page: Page,
}

impl ManagerRoot {
    fn dark(&self) -> bool {
        self.theme.is_dark(self.system_dark)
    }
}

/// Blocks (in the background) until another launch signals; yields the requested page.
fn wait_for_navigation(context: &ComponentContext<ManagerRoot>) {
    context.spawn_background(|token| {
        let ev = unsafe { CreateEventW(None, false, false, w!("Local\\ClaudeUsageManagerNavigate")) };
        let Ok(ev) = ev else { return ManagerMsg::Widget(Page::Home) };
        loop {
            if token.is_cancelled() {
                return ManagerMsg::Ignore;
            }
            if unsafe { WaitForSingleObject(ev, 500) } == WAIT_OBJECT_0 {
                let page = std::fs::read_to_string(nav_file())
                    .ok()
                    .and_then(|t| Page::parse(t.trim()))
                    .unwrap_or(Page::Home);
                return ManagerMsg::Widget(page);
            }
        }
    });
}

impl Component for ManagerRoot {
    type Input = ManagerInput;
    type Message = ManagerMsg;

    fn create(input: &ManagerInput, context: &ComponentContext<Self>) -> Self {
        wait_for_navigation(context);
        Self {
            page: input.first,
            theme: Theme::parse(&config::load().theme),
            system_dark: system_prefers_dark(),
            nav_open: true,
            programmatic_at: None,
            icon: window_icon_path(),
        }
    }

    fn update(&mut self, message: ManagerMsg, context: &ComponentContext<Self>) {
        match message {
            ManagerMsg::Select(p) => {
                // NavigationView also reports selection changes we caused ourselves (e.g. when a
                // second launch switches page); those must not overwrite the requested page.
                let ours = self.programmatic_at.is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(600));
                if !ours {
                    self.page = p;
                }
            }
            ManagerMsg::Ignore => {}
            ManagerMsg::ToggleNav => self.nav_open = !self.nav_open,
            ManagerMsg::ShowRings => show_rings(),
            ManagerMsg::SetTheme(t) => self.theme = t,
            ManagerMsg::Widget(p) => {
                self.page = p;
                self.programmatic_at = Some(std::time::Instant::now());
                wait_for_navigation(context);
                focus_window();
            }
        }
    }

    fn view(&self, _input: &ManagerInput, context: &mut ViewContext<Self>) -> View {
        context.window_title(TITLE);
        // Mica makes the pane and title bar follow the theme. The content card has its own solid
        // colour because the canvas rings cannot be transparent.
        let mut visuals = WindowVisuals::new()
            .backdrop(WindowBackdrop::Mica)
            .theme(if self.dark() { WindowTheme::Dark } else { WindowTheme::Light })
            .client_size(1000.0, 700.0);
        if let Some(icon) = self.icon {
            visuals = visuals.icon(icon);
        }
        context.window_visuals(visuals);
        let icon_of = |p: Page| match p {
            Page::Home => Symbol::Home,
            Page::Profiles => Symbol::People,
            Page::Mcp => Symbol::Link,
            Page::Settings => Symbol::Setting,
        };
        let items: Vec<KeyedView> = Page::ALL
            .into_iter()
            .map(|p| {
                KeyedView::new(
                    p.arg(),
                    NavigationViewItem::new()
                        .tag(p.arg())
                        .is_selected(p == self.page)
                        .slots([
                            SlotView::new(NavigationViewItemSlot::Icon, SymbolIcon::new().symbol(icon_of(p))),
                            SlotView::new(NavigationViewItemSlot::Content, TextBlock::new().text(p.label())),
                        ]),
                )
            })
            .collect();
        let page: View = match self.page {
            Page::Home => View::component::<HomePage>(self.dark()),
            Page::Profiles => View::component::<ProfilesPage>(self.dark()),
            Page::Mcp => View::component::<McpPage>(self.dark()),
            Page::Settings => View::component::<SettingsPage>(SettingsInput {
                on_theme: context.callback(ManagerMsg::SetTheme),
                theme: self.theme,
                dark: self.dark(),
            }),
        };
        // An explicit page colour keeps the canvas rings seamless in every theme.
        let g = page_gray(self.dark());
        let content: View = ScrollViewer::new().content(
            Grid::new().background(Color::rgb(g, g, g)).children((StackPanel::new()
                .spacing(16.0)
                .margin(Thickness::uniform(24.0))
                .children((TextBlock::new().text(self.page.label()).font_size(24.0), page)),)),
        );
        // Same layout as the reactor gallery: an extended title bar (pane toggle, app name and
        // current page, theme button) above a NavigationView.
        let title_bar = TitleBar::new()
            .title("Usage Rings for Claude Code")
            .subtitle(self.page.label())
            .is_back_button_visible(false)
            .is_pane_toggle_button_visible(true)
            .on_pane_toggle_requested(context.message(ManagerMsg::ToggleNav))
            .slot(
                TitleBarSlot::RightHeader,
                Button::new()
                    .on_click(context.message(ManagerMsg::ShowRings))
                    .content("Show rings"),
            );
        let nav = NavigationView::new()
            .pane_display_mode(NavigationViewPaneDisplayMode::Left)
            .open_pane_length(240.0)
            .pane_title("Usage Rings")
            .is_pane_open(self.nav_open)
            .is_pane_toggle_button_visible(false)
            .is_settings_visible(false)
            .is_back_button_visible(NavigationViewBackButtonVisible::Collapsed)
            .on_selected_tag_changed(context.callback(|tag: Option<String>| match tag.and_then(|t| Page::parse(&t)) {
                Some(p) => ManagerMsg::Select(p),
                None => ManagerMsg::Ignore,
            }))
            .grid_row(1)
            .slots([
                SlotView::collection(NavigationViewSlot::MenuItems, items),
                SlotView::new(NavigationViewSlot::Content, content),
            ]);
        Grid::new().rows([GridLength::Auto, GridLength::STAR]).children((title_bar, nav))
    }
}


/// Solid card colour per theme. Cards are slightly lighter than the page; the canvas rings are
/// cleared to this exact value so they blend in.
fn card_gray(dark: bool) -> u8 {
    if dark { 0x2B } else { 0xFB }
}

/// A gallery-style card: rounded, hairline border, raised a step from the page.
fn card(dark: bool, child: impl Into<View>) -> View {
    let g = card_gray(dark);
    Border::new()
        .background(Color::rgb(g, g, g))
        .border_brush(ThemeBrush::CardStroke)
        .border_thickness(Thickness::uniform(1.0))
        .corner_radius(CornerRadius::uniform(8.0))
        .padding(Thickness::uniform(16.0))
        .content(child)
}

/// Secondary text (paths, hints, descriptions).
fn dim(text: impl Into<String>) -> TextBlock {
    TextBlock::new()
        .text(text)
        .font_size(12.0)
        .foreground(Color::rgb(0x8A, 0x8A, 0x8A))
        .text_wrapping(TextWrapping::WrapWholeWords)
}

/// Card title.
fn title(text: impl Into<String>) -> TextBlock {
    TextBlock::new().text(text).font_size(16.0).font_weight(FontWeight::SEMI_BOLD)
}

/// An inline error line that takes no room while empty.
fn error_note(text: String) -> View {
    if text.is_empty() {
        Grid::new().height(0.0).into()
    } else {
        TextBlock::new()
            .text(text)
            .foreground(ThemeBrush::SystemCritical)
            .text_wrapping(TextWrapping::WrapWholeWords)
            .into()
    }
}

/// A small rounded label such as `stdio` / `http`.
fn badge(text: &str) -> View {
    Border::new()
        .background(Color::argb(48, 128, 128, 128))
        .corner_radius(CornerRadius::uniform(4.0))
        .padding(Thickness::xy(8.0, 2.0))
        .content(dim(text))
}

/// A card for an empty list: what is missing and what to do about it.
fn empty_card(dark: bool, headline: &str, hint: &str) -> View {
    card(
        dark,
        StackPanel::new().spacing(4.0).children((TextBlock::new().text(headline).font_size(16.0), dim(hint))),
    )
}

// ---------------------------------------------------------------- Home (dashboard)

/// Today's local date.
fn today() -> manager::Ymd {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let t = unsafe { GetLocalTime() };
    (t.wYear as i32, t.wMonth as u32, t.wDay as u32)
}

/// Distinct, theme-neutral slice colours; cycles for more than eight accounts.
fn palette(i: usize) -> (u8, u8, u8) {
    const P: [(u8, u8, u8); 8] = [
        (0x4C, 0xA3, 0xFF),
        (0xF5, 0x9E, 0x0B),
        (0x34, 0xD3, 0x99),
        (0xE8, 0x6C, 0xA8),
        (0x9B, 0x8A, 0xFB),
        (0xF4, 0x72, 0x5B),
        (0x2D, 0xD4, 0xD4),
        (0xB5, 0xD3, 0x3D),
    ];
    P[i % P.len()]
}

/// What the dashboard shows, worked out once from the profile cards so it can be tested.
struct Dashboard {
    accounts: usize,
    with_reading: usize,
    /// Highest weekly use: name and percent.
    top: Option<(String, f32)>,
    /// Weekly usage as shares of the combined total, one per account (same order as the cards).
    shares: Vec<f32>,
    /// Average weekly use over accounts that have a reading.
    average: Option<f32>,
    /// Soonest first: (days until, date text, account).
    resets: Vec<(i64, String, String)>,
    /// Accounts whose usage could not be read.
    problems: Vec<String>,
}

impl Dashboard {
    fn build(profiles: &[ProfileCard], today: manager::Ymd) -> Self {
        let weekly: Vec<Option<f32>> = profiles.iter().map(|p| p.weekly_used).collect();
        let readings: Vec<(String, f32)> =
            profiles.iter().filter_map(|p| p.weekly_used.map(|u| (p.name.clone(), u))).collect();
        let top = readings.iter().cloned().max_by(|a, b| a.1.total_cmp(&b.1));
        let average = (!readings.is_empty()).then(|| readings.iter().map(|r| r.1).sum::<f32>() / readings.len() as f32);
        let mut resets: Vec<(i64, String, String)> = profiles
            .iter()
            .filter_map(|p| {
                let text = p.weekly_reset.as_ref()?;
                let (_, days) = manager::upcoming_reset(text, today)?;
                Some((days, text.clone(), p.name.clone()))
            })
            .collect();
        resets.sort();
        let problems = profiles
            .iter()
            .filter_map(|p| p.error.as_ref().map(|e| format!("{}: {e}", p.name)))
            .collect();
        Self {
            accounts: profiles.len(),
            with_reading: readings.len(),
            top,
            shares: manager::usage_shares(&weekly),
            average,
            resets,
            problems,
        }
    }
}

fn in_days(days: i64) -> String {
    match days {
        0 => "today".into(),
        1 => "tomorrow".into(),
        n => format!("in {n} days"),
    }
}

/// Points along an arc between two fractions of a turn (0 = 12 o'clock, clockwise).
fn arc_between(cx: f32, cy: f32, r: f32, from: f32, to: f32) -> Vec<(f32, f32)> {
    let steps = (((to - from).abs() * 120.0).ceil() as usize).max(1);
    (0..=steps)
        .map(|i| {
            let f = from + (to - from) * i as f32 / steps as f32;
            let a = -std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU * f;
            (cx + r * a.cos(), cy + r * a.sin())
        })
        .collect()
}

/// A donut of per-account shares. With no usage at all it is just the empty track.
fn donut(shares: Vec<f32>, dark: bool) -> View {
    const SIZE: f64 = 170.0;
    const STROKE: f32 = 18.0;
    let canvas = windows_canvas::canvas(move |dc| {
        use windows_canvas::{CapStyle, ColorF, Ellipse, PathBuilder, StrokeStyleBuilder, Vector2};
        let (cx, cy) = (dc.width / 2.0, dc.height / 2.0);
        let r = (dc.width.min(dc.height) - STROKE) / 2.0;
        let bg = card_gray(dark) as f32 / 255.0;
        dc.clear(ColorF::new(bg, bg, bg, 1.0));
        let track = dc.create_solid_brush(ColorF::new(0.5, 0.5, 0.5, 0.25))?;
        dc.draw_ellipse(&Ellipse::new(Vector2::new(cx, cy), r, r), &track, STROKE);
        let style = dc.device().create_stroke_style(&StrokeStyleBuilder::new().caps(CapStyle::Flat))?;
        let mut start = 0.0f32;
        for (i, share) in shares.iter().enumerate().filter(|(_, s)| **s > 0.0) {
            // A full-circle slice would have identical ends; stop a hair short.
            let end = (start + share).min(start + 0.9995);
            let (red, green, blue) = palette(i);
            let brush = dc.create_solid_brush(ColorF::new(red as f32 / 255.0, green as f32 / 255.0, blue as f32 / 255.0, 1.0))?;
            let pts = arc_between(cx, cy, r, start, end);
            let mut figure = PathBuilder::new(dc.device())?.begin_hollow(Vector2::new(pts[0].0, pts[0].1));
            for p in &pts[1..] {
                figure = figure.line_to(Vector2::new(p.0, p.1));
            }
            let path = figure.end_open().build()?;
            dc.draw_path_styled(&path, &brush, STROKE, &style);
            start += share;
        }
        Ok(())
    });
    Grid::new().width(SIZE).height(SIZE).children((canvas,))
}


struct HomePage {
    profiles: Vec<ProfileCard>,
    today: manager::Ymd,
}

impl Component for HomePage {
    type Input = bool; // dark theme
    type Message = ();

    fn create(_: &bool, _: &ComponentContext<Self>) -> Self {
        Self { profiles: manager::snapshot().profiles, today: today() }
    }

    fn view(&self, dark: &bool, _: &mut ViewContext<Self>) -> View {
        let dark = *dark;
        let d = Dashboard::build(&self.profiles, self.today);

        let stat = |col: i32, label: &str, value: String, sub: String| -> View {
            StackPanel::new()
                .grid_column(col)
                .children((card(
                    dark,
                    StackPanel::new().spacing(2.0).children((
                        dim(label),
                        TextBlock::new().text(value).font_size(26.0).font_weight(FontWeight::SEMI_BOLD),
                        dim(sub),
                    )),
                ),))
                .into()
        };
        let next = d.resets.first();
        let stats = Grid::new()
            .columns([GridLength::STAR, GridLength::STAR, GridLength::STAR])
            .column_spacing(8.0)
            .children((
                stat(
                    0,
                    "Accounts",
                    d.accounts.to_string(),
                    format!("{} with a usage reading", d.with_reading),
                ),
                stat(
                    1,
                    "Highest weekly use",
                    d.top.as_ref().map(|t| format!("{:.0}%", t.1)).unwrap_or_else(|| "\u{2014}".into()),
                    d.top.as_ref().map(|t| t.0.clone()).unwrap_or_else(|| "no readings yet".into()),
                ),
                stat(
                    2,
                    "Next weekly reset",
                    next.map(|r| r.1.clone()).unwrap_or_else(|| "\u{2014}".into()),
                    next.map(|r| format!("{} \u{b7} {}", r.2, in_days(r.0))).unwrap_or_else(|| "no reset dates yet".into()),
                ),
            ));

        // Donut + legend: each account's share of the combined weekly usage.
        let legend: Vec<KeyedView> = self
            .profiles
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let (r, g, b) = palette(i);
                let dot = Border::new()
                    .width(10.0)
                    .height(10.0)
                    .corner_radius(CornerRadius::uniform(5.0))
                    .background(Color::rgb(r, g, b))
                    .vertical_alignment(VerticalAlignment::Center)
                    .content(Grid::new());
                let row = StackPanel::new().spacing(10.0).orientation(Orientation::Horizontal).children((
                    dot,
                    TextBlock::new().text(p.name.clone()),
                    dim(format!("Week {}  \u{b7}  Session {}", percent_text(p.weekly_used), percent_text(p.session_used))),
                ));
                KeyedView::new(p.name.clone(), row)
            })
            .collect();
        let centre = TextBlock::new()
            .text(d.average.map(|a| format!("avg {a:.0}%")).unwrap_or_else(|| "\u{2014}".into()))
            .horizontal_alignment(HorizontalAlignment::Center)
            .vertical_alignment(VerticalAlignment::Center);
        let chart = Grid::new().width(170.0).height(170.0).children((donut(d.shares.clone(), dark), centre));
        let usage_card = card(
            dark,
            StackPanel::new().spacing(12.0).children((
                title("Weekly usage by account"),
                StackPanel::new().spacing(24.0).orientation(Orientation::Horizontal).children((
                    chart,
                    StackPanel::new()
                        .spacing(8.0)
                        .vertical_alignment(VerticalAlignment::Center)
                        .keyed_children(legend),
                )),
                dim("Each slice is that account's share of the combined weekly usage."),
            )),
        );

        let reset_rows: Vec<KeyedView> = d
            .resets
            .iter()
            .map(|(days, date, name)| {
                KeyedView::new(
                    name.clone(),
                    StackPanel::new().spacing(12.0).orientation(Orientation::Horizontal).children((
                        TextBlock::new().text(date.clone()).font_weight(FontWeight::SEMI_BOLD),
                        TextBlock::new().text(name.clone()),
                        dim(in_days(*days)),
                    )),
                )
            })
            .collect();
        let resets_body: View = if reset_rows.is_empty() {
            dim("Reset dates appear after the first usage reading.").into()
        } else {
            StackPanel::new().spacing(8.0).keyed_children(reset_rows)
        };
        let resets_card = card(dark, StackPanel::new().spacing(12.0).children((title("Upcoming resets"), resets_body)));

        let problems: View = if d.problems.is_empty() {
            Grid::new().height(0.0).into()
        } else {
            let lines: Vec<KeyedView> =
                d.problems.iter().enumerate().map(|(i, p)| KeyedView::new(i.to_string(), error_note(p.clone()))).collect();
            card(dark, StackPanel::new().spacing(8.0).children((title("Needs attention"), StackPanel::new().spacing(4.0).keyed_children(lines))))
        };

        StackPanel::new().spacing(8.0).children((
            stats,
            usage_card,
            resets_card,
            problems,
        ))
    }
}

// ---------------------------------------------------------------- Profiles page

/// A wrapped status/error line that takes no room while it is empty.
fn note(text: String) -> View {
    if text.is_empty() {
        Grid::new().height(0.0).into()
    } else {
        TextBlock::new().text(text).text_wrapping(TextWrapping::Wrap).into()
    }
}

/// Solid page colour per theme (#202020 / #F3F3F3). A canvas cannot be transparent, so the rings
/// are cleared to this exact value to blend in.
fn page_gray(dark: bool) -> u8 {
    if dark { 0x20 } else { 0xF3 }
}

const RING_SIZE: f64 = 36.0;
const RING_STROKE: f32 = 4.0;

/// Points along an arc that starts at 12 o'clock and runs clockwise for `fraction` of a turn.
fn arc_points(cx: f32, cy: f32, r: f32, fraction: f32) -> Vec<(f32, f32)> {
    let fraction = fraction.clamp(0.0, 1.0);
    let steps = ((fraction * 120.0).ceil() as usize).max(1);
    (0..=steps)
        .map(|i| {
            let a = -std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU * fraction * i as f32 / steps as f32;
            (cx + r * a.cos(), cy + r * a.sin())
        })
        .collect()
}

/// A ring showing a real reading, or only the empty track when there is none.
fn ring_meter(used: Option<f32>, dark: bool) -> View {
    let canvas = windows_canvas::canvas(move |dc| {
        let (cx, cy) = (dc.width / 2.0, dc.height / 2.0);
        let r = (dc.width.min(dc.height) - RING_STROKE) / 2.0;
        let bg = card_gray(dark) as f32 / 255.0; // matches the card behind the canvas
        dc.clear(windows_canvas::ColorF::new(bg, bg, bg, 1.0));
        let track = dc.create_solid_brush(windows_canvas::ColorF::new(0.5, 0.5, 0.5, 0.25))?;
        dc.draw_ellipse(&windows_canvas::Ellipse::new(windows_canvas::Vector2::new(cx, cy), r, r), &track, RING_STROKE);
        if let Some(u) = used.map(|u| u.clamp(0.0, 100.0)).filter(|u| *u > 0.0) {
            let (red, green, blue) = manager::level_rgb(u);
            let brush = dc.create_solid_brush(windows_canvas::ColorF::new(red, green, blue, 1.0))?;
            let style = dc.device().create_stroke_style(
                &windows_canvas::StrokeStyleBuilder::new().caps(windows_canvas::CapStyle::Round),
            )?;
            let pts = arc_points(cx, cy, r, u / 100.0);
            let mut figure = windows_canvas::PathBuilder::new(dc.device())?
                .begin_hollow(windows_canvas::Vector2::new(pts[0].0, pts[0].1));
            for p in &pts[1..] {
                figure = figure.line_to(windows_canvas::Vector2::new(p.0, p.1));
            }
            let path = figure.end_open().build()?;
            dc.draw_path_styled(&path, &brush, RING_STROKE, &style);
        }
        Ok(())
    });
    Grid::new().width(RING_SIZE).height(RING_SIZE).children((canvas,))
}

fn percent_text(used: Option<f32>) -> String {
    // A ring shows a real reading or nothing, never a made-up number.
    used.map(|u| format!("{:.0}%", u.clamp(0.0, 100.0))).unwrap_or_else(|| "\u{2014}".into())
}

#[derive(Clone, PartialEq, Debug)]
enum Dialog {
    None,
    New { name: String, servers: Vec<String> },
    ConfirmRemove(String),
    Created(String),
    Edit { original: Option<String>, name: String, command: String, config_dir: String },
}

/// Which field of the edit dialog changed.
#[derive(Clone, Copy)]
enum EditField {
    Name,
    Command,
    ConfigDir,
}

/// Native folder picker; `None` on cancel or error. Runs on the XAML (STA) thread.
fn pick_folder() -> Option<String> {
    use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, FOS_PICKFOLDERS, SIGDN_FILESYSPATH};
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let options = dialog.GetOptions().ok()?;
        dialog.SetOptions(options | FOS_PICKFOLDERS).ok()?;
        dialog.Show(None).ok()?;
        let path = dialog.GetResult().ok()?.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let text = path.to_string().ok();
        CoTaskMemFree(Some(path.0 as *const _));
        text
    }
}

/// Profiles page state and actions, free of UI so tests can drive it directly.
struct ProfilesModel {
    state: ManagerState,
    dialog: Dialog,
    /// Inline error shown in the open dialog, or on the page when no dialog is open.
    error: Option<String>,
}

impl ProfilesModel {
    fn load() -> Self {
        Self { state: manager::snapshot(), dialog: Dialog::None, error: None }
    }

    fn open_add_existing(&mut self) {
        self.error = None;
        self.dialog = Dialog::Edit {
            original: None,
            name: String::new(),
            command: String::new(),
            config_dir: String::new(),
        };
    }

    fn open_new(&mut self) {
        self.error = None;
        self.dialog = Dialog::New { name: String::new(), servers: Vec::new() };
    }

    fn set_name(&mut self, text: String) {
        if let Dialog::New { name, .. } = &mut self.dialog {
            *name = text;
        }
    }

    fn pick_server(&mut self, server: &str, on: bool) {
        if let Dialog::New { servers, .. } = &mut self.dialog {
            servers.retain(|s| s != server);
            if on {
                servers.push(server.to_string());
            }
        }
    }

    /// Creates the profile; failures stay inline in the dialog and nothing else changes.
    fn submit_new(&mut self) {
        let Dialog::New { name, servers } = &self.dialog else { return };
        match manager::new_profile(name, servers) {
            Ok(made) => {
                let mut text = format!("Profile folder: {}\nLauncher: {}", made.dir.display(), made.launcher.display());
                for n in &made.notes {
                    text += &format!("\n{n}");
                }
                self.error = None;
                self.state = manager::snapshot();
                self.dialog = Dialog::Created(text);
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn ask_remove(&mut self, name: String) {
        self.error = None;
        self.dialog = Dialog::ConfirmRemove(name);
    }

    /// Removes the entry from the config only; the profile's files are never touched.
    fn confirm_remove(&mut self) {
        let Dialog::ConfirmRemove(name) = &self.dialog else { return };
        match manager::remove_profile(name) {
            Ok(()) => self.state = manager::snapshot(),
            Err(e) => self.error = Some(e),
        }
        self.dialog = Dialog::None;
    }

    /// Pre-fills from the config entry itself (the card shows the resolved folder, not the setting).
    fn open_edit(&mut self, name: &str) {
        self.error = None;
        if let Some(p) = config::load().profiles.into_iter().find(|p| p.name == name) {
            self.dialog = Dialog::Edit {
                original: Some(p.name.clone()),
                name: p.name,
                command: p.command.unwrap_or_default(),
                config_dir: p.config_dir.unwrap_or_default(),
            };
        } else {
            self.error = Some("unknown profile".into());
        }
    }

    fn set_field(&mut self, field: EditField, text: String) {
        if let Dialog::Edit { name, command, config_dir, .. } = &mut self.dialog {
            match field {
                EditField::Name => *name = text,
                EditField::Command => *command = text,
                EditField::ConfigDir => *config_dir = text,
            }
        }
    }

    fn picked_folder(&mut self, folder: Option<String>) {
        if let Some(f) = folder {
            self.set_field(EditField::ConfigDir, f);
        }
    }

    /// Blank command/folder mean "default"; errors stay inline and nothing is written.
    fn submit_edit(&mut self) {
        let Dialog::Edit { original, name, command, config_dir } = &self.dialog else { return };
        let opt = |s: &str| Some(s.trim().to_string()).filter(|s| !s.is_empty());
        let result = match original {
            Some(o) => manager::edit_profile(o, name.trim(), opt(command), opt(config_dir)),
            None => manager::add_profile(name.trim(), opt(command), opt(config_dir)),
        };
        match result {
            Ok(()) => {
                self.state = manager::snapshot();
                self.dialog = Dialog::None;
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn dismiss(&mut self) {
        self.dialog = Dialog::None;
        self.error = None;
    }
}

#[derive(Clone)]
enum ProfilesMsg {
    OpenNew,
    OpenAdd,
    Name(String),
    PickServer(String, bool),
    Remove(String),
    Edit(String),
    Field(EditField, String),
    Browse,
    /// Result of whichever dialog is open.
    Closed(ContentDialogResult),
}

struct ProfilesPage {
    model: ProfilesModel,
}

impl Component for ProfilesPage {
    type Input = bool; // dark theme
    type Message = ProfilesMsg;

    fn create(_: &bool, _: &ComponentContext<Self>) -> Self {
        Self { model: ProfilesModel::load() }
    }

    fn update(&mut self, message: ProfilesMsg, _: &ComponentContext<Self>) {
        let m = &mut self.model;
        match message {
            ProfilesMsg::OpenNew => m.open_new(),
            ProfilesMsg::OpenAdd => m.open_add_existing(),
            ProfilesMsg::Name(t) => m.set_name(t),
            ProfilesMsg::PickServer(s, on) => m.pick_server(&s, on),
            ProfilesMsg::Remove(n) => m.ask_remove(n),
            ProfilesMsg::Edit(n) => m.open_edit(&n),
            ProfilesMsg::Field(f, t) => m.set_field(f, t),
            ProfilesMsg::Browse => m.picked_folder(pick_folder()),
            ProfilesMsg::Closed(result) => {
                let primary = result == ContentDialogResult::Primary;
                match (&m.dialog, primary) {
                    (Dialog::New { .. }, true) => m.submit_new(),
                    (Dialog::ConfirmRemove(_), true) => m.confirm_remove(),
                    (Dialog::Edit { .. }, true) => m.submit_edit(),
                    _ => m.dismiss(),
                }
            }
        }
    }

    fn view(&self, dark: &bool, context: &mut ViewContext<Self>) -> View {
        let m = &self.model;
        let dark = *dark;
        let cards: Vec<KeyedView> = m
            .state
            .profiles
            .iter()
            .map(|p| {
                let usage = format!(
                    "Session {}  \u{b7}  Week {}{}",
                    percent_text(p.session_used),
                    percent_text(p.weekly_used),
                    p.weekly_reset.as_ref().map(|d| format!(" (resets {d})")).unwrap_or_default()
                );
                let mcps = if p.mcps.is_empty() { "no MCP servers".to_string() } else { p.mcps.join(" \u{b7} ") };
                let rings = StackPanel::new()
                    .spacing(10.0)
                    .orientation(Orientation::Horizontal)
                    .vertical_alignment(VerticalAlignment::Center)
                    .grid_column(0)
                    .children((ring_meter(p.session_used, dark), ring_meter(p.weekly_used, dark)));
                let info = StackPanel::new().spacing(3.0).grid_column(1).children((
                    title(p.name.clone()),
                    dim(p.dir.clone()),
                    TextBlock::new().text(usage).margin(Thickness::new(0.0, 6.0, 0.0, 0.0)),
                    dim(mcps),
                    error_note(p.error.clone().unwrap_or_default()),
                ));
                let actions = StackPanel::new()
                    .spacing(8.0)
                    .orientation(Orientation::Horizontal)
                    .vertical_alignment(VerticalAlignment::Center)
                    .grid_column(2)
                    .children((
                        Button::new().on_click(context.message(ProfilesMsg::Edit(p.name.clone()))).content("Edit"),
                        Button::new().on_click(context.message(ProfilesMsg::Remove(p.name.clone()))).content("Remove"),
                    ));
                let row = Grid::new()
                    .columns([GridLength::Auto, GridLength::STAR, GridLength::Auto])
                    .column_spacing(20.0)
                    .children((rings, info, actions));
                KeyedView::new(p.name.clone(), card(dark, row))
            })
            .collect();

        let list: View = if cards.is_empty() {
            empty_card(dark, "No profiles yet.", "Create a new profile, or add an existing Claude config folder.")
        } else {
            StackPanel::new().spacing(8.0).keyed_children(cards)
        };
        let page_error = if m.dialog == Dialog::None { m.error.clone().unwrap_or_default() } else { String::new() };

        StackPanel::new().spacing(16.0).children((
            StackPanel::new().spacing(8.0).orientation(Orientation::Horizontal).children((
                Button::new()
                    .style(ButtonStyle::Accent)
                    .on_click(context.message(ProfilesMsg::OpenNew))
                    .content("+ New profile"),
                Button::new().on_click(context.message(ProfilesMsg::OpenAdd)).content("Add existing..."),
            )),
            error_note(page_error),
            list,
            self.dialog_view(context),
        ))
    }
}


impl ProfilesPage {
    fn dialog_view(&self, context: &mut ViewContext<Self>) -> View {
        let m = &self.model;
        let on_closed = context.callback(ProfilesMsg::Closed);
        let error = m.error.clone().unwrap_or_default();
        match &m.dialog {
            Dialog::None => ContentDialog::new().is_open(false).title("").content(TextBlock::new()),
            Dialog::New { name, servers } => {
                let boxes: Vec<KeyedView> = m
                    .state
                    .servers
                    .iter()
                    .map(|s| {
                        let n = s.name.clone();
                        KeyedView::new(
                            s.name.clone(),
                            CheckBox::new()
                                .is_checked(servers.contains(&s.name))
                                .on_is_checked_changed(context.callback(move |on| ProfilesMsg::PickServer(n.clone(), on)))
                                .content(s.name.clone()),
                        )
                    })
                    .collect();
                ContentDialog::new()
                    .is_open(true)
                    .title("New profile")
                    .primary_button_text("Create")
                    .close_button_text("Cancel")
                    .on_closed(on_closed)
                    .content(StackPanel::new().spacing(8.0).children((
                        TextBox::new().text(name.clone()).placeholder_text("Name").on_text_changed(context.callback(ProfilesMsg::Name)),
                        StackPanel::new().spacing(4.0).keyed_children(boxes),
                        note(error),
                    )))
            }
            Dialog::Edit { original, name, command, config_dir } => ContentDialog::new()
                .is_open(true)
                .title(if original.is_some() { "Edit profile" } else { "Add existing profile" })
                .primary_button_text("Save")
                .close_button_text("Cancel")
                .on_closed(on_closed)
                .content(StackPanel::new().spacing(8.0).children((
                    TextBox::new()
                        .text(name.clone())
                        .placeholder_text("Name")
                        .on_text_changed(context.callback(|t| ProfilesMsg::Field(EditField::Name, t))),
                    TextBox::new()
                        .text(command.clone())
                        .placeholder_text("Command (optional)")
                        .on_text_changed(context.callback(|t| ProfilesMsg::Field(EditField::Command, t))),
                    TextBox::new()
                        .text(config_dir.clone())
                        .placeholder_text("Config folder (optional)")
                        .on_text_changed(context.callback(|t| ProfilesMsg::Field(EditField::ConfigDir, t))),
                    Button::new().on_click(context.message(ProfilesMsg::Browse)).content("Browse\u{2026}"),
                    note(error),
                ))),
            Dialog::ConfirmRemove(name) => ContentDialog::new()
                .is_open(true)
                .title(format!("Remove \"{name}\"?"))
                .primary_button_text("Remove")
                .close_button_text("Cancel")
                .on_closed(on_closed)
                .content(TextBlock::new().text(
                    "This removes the profile from the widget. Its folder and files are left untouched.",
                )),
            Dialog::Created(text) => ContentDialog::new()
                .is_open(true)
                .title("Profile created")
                .close_button_text("OK")
                .on_closed(on_closed)
                .content(TextBlock::new().text(text.clone())),
        }
    }
}

// ---------------------------------------------------------------- MCP servers page

#[derive(Clone, PartialEq, Debug, Default)]
struct ServerForm {
    /// `None` when adding a new server.
    original: Option<String>,
    name: String,
    /// `stdio` or `http`.
    kind: String,
    command: String,
    args: String,
    env: String,
    url: String,
    headers: String,
}

impl ServerForm {
    fn blank() -> Self {
        Self { kind: "stdio".into(), ..Self::default() }
    }

    /// Rows are pre-filled with the exact strings the parsers read back.
    fn from_server(s: &claude_usage_widget::mcp::Server) -> Self {
        let kv = |m: &std::collections::BTreeMap<String, String>| {
            m.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("\n")
        };
        Self {
            original: Some(s.name.clone()),
            name: s.name.clone(),
            kind: s.kind.clone(),
            command: s.command.clone(),
            args: s.args.join("\n"),
            env: kv(&s.env),
            url: s.url.clone(),
            headers: kv(&s.headers),
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
enum McpDialog {
    None,
    Form(ServerForm),
    ConfirmDelete(String),
}

#[derive(Clone, Copy)]
enum ServerField {
    Name,
    Command,
    Args,
    Env,
    Url,
    Headers,
}

struct McpModel {
    state: ManagerState,
    dialog: McpDialog,
    /// Index into `state.profiles` for the Import dropdown.
    import_from: usize,
    /// Inline error (in the open dialog, otherwise on the page).
    error: Option<String>,
    /// "Imported: a, b" / "Nothing to import".
    notice: Option<String>,
}

impl McpModel {
    fn load() -> Self {
        Self { state: manager::snapshot(), dialog: McpDialog::None, import_from: 0, error: None, notice: None }
    }

    fn reload(&mut self) {
        self.state = manager::snapshot();
        self.import_from = self.import_from.min(self.state.profiles.len().saturating_sub(1));
    }

    /// A checkbox is on only for a readable profile that lists the server.
    fn is_enabled(&self, profile: &ProfileCard, server: &str) -> bool {
        profile.error.is_none() && profile.mcps.iter().any(|m| m == server)
    }

    fn toggle(&mut self, profile: &str, server: &str, on: bool) {
        self.notice = None;
        match manager::toggle(profile, server, on) {
            Ok(()) => self.error = None,
            Err(e) => self.error = Some(e),
        }
        self.reload();
    }

    fn open_add(&mut self) {
        self.error = None;
        self.dialog = McpDialog::Form(ServerForm::blank());
    }

    fn open_edit(&mut self, name: &str) {
        self.error = None;
        match self.state.servers.iter().find(|s| s.name == name) {
            Some(s) => self.dialog = McpDialog::Form(ServerForm::from_server(s)),
            None => self.error = Some("unknown server".into()),
        }
    }

    fn set_kind(&mut self, kind: &str) {
        if let McpDialog::Form(f) = &mut self.dialog {
            f.kind = kind.to_string();
        }
    }

    fn set_field(&mut self, field: ServerField, text: String) {
        if let McpDialog::Form(f) = &mut self.dialog {
            match field {
                ServerField::Name => f.name = text,
                ServerField::Command => f.command = text,
                ServerField::Args => f.args = text,
                ServerField::Env => f.env = text,
                ServerField::Url => f.url = text,
                ServerField::Headers => f.headers = text,
            }
        }
    }

    /// Builds via `manager::build_server` so parsing and carried-over extras stay single-sourced.
    fn submit_form(&mut self) {
        let McpDialog::Form(f) = &self.dialog else { return };
        let result = manager::build_server(&f.name, &f.kind, &f.command, &f.args, &f.env, &f.url, &f.headers)
            .and_then(|server| manager::save_server(f.original.clone(), server));
        match result {
            Ok(()) => {
                self.dialog = McpDialog::None;
                self.error = None;
                self.reload();
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn ask_delete(&mut self, name: String) {
        self.error = None;
        self.dialog = McpDialog::ConfirmDelete(name);
    }

    fn confirm_delete(&mut self) {
        let McpDialog::ConfirmDelete(name) = &self.dialog else { return };
        if let Err(e) = manager::delete_server(name) {
            self.error = Some(e);
        }
        self.dialog = McpDialog::None;
        self.reload();
    }

    fn import(&mut self) {
        let Some(p) = self.state.profiles.get(self.import_from) else {
            self.error = Some("No profiles to import from.".into());
            return;
        };
        match manager::import_from(&p.name.clone()) {
            Ok(added) if added.is_empty() => {
                self.error = None;
                self.notice = Some("Nothing to import".into());
            }
            Ok(added) => {
                self.error = None;
                self.notice = Some(format!("Imported: {}", added.join(", ")));
            }
            Err(e) => {
                self.notice = None;
                self.error = Some(e);
            }
        }
        self.reload();
    }

    fn dismiss(&mut self) {
        self.dialog = McpDialog::None;
        self.error = None;
    }
}

#[derive(Clone)]
enum McpMsg {
    Toggle(String, String, bool),
    Add,
    Edit(String),
    Delete(String),
    Kind(String),
    Field(ServerField, String),
    ImportFrom(Option<usize>),
    Import,
    Closed(ContentDialogResult),
}

struct McpPage {
    model: McpModel,
}

impl Component for McpPage {
    type Input = bool; // dark theme
    type Message = McpMsg;

    fn create(_: &bool, _: &ComponentContext<Self>) -> Self {
        Self { model: McpModel::load() }
    }

    fn update(&mut self, message: McpMsg, _: &ComponentContext<Self>) {
        let m = &mut self.model;
        match message {
            McpMsg::Toggle(p, s, on) => m.toggle(&p, &s, on),
            McpMsg::Add => m.open_add(),
            McpMsg::Edit(n) => m.open_edit(&n),
            McpMsg::Delete(n) => m.ask_delete(n),
            McpMsg::Kind(k) => m.set_kind(&k),
            McpMsg::Field(f, t) => m.set_field(f, t),
            McpMsg::ImportFrom(i) => m.import_from = i.unwrap_or(0),
            McpMsg::Import => m.import(),
            McpMsg::Closed(result) => {
                let primary = result == ContentDialogResult::Primary;
                match (&m.dialog, primary) {
                    (McpDialog::Form(_), true) => m.submit_form(),
                    (McpDialog::ConfirmDelete(_), true) => m.confirm_delete(),
                    _ => m.dismiss(),
                }
            }
        }
    }

    fn view(&self, dark: &bool, context: &mut ViewContext<Self>) -> View {
        let m = &self.model;
        let dark = *dark;
        let cards: Vec<KeyedView> = m
            .state
            .servers
            .iter()
            .map(|s| {
                let summary = if s.kind == "http" {
                    s.url.clone()
                } else {
                    std::iter::once(s.command.as_str()).chain(s.args.iter().map(String::as_str)).collect::<Vec<_>>().join(" ")
                };
                let boxes: Vec<KeyedView> = m
                    .state
                    .profiles
                    .iter()
                    .map(|p| {
                        let (pn, sn) = (p.name.clone(), s.name.clone());
                        let check = CheckBox::new()
                            .is_checked(m.is_enabled(p, &s.name))
                            .is_enabled(p.error.is_none())
                            .on_is_checked_changed(context.callback(move |on| McpMsg::Toggle(pn.clone(), sn.clone(), on)))
                            .content(p.name.clone());
                        let view: View = match &p.error {
                            Some(e) => check.tooltip(e.clone()),
                            None => check,
                        };
                        KeyedView::new(p.name.clone(), view)
                    })
                    .collect();
                // Rows of four so a long profile list wraps instead of running off the card.
                let rows: Vec<KeyedView> = boxes
                    .chunks(4)
                    .enumerate()
                    .map(|(i, c)| {
                        KeyedView::new(format!("row{i}"), StackPanel::new().spacing(4.0).orientation(Orientation::Horizontal).keyed_children(c.to_vec()))
                    })
                    .collect();
                let info = StackPanel::new().spacing(6.0).grid_column(0).children((
                    StackPanel::new().spacing(10.0).orientation(Orientation::Horizontal).children((
                        title(s.name.clone()),
                        badge(&s.kind),
                    )),
                    dim(summary.clone()).tooltip(summary),
                    dim("Enabled for"),
                    StackPanel::new().spacing(2.0).keyed_children(rows),
                ));
                let actions = StackPanel::new()
                    .spacing(8.0)
                    .orientation(Orientation::Horizontal)
                    .vertical_alignment(VerticalAlignment::Top)
                    .grid_column(1)
                    .children((
                        Button::new().on_click(context.message(McpMsg::Edit(s.name.clone()))).content("Edit"),
                        Button::new().on_click(context.message(McpMsg::Delete(s.name.clone()))).content("Delete"),
                    ));
                let row = Grid::new()
                    .columns([GridLength::STAR, GridLength::Auto])
                    .column_spacing(20.0)
                    .children((info, actions));
                KeyedView::new(s.name.clone(), card(dark, row))
            })
            .collect();
        let list: View = if cards.is_empty() {
            empty_card(dark, "No MCP servers yet.", "Add a server, or import the ones a profile already has.")
        } else {
            StackPanel::new().spacing(8.0).keyed_children(cards)
        };
        let warnings: Vec<KeyedView> = m
            .state
            .profiles
            .iter()
            .filter_map(|p| p.error.as_ref().map(|e| KeyedView::new(p.name.clone(), error_note(format!("{}: {e}", p.name)))))
            .collect();
        let names: Vec<String> = m.state.profiles.iter().map(|p| p.name.clone()).collect();
        let page_error = if m.dialog == McpDialog::None { m.error.clone().unwrap_or_default() } else { String::new() };

        StackPanel::new().spacing(16.0).children((
            StackPanel::new().spacing(8.0).orientation(Orientation::Horizontal).children((
                Button::new()
                    .style(ButtonStyle::Accent)
                    .on_click(context.message(McpMsg::Add))
                    .content("+ Add server"),
                dim("Import from").vertical_alignment(VerticalAlignment::Center).margin(Thickness::new(16.0, 0.0, 0.0, 0.0)),
                ComboBox::new()
                    .items_source(names)
                    .selected_index(if m.state.profiles.is_empty() { None } else { Some(m.import_from) })
                    .on_selection_changed(context.callback(McpMsg::ImportFrom)),
                Button::new().on_click(context.message(McpMsg::Import)).content("Import"),
            )),
            note(m.notice.clone().unwrap_or_default()),
            error_note(page_error),
            list,
            StackPanel::new().spacing(4.0).keyed_children(warnings),
            self.dialog_view(context),
        ))
    }
}


impl McpPage {
    fn dialog_view(&self, context: &mut ViewContext<Self>) -> View {
        let m = &self.model;
        let on_closed = context.callback(McpMsg::Closed);
        let error = m.error.clone().unwrap_or_default();
        match &m.dialog {
            McpDialog::None => ContentDialog::new().is_open(false).title("").content(TextBlock::new()),
            McpDialog::ConfirmDelete(name) => ContentDialog::new()
                .is_open(true)
                .title(format!("Delete \"{name}\"?"))
                .primary_button_text("Delete")
                .close_button_text("Cancel")
                .on_closed(on_closed)
                .content(TextBlock::new().text("Delete this server and remove it from every profile?")),
            McpDialog::Form(f) => {
                let text = |value: &str, placeholder: &str, field: ServerField| {
                    TextBox::new()
                        .text(value.to_string())
                        .placeholder_text(placeholder)
                        .accepts_return(true)
                        .on_text_changed(context.callback(move |t| McpMsg::Field(field, t)))
                };
                let fields: View = if f.kind == "http" {
                    StackPanel::new().spacing(8.0).children((
                        text(&f.url, "URL", ServerField::Url),
                        text(&f.headers, "Headers (one Name=Value per line)", ServerField::Headers),
                    ))
                } else {
                    StackPanel::new().spacing(8.0).children((
                        text(&f.command, "Command", ServerField::Command),
                        text(&f.args, "Arguments (one per line)", ServerField::Args),
                        text(&f.env, "Environment (one NAME=value per line)", ServerField::Env),
                    ))
                };
                ContentDialog::new()
                    .is_open(true)
                    .title(if f.original.is_some() { "Edit server" } else { "Add server" })
                    .primary_button_text("Save")
                    .close_button_text("Cancel")
                    .on_closed(on_closed)
                    .content(StackPanel::new().spacing(8.0).children((
                        text(&f.name, "Name", ServerField::Name),
                        StackPanel::new().spacing(8.0).orientation(Orientation::Horizontal).children((
                            Button::new().on_click(context.message(McpMsg::Kind("stdio".into()))).content("stdio"),
                            Button::new().on_click(context.message(McpMsg::Kind("http".into()))).content("http"),
                            TextBlock::new().text(format!("Type: {}", f.kind)),
                        )),
                        fields,
                        note(error),
                    )))
            }
        }
    }
}

// ---------------------------------------------------------------- Settings page

/// A valid refresh interval typed but not yet saved. Leaving the page saves it; this covers
/// closing the window, where the page is never dropped on the way out.
static PENDING_REFRESH: std::sync::Mutex<Option<u64>> = std::sync::Mutex::new(None);

fn flush_pending_refresh() {
    let pending = PENDING_REFRESH.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(n) = pending {
        let _ = manager::save_settings(n);
    }
}

struct SettingsModel {
    /// Text of the refresh box.
    seconds: String,
    /// The value already stored, so leaving the page only writes (and refreshes the widget) when
    /// the text really changed.
    committed: String,
    error: Option<String>,
    saved: bool,
    theme: Theme,
    update: UpdateState,
}

impl SettingsModel {
    fn load() -> Self {
        let cfg = config::load();
        let seconds = cfg.refresh_seconds.to_string();
        Self {
            committed: seconds.clone(),
            seconds,
            error: None,
            saved: false,
            theme: Theme::parse(&cfg.theme),
            update: UpdateState::Idle,
        }
    }

    /// Typing only validates: the hint appears at once, nothing is written until the user leaves.
    fn set_seconds(&mut self, text: String) {
        self.seconds = text;
        self.saved = false;
        self.error = match self.seconds.trim().parse::<u64>() {
            Err(_) => Some("Enter a number of seconds.".into()),
            Ok(n) => config::validate_refresh(n).err(),
        };
        // Remember a valid, changed value so closing the window can still save it.
        let pending = match (self.error.is_none(), self.seconds.trim().parse::<u64>()) {
            (true, Ok(n)) if self.seconds.trim() != self.committed => Some(n),
            _ => None,
        };
        *PENDING_REFRESH.lock().unwrap_or_else(|e| e.into_inner()) = pending;
    }

    /// Parse failures and `save_settings` errors stay inline; nothing is written on error.
    fn save(&mut self) {
        self.saved = false;
        match self.seconds.trim().parse::<u64>() {
            Err(_) => self.error = Some("Enter a number of seconds.".into()),
            Ok(n) => match manager::save_settings(n) {
                Ok(()) => {
                    self.error = None;
                    self.saved = true;
                    self.committed = self.seconds.trim().to_string();
                    *PENDING_REFRESH.lock().unwrap_or_else(|e| e.into_inner()) = None;
                }
                Err(e) => self.error = Some(e),
            },
        }
    }

    /// Called when the user leaves the page or closes the window. An invalid entry is dropped
    /// (the stored value stays), an unchanged one is not rewritten.
    fn commit_on_leave(&mut self) {
        if self.seconds.trim() != self.committed {
            self.save();
        }
    }
}

/// Where the "Check for updates" card is. The network work happens in background tasks; these
/// transitions are plain so they can be tested without a network.
#[derive(Clone, PartialEq, Debug)]
enum UpdateState {
    Idle,
    Checking,
    UpToDate,
    Available(update::Release),
    Installing,
    Failed(String),
}

impl SettingsModel {
    fn busy(&self) -> bool {
        matches!(self.update, UpdateState::Checking | UpdateState::Installing)
    }

    /// Returns whether a check should start (not while one is running or installing).
    fn start_check(&mut self) -> bool {
        if self.busy() {
            return false;
        }
        self.update = UpdateState::Checking;
        true
    }

    fn checked(&mut self, result: Result<update::Release, String>, current: &str) {
        self.update = match result {
            Err(e) => UpdateState::Failed(e),
            Ok(r) if update::is_newer(&r.tag, current) => UpdateState::Available(r),
            Ok(_) => UpdateState::UpToDate,
        };
    }

    /// Only an available update can be installed; returns it so the caller can start the download.
    fn start_install(&mut self) -> Option<update::Release> {
        match std::mem::replace(&mut self.update, UpdateState::Installing) {
            UpdateState::Available(r) => Some(r),
            other => {
                self.update = other;
                None
            }
        }
    }

    /// Ok means the helper took over and the app is about to exit; an error comes back inline.
    fn installed(&mut self, result: Result<(), String>) -> bool {
        match result {
            Ok(()) => true,
            Err(e) => {
                self.update = UpdateState::Failed(e);
                false
            }
        }
    }
}

#[derive(Clone)]
enum SettingsMsg {
    Seconds(String),
    Theme(Theme),
    CheckUpdates,
    UpdateChecked(Result<update::Release, String>),
    InstallUpdate,
    Installed(Result<(), String>),
}

/// What the page needs from the window: a way to tell it the theme changed.
#[derive(Clone, PartialEq)]
struct SettingsInput {
    on_theme: Callback<Theme>,
    /// The window's current theme, so the page shows the same choice when it changes elsewhere.
    theme: Theme,
    dark: bool,
}

struct SettingsPage {
    model: SettingsModel,
}

/// Leaving the page (or closing the window) saves a changed, valid interval.
impl Drop for SettingsPage {
    fn drop(&mut self) {
        self.model.commit_on_leave();
    }
}

impl SettingsModel {
    /// Saves right away (no Save button for the theme); returns whether it was stored.
    fn choose_theme(&mut self, theme: Theme) -> bool {
        self.saved = false;
        match manager::save_theme(theme) {
            Ok(()) => {
                self.theme = theme;
                self.error = None;
                true
            }
            Err(e) => {
                self.error = Some(e);
                false
            }
        }
    }
}

impl Component for SettingsPage {
    type Input = SettingsInput;
    type Message = SettingsMsg;

    fn create(_: &SettingsInput, _: &ComponentContext<Self>) -> Self {
        Self { model: SettingsModel::load() }
    }
    fn input_changed(&mut self, input: &SettingsInput, _: &ComponentContext<Self>) {
        self.model.theme = input.theme;
    }


    fn update(&mut self, message: SettingsMsg, context: &ComponentContext<Self>) {
        match message {
            SettingsMsg::Seconds(t) => self.model.set_seconds(t),
            SettingsMsg::Theme(t) => {
                self.model.choose_theme(t);
            }
            SettingsMsg::CheckUpdates => {
                if self.model.start_check() {
                    context.spawn_background(|_| SettingsMsg::UpdateChecked(update::fetch_latest()));
                }
            }
            SettingsMsg::UpdateChecked(result) => self.model.checked(result, env!("CARGO_PKG_VERSION")),
            SettingsMsg::InstallUpdate => {
                if let Some(release) = self.model.start_install() {
                    context.spawn_background(move |_| {
                        SettingsMsg::Installed(
                            update::download_and_verify(&release).and_then(|ready| update::start_replace_helper(&ready)),
                        )
                    });
                }
            }
            SettingsMsg::Installed(result) => {
                if self.model.installed(result) {
                    // The helper waits for this process to exit, swaps the exes and relaunches.
                    std::process::exit(0);
                }
            }
        }
    }

    fn view(&self, input: &SettingsInput, context: &mut ViewContext<Self>) -> View {
        let m = &self.model;
        let dark = input.dark;
        let status = match (&m.error, m.saved) {
            (Some(_), _) => String::new(),
            (None, true) => "Saved.".to_string(),
            _ => String::new(),
        };
        let setting = |name: &str, hint: &str, extra: View, control: View| -> View {
            let text = StackPanel::new().spacing(3.0).grid_column(0).children((title(name), dim(hint), extra));
            let right = StackPanel::new()
                .spacing(8.0)
                .orientation(Orientation::Horizontal)
                .vertical_alignment(VerticalAlignment::Center)
                .grid_column(1)
                .children((control,));
            card(
                dark,
                Grid::new().columns([GridLength::STAR, GridLength::Auto]).column_spacing(20.0).children((text, right)),
            )
        };

        // Theme: a ComboBox like the gallery's. The window and this page re-theme right away.
        let themes = [Theme::System, Theme::Light, Theme::Dark];
        let on_theme = input.on_theme.clone();
        let sender = context.sender();
        let current = m.theme;
        let theme_box = ComboBox::new()
            .items_source(themes.iter().map(|t| t.label().to_string()).collect::<Vec<_>>())
            .selected_index(themes.iter().position(|t| *t == current))
            .on_selection_changed(Callback::new(move |i: Option<usize>| {
                let Some(t) = i.and_then(|i| themes.get(i).copied()) else { return };
                if t != current && manager::save_theme(t).is_ok() {
                    let _ = on_theme.call(t);
                    sender.send(SettingsMsg::Theme(t));
                }
            }));

        // Updates: status text on the left, actions on the right.
        let busy = m.busy();
        let update_status: View = match &m.update {
            UpdateState::Idle => Grid::new().height(0.0).into(),
            UpdateState::Checking => dim("Checking for updates...").into(),
            UpdateState::UpToDate => TextBlock::new().text("You are up to date.").into(),
            UpdateState::Available(r) => TextBlock::new().text(format!("{} is available.", r.tag)).into(),
            UpdateState::Installing => dim("Downloading and installing. The app will restart.").into(),
            UpdateState::Failed(e) => error_note(format!("Update failed: {e}")),
        };
        let check_button = Button::new()
            .is_enabled(!busy)
            .on_click(context.message(SettingsMsg::CheckUpdates))
            .content("Check for updates");
        let update_controls: View = match &m.update {
            UpdateState::Available(r) => StackPanel::new().spacing(8.0).orientation(Orientation::Horizontal).children((
                Button::new()
                    .style(ButtonStyle::Accent)
                    .is_enabled(!busy)
                    .on_click(context.message(SettingsMsg::InstallUpdate))
                    .content(format!("Update to {} and restart", r.tag)),
                check_button,
            )).into(),
            _ => check_button.into(),
        };
        let refresh_box = StackPanel::new().spacing(8.0).orientation(Orientation::Horizontal).children((
            TextBox::new()
                .text(m.seconds.clone())
                .width(120.0)
                .on_text_changed(context.callback(SettingsMsg::Seconds)),
        ));

        StackPanel::new().spacing(8.0).children((
            setting(
                "Theme",
                "Choose how the manager looks. System follows your Windows app mode.",
                Grid::new().height(0.0).into(),
                theme_box.into(),
            ),
            setting(
                "Refresh interval (seconds)",
                "How often the widget asks Claude Code for usage (minimum 15). Saved when you leave this page.",
                StackPanel::new().children((
                    note(status),
                    error_note(m.error.clone().unwrap_or_default()),
                )).into(),
                refresh_box.into(),
            ),
            setting(
                "Check for updates",
                &format!("Installed version: v{}", env!("CARGO_PKG_VERSION")),
                update_status,
                update_controls,
            ),
        ))
    }
}


/// Shows the rings widget: signal the running one, or start the widget exe next to this one.
/// The widget itself keeps to a single instance, so a click racing a start is harmless.
fn show_rings() {
    if manager::show_widget() {
        return;
    }
    let exe = std::env::current_exe().ok().map(|p| p.with_file_name("claude-usage-widget.exe"));
    if let Some(exe) = exe.filter(|p| p.exists()) {
        let _ = std::process::Command::new(exe).spawn();
    }
}

fn focus_window() {
    unsafe {
        if let Ok(hwnd) = FindWindowW(None, &HSTRING::from(TITLE)) {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

fn fail(text: &str) -> ! {
    unsafe {
        MessageBoxW(None, &HSTRING::from(text), w!("Usage Rings for Claude Code"), MB_OK | MB_ICONERROR);
    }
    std::process::exit(1)
}

/// Centers the manager on the monitor that has the mouse, once, when its window first exists.
/// (The reactor window API has no position setting, so this waits for the window and moves it.)
fn center_when_ready() {
    use windows::Win32::Foundation::{POINT, RECT};
    use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTOPRIMARY};
    use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetWindowRect, SetWindowPos, SWP_NOSIZE, SWP_NOZORDER};
    std::thread::spawn(|| unsafe {
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let Ok(hwnd) = FindWindowW(None, &HSTRING::from(TITLE)) else { continue };
            let mut win = RECT::default();
            if GetWindowRect(hwnd, &mut win).is_err() || win.right <= win.left {
                continue;
            }
            let mut cursor = POINT::default();
            let _ = GetCursorPos(&mut cursor);
            let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTOPRIMARY);
            let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            if !GetMonitorInfoW(monitor, &mut info).as_bool() {
                return;
            }
            let work = info.rcWork;
            let (w, h) = (win.right - win.left, win.bottom - win.top);
            let x = work.left + ((work.right - work.left - w) / 2).max(0);
            let y = work.top + ((work.bottom - work.top - h) / 2).max(0);
            let _ = SetWindowPos(hwnd, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER);
            return;
        }
    });
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let page = Page::parse_or_home(args.windows(2).find(|w| w[0] == "--page").map(|w| w[1].as_str()));

    // Keep the mutex handle alive for the whole process.
    let _mutex = unsafe { CreateMutexW(None, true, w!("Local\\ClaudeUsageManager")) };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        let _ = std::fs::write(nav_file(), page.arg());
        unsafe {
            if let Ok(ev) = CreateEventW(None, false, false, w!("Local\\ClaudeUsageManagerNavigate")) {
                let _ = SetEvent(ev);
            }
        }
        focus_window();
        return;
    }

    center_when_ready();
    let opened = App::run_component::<ManagerRoot>(ManagerInput { first: page });
    // The window is closed and its pages are gone; save a typed interval that was never committed.
    flush_pending_refresh();
    if opened.is_err() {
        fail("Cannot open the manager window. The Windows App Runtime may be missing.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_reactor::test::{Pump, RecordingRuntime};
    use claude_usage_widget::config;

    #[test]
    fn root_mounts_and_sets_title() {
        // Pages read the config, so this must not race the tests that swap APPDATA.
        let (_g, _root) = env("root", &["a"]);
        let mut pump = Pump::new(RecordingRuntime::default());
        pump.mount_view(View::component::<ManagerRoot>(ManagerInput { first: Page::Mcp })).unwrap();
        pump.dispatch_components(16).unwrap();
        let window = pump.window().expect("window node");
        assert_eq!(pump.runtime().window_title(window), Some(TITLE));
    }

    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Fresh APPDATA with two profiles: `a` (has a reading) and `b` (none); returns the temp root.
    fn env(tag: &str, profiles: &[&str]) -> (std::sync::MutexGuard<'static, ()>, PathBuf) {
        let guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("manager-ui-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("APPDATA", root.join("appdata"));
        let cfg = config::Config {
            refresh_seconds: 900,
            theme: "system".into(),
            profiles: profiles
                .iter()
                .map(|n| {
                    let d = root.join(n);
                    std::fs::create_dir_all(&d).unwrap();
                    std::fs::write(d.join(".claude.json"), "{}").unwrap();
                    config::ProfileConfig {
                        name: n.to_string(),
                        command: None,
                        config_dir: Some(d.display().to_string()),
                    }
                })
                .collect(),
        };
        config::save(&cfg).unwrap();
        (guard, root)
    }

    /// Everything the page has told the native layer so far, as searchable text.
    fn render<C: Component>(input: C::Input) -> String {
        let mut pump = Pump::new(RecordingRuntime::default());
        pump.mount_view(View::component::<C>(input)).unwrap();
        pump.dispatch_components(16).unwrap();
        format!("{:?}", pump.runtime().commands())
    }

    /// `render` output is `{:?}` text, so backslashes and quotes appear escaped.
    fn debug_escaped(s: &str) -> String {
        let d = format!("{s:?}");
        d[1..d.len() - 1].to_string()
    }

    #[test]
    fn remove_confirms_then_keeps_files() {
        let (_g, root) = env("remove", &["a", "b"]);
        let mut m = ProfilesModel::load();
        m.ask_remove("a".into());
        assert_eq!(m.dialog, Dialog::ConfirmRemove("a".into()));
        // Cancelling changes nothing.
        m.dismiss();
        assert_eq!(config::load().profiles.len(), 2);
        m.ask_remove("a".into());
        m.confirm_remove();
        assert_eq!(m.dialog, Dialog::None);
        let names: Vec<_> = config::load().profiles.into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["b"]);
        assert_eq!(m.state.profiles.len(), 1);
        assert!(root.join("a").join(".claude.json").exists(), "profile files stay");
        let out = render::<ProfilesPage>(true);
        assert!(!out.contains("\"a\""), "{out}");
    }

    #[test]
    fn edit_saves_prefills_and_keeps_others() {
        let (_g, root) = env("edit", &["a", "b"]);
        let mut m = ProfilesModel::load();
        m.open_edit("a");
        match &m.dialog {
            Dialog::Edit { name, config_dir, command, .. } => {
                assert_eq!(name, "a");
                assert_eq!(config_dir, &root.join("a").display().to_string());
                assert_eq!(command, "");
            }
            other => panic!("{other:?}"),
        }
        m.set_field(EditField::Name, " a2 ".into());
        m.set_field(EditField::Command, "claude-x".into());
        m.picked_folder(None); // cancelled picker changes nothing
        m.picked_folder(Some("D:\\other".into()));
        m.submit_edit();
        assert_eq!(m.dialog, Dialog::None, "{:?}", m.error);
        let cfg = config::load();
        assert_eq!(cfg.profiles[0].name, "a2");
        assert_eq!(cfg.profiles[0].command.as_deref(), Some("claude-x"));
        assert_eq!(cfg.profiles[0].config_dir.as_deref(), Some("D:\\other"));
        assert_eq!(cfg.profiles[1].name, "b");
        assert_eq!(m.state.profiles[0].name, "a2");
    }

    #[test]
    fn add_existing_shows_a_folder_without_creating_files() {
        let (_g, root) = env("addex", &["a"]);
        let mut m = ProfilesModel::load();
        m.open_add_existing();
        m.set_field(EditField::Name, "old".into());
        m.picked_folder(Some(root.join("a").display().to_string()));
        m.submit_edit();
        assert_eq!(m.dialog, Dialog::None, "{:?}", m.error);
        assert_eq!(m.state.profiles.len(), 2);
        m.open_add_existing();
        m.set_field(EditField::Name, "a".into());
        m.submit_edit();
        assert!(m.error.as_deref().unwrap_or("").contains("appears twice"), "dialog={:?} error={:?} cfg={:?}", m.dialog, m.error, config::load().profiles.iter().map(|p| p.name.clone()).collect::<Vec<_>>());
    }

    #[test]
    fn edit_errors_stay_inline_and_write_nothing() {
        let (_g, _root) = env("editerr", &["a", "b"]);
        let mut m = ProfilesModel::load();
        m.open_edit("a");
        m.set_field(EditField::Name, "b".into());
        m.submit_edit();
        assert!(matches!(m.dialog, Dialog::Edit { .. }));
        assert!(m.error.as_deref().unwrap_or("").contains("appears twice"), "{:?}", m.error);
        assert_eq!(config::load().profiles[0].name, "a");
        m.dismiss();
        m.open_edit("nope");
        assert_eq!(m.error.as_deref(), Some("unknown profile"));
    }

    #[test]
    fn new_profile_errors_stay_inline_and_write_nothing() {
        let (_g, _root) = env("newerr", &["a"]);
        let mut m = ProfilesModel::load();
        m.open_new();
        m.set_name("../evil".into());
        m.submit_new();
        assert!(matches!(m.dialog, Dialog::New { .. }), "dialog stays open");
        assert!(m.error.as_deref().unwrap_or("").contains("name may only use"), "{:?}", m.error);
        assert_eq!(config::load().profiles.len(), 1);
        m.dismiss();
        assert!(m.error.is_none());
    }

    #[test]
    fn new_profile_success_shows_paths_and_reloads() {
        let (_g, root) = env("newok", &["a"]);
        // Keep everything inside the temp root, with a stand-in for claude.exe.
        let exe = root.join("claude.exe");
        std::fs::write(&exe, "").unwrap();
        std::env::set_var("CLAUDE_PROFILE_ROOT", root.join("profiles"));
        std::env::set_var("CLAUDE_BIN_DIR", root.join("bin"));
        std::env::set_var("CLAUDE_EXE", &exe);
        let mut m = ProfilesModel::load();
        m.open_new();
        m.set_name("  work ".into());
        m.pick_server("nope", true);
        m.pick_server("nope", false);
        m.submit_new();
        match &m.dialog {
            Dialog::Created(text) => assert!(text.contains("Profile folder:") && text.contains("Launcher:"), "{text}"),
            other => panic!("expected Created, got {other:?} ({:?})", m.error),
        }
        assert!(m.state.profiles.iter().any(|p| p.name == "work"));
    }

    #[test]
    fn dialogs_render() {
        let (_g, _root) = env("dialogs", &["a"]);
        let mut pump = Pump::new(RecordingRuntime::default());
        pump.mount_view(View::component::<ProfilesPage>(true)).unwrap();
        pump.dispatch_components(16).unwrap();
        let out = format!("{:?}", pump.runtime().commands());
        assert!(out.contains("+ New profile") && out.contains("Remove"), "{out}");
    }

    fn add_server(m: &mut McpModel, name: &str, command: &str, args: &str, env: &str) {
        m.open_add();
        m.set_field(ServerField::Name, name.into());
        m.set_field(ServerField::Command, command.into());
        m.set_field(ServerField::Args, args.into());
        m.set_field(ServerField::Env, env.into());
        m.submit_form();
    }

    fn claude_json(root: &PathBuf, profile: &str) -> String {
        std::fs::read_to_string(root.join(profile).join(".claude.json")).unwrap()
    }

    #[test]
    fn mcp_add_toggle_edit_delete_round_trip() {
        let (_g, root) = env("mcpflow", &["a", "b"]);
        let mut m = McpModel::load();
        add_server(&mut m, "s", "npx", "-y\n@x/y", "K=v");
        assert_eq!(m.dialog, McpDialog::None, "{:?}", m.error);
        assert_eq!(m.state.servers.len(), 1);
        assert!(!m.is_enabled(&m.state.profiles[0], "s"));

        m.toggle("a", "s", true);
        assert!(m.is_enabled(&m.state.profiles[0], "s") && !m.is_enabled(&m.state.profiles[1], "s"));
        assert!(claude_json(&root, "a").contains("\"s\""));
        assert!(!claude_json(&root, "b").contains("\"s\""));

        // Rename keeps the server enabled where it was; the form pre-fills with what the parsers read.
        m.open_edit("s");
        match &m.dialog {
            McpDialog::Form(f) => assert_eq!((f.args.as_str(), f.env.as_str()), ("-y\n@x/y", "K=v")),
            other => panic!("{other:?}"),
        }
        m.set_field(ServerField::Name, "s2".into());
        m.submit_form();
        assert_eq!(m.dialog, McpDialog::None, "{:?}", m.error);
        assert!(m.is_enabled(&m.state.profiles[0], "s2"));
        assert!(!claude_json(&root, "a").contains("\"s\""));

        m.toggle("a", "s2", false);
        assert!(!claude_json(&root, "a").contains("s2"));

        m.ask_delete("s2".into());
        m.dismiss();
        assert_eq!(m.state.servers.len(), 1, "cancel keeps it");
        m.ask_delete("s2".into());
        m.confirm_delete();
        assert!(m.state.servers.is_empty());
    }

    #[test]
    fn mcp_form_errors_stay_inline() {
        let (_g, _root) = env("mcperr", &["a"]);
        let mut m = McpModel::load();
        add_server(&mut m, "", "npx", "", "");
        assert!(matches!(m.dialog, McpDialog::Form(_)));
        assert!(m.error.is_some());
        m.set_field(ServerField::Name, "ok".into());
        m.set_field(ServerField::Env, "no-equals-sign".into());
        m.submit_form();
        assert!(matches!(m.dialog, McpDialog::Form(_)), "bad env line keeps the dialog open");
        assert!(m.error.is_some());
        assert!(manager::snapshot().servers.is_empty(), "nothing written");
    }

    #[test]
    fn mcp_broken_profile_is_disabled_and_untouched() {
        let (_g, root) = env("mcpbroken", &["a", "b"]);
        let mut m = McpModel::load();
        add_server(&mut m, "s", "npx", "", "");
        std::fs::write(root.join("a").join(".claude.json"), "{ not json").unwrap();
        m.reload();
        assert!(m.state.profiles[0].error.is_some());
        assert!(!m.is_enabled(&m.state.profiles[0], "s"));
        m.toggle("a", "s", true);
        assert!(m.error.is_some(), "the read error surfaces inline");
        assert_eq!(claude_json(&root, "a"), "{ not json", "file untouched");
        let out = render::<McpPage>(true);
        assert!(out.contains("a: "), "warning card for the broken profile: {out}");
    }

    #[test]
    fn mcp_import_reports_names_then_nothing() {
        let (_g, root) = env("mcpimport", &["a"]);
        std::fs::write(
            root.join("a").join(".claude.json"),
            r#"{"mcpServers":{"imp":{"type":"stdio","command":"npx","args":[]}}}"#,
        )
        .unwrap();
        let mut m = McpModel::load();
        m.import();
        assert_eq!(m.notice.as_deref(), Some("Imported: imp"), "{:?}", m.error);
        assert!(m.state.servers.iter().any(|s| s.name == "imp"));
        m.import();
        assert_eq!(m.notice.as_deref(), Some("Nothing to import"));
    }

    #[test]
    fn mcp_page_renders_matrix_and_empty_state() {
        let (_g, _root) = env("mcprender", &["a", "b"]);
        assert!(render::<McpPage>(true).contains("No MCP servers yet."));
        let mut m = McpModel::load();
        add_server(&mut m, "srv", "npx", "-y", "");
        let out = render::<McpPage>(true);
        assert!(out.contains("Str(\"srv\")") && out.contains("Str(\"stdio\")") && out.contains("npx -y"), "title, kind badge and summary: {}", out.len());
        assert!(out.contains("+ Add server") && out.contains("Import"), "{out}");
    }

    #[test]
    fn settings_prefill_validate_and_save() {
        let (_g, _root) = env("settings", &["a"]);
        let mut m = SettingsModel::load();
        assert_eq!(m.seconds, "900");
        m.set_seconds("abc".into());
        m.save();
        assert_eq!(m.error.as_deref(), Some("Enter a number of seconds."));
        m.set_seconds("5".into());
        m.save();
        assert!(m.error.as_deref().unwrap_or("").contains("15"), "{:?}", m.error);
        assert!(!m.saved);
        assert_eq!(config::load().refresh_seconds, 900, "nothing persisted on error");
        m.set_seconds(" 42 ".into());
        m.save();
        assert!(m.saved && m.error.is_none());
        assert_eq!(config::load().refresh_seconds, 42);
        assert!(render::<SettingsPage>(SettingsInput { on_theme: Callback::new(|_| {}), theme: Theme::System, dark: false }).contains("Refresh interval (seconds)"));
    }

    #[test]
    fn theme_defaults_to_system_and_choice_is_saved() {
        let (_g, _root) = env("themeui", &["a"]);
        let mut m = SettingsModel::load();
        assert_eq!(m.theme, Theme::System, "default is system");
        assert!(m.choose_theme(Theme::Dark));
        assert_eq!(m.theme, Theme::Dark);
        assert_eq!(config::load().theme, "dark");
        assert_eq!(SettingsModel::load().theme, Theme::Dark, "reopening shows the saved choice");
        assert!(m.choose_theme(Theme::System));
        assert_eq!(config::load().theme, "system");
        assert!(render::<SettingsPage>(SettingsInput { on_theme: Callback::new(|_| {}), theme: Theme::System, dark: false }).contains("Theme"));
    }

    #[test]
    fn window_follows_the_saved_theme_and_system_setting() {
        let (_g, _root) = env("themewin", &["a"]);
        // System follows Windows; an explicit choice wins over it.
        let root = |theme, system_dark| ManagerRoot {
            theme,
            system_dark,
            nav_open: true,
            programmatic_at: None,
            icon: None,
            page: Page::Profiles,
        };
        assert!(root(Theme::System, true).dark() && !root(Theme::System, false).dark());
        assert!(root(Theme::Dark, false).dark() && !root(Theme::Light, true).dark());
    }

    fn release(tag: &str) -> update::Release {
        update::Release { tag: tag.into(), zip_url: "https://x.test/a.zip".into(), sha_url: "https://x.test/a.sha256".into() }
    }

    #[test]
    fn closing_the_window_saves_a_typed_interval() {
        let (_g, _root) = env("closesave", &["a"]);
        *PENDING_REFRESH.lock().unwrap_or_else(|e| e.into_inner()) = None; // nothing left by other tests
        let mut m = SettingsModel::load();
        m.set_seconds("7".into());
        flush_pending_refresh();
        assert_eq!(config::load().refresh_seconds, 900, "invalid input is never written");
        m.set_seconds("1500".into());
        assert_eq!(config::load().refresh_seconds, 900, "typing alone does not write");
        flush_pending_refresh();
        assert_eq!(config::load().refresh_seconds, 1500, "closing the window saves it");
        // Typing the stored value again, or saving explicitly, leaves nothing pending.
        let mut m = SettingsModel::load();
        m.set_seconds("1500".into());
        m.set_seconds("2000".into());
        m.save();
        flush_pending_refresh();
        assert_eq!(config::load().refresh_seconds, 2000);
    }

    #[test]
    fn update_flow_states() {
        let (_g, _root) = env("updates", &["a"]);
        let mut m = SettingsModel::load();
        assert_eq!(m.update, UpdateState::Idle);
        assert!(m.start_install().is_none(), "nothing to install before a check");
        assert_eq!(m.update, UpdateState::Idle, "refused install leaves the state alone");

        assert!(m.start_check());
        assert!(!m.start_check(), "no second check while one runs");
        assert!(m.busy());
        m.checked(Ok(release("v0.5.1")), "0.5.1");
        assert_eq!(m.update, UpdateState::UpToDate, "same version is up to date");
        m.checked(Ok(release("v0.4.0")), "0.5.1");
        assert_eq!(m.update, UpdateState::UpToDate, "an older release is not an update");

        assert!(m.start_check());
        m.checked(Ok(release("v0.6.0")), "0.5.1");
        assert_eq!(m.update, UpdateState::Available(release("v0.6.0")));
        let r = m.start_install().expect("available update installs");
        assert_eq!(r.tag, "v0.6.0");
        assert_eq!(m.update, UpdateState::Installing);
        assert!(!m.start_check(), "busy while installing");

        // A failed install comes back inline and the user can check again.
        assert!(!m.installed(Err("Checksum mismatch".into())));
        assert_eq!(m.update, UpdateState::Failed("Checksum mismatch".into()));
        assert!(m.start_check());
        m.checked(Err("offline".into()), "0.5.1");
        assert_eq!(m.update, UpdateState::Failed("offline".into()));
        // Ok from the helper means the app is about to exit.
        assert!(m.installed(Ok(())));
    }

    #[test]
    fn settings_page_has_an_updates_card() {
        let (_g, _root) = env("updatecard", &["a"]);
        let out = render::<SettingsPage>(SettingsInput { on_theme: Callback::new(|_| {}), theme: Theme::System, dark: false });
        assert!(out.contains("Check for updates") && out.contains("Installed version"), "{}", out.len());
    }

    #[test]
    fn icon_matches_the_tray_artwork() {
        let px = |rgba: &[u8], size: u32, x: u32, y: u32| {
            let i = ((y * size + x) * 4) as usize;
            [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
        };
        let img = icon_rgba(64);
        assert_eq!(img.len(), 64 * 64 * 4);
        assert_eq!(px(&img, 64, 0, 0)[3], 0, "corner outside the disc is transparent");
        let centre = px(&img, 64, 32, 32);
        assert_eq!(centre[3], 255);
        assert!(centre[0] < 40, "dark disc: {centre:?}");
        // Just right of 12 o'clock on the outer ring (radius 24, x = 34): orange arc.
        let orange = px(&img, 64, 34, 8);
        assert!(orange[0] > 200 && orange[1] > 120 && orange[2] < 100, "{orange:?}");
        // Just right of 12 o'clock on the inner ring (radius 12.8): green arc.
        let green = px(&img, 64, 34, 19);
        assert!(green[1] > 150 && green[0] < 100, "{green:?}");
    }

    #[test]
    fn ico_file_is_well_formed() {
        let ico = ico_bytes(&[16, 32]);
        assert_eq!(&ico[..6], &[0, 0, 1, 0, 2, 0]);
        let entry = |n: usize| {
            let e = &ico[6 + 16 * n..6 + 16 * (n + 1)];
            (e[0], u32::from_le_bytes(e[8..12].try_into().unwrap()), u32::from_le_bytes(e[12..16].try_into().unwrap()))
        };
        let (w0, len0, off0) = entry(0);
        let (w1, len1, off1) = entry(1);
        assert_eq!((w0, w1), (16, 32));
        assert_eq!(off0, 6 + 32);
        assert_eq!(off1, off0 + len0);
        assert_eq!(ico.len() as u32, off1 + len1);
        // 40-byte header + 16x16x4 colour + 16 rows x 4-byte mask rows
        assert_eq!(len0, 40 + 16 * 16 * 4 + 16 * 4);
    }

    #[test]
    fn arcs_start_at_twelve_and_run_clockwise() {
        let pts = arc_points(50.0, 50.0, 10.0, 0.25);
        let (x0, y0) = pts[0];
        assert!((x0 - 50.0).abs() < 1e-3 && (y0 - 40.0).abs() < 1e-3, "starts at the top: {x0},{y0}");
        let (x1, y1) = *pts.last().unwrap();
        assert!((x1 - 60.0).abs() < 1e-3 && (y1 - 50.0).abs() < 1e-3, "quarter turn ends at 3 o'clock: {x1},{y1}");
        assert_eq!(arc_points(0.0, 0.0, 5.0, 2.0).len(), arc_points(0.0, 0.0, 5.0, 1.0).len(), "clamped to a full turn");
    }

    #[test]
    fn profiles_empty_state() {
        let (_g, _root) = env("empty", &[]);
        let out = render::<ProfilesPage>(true);
        assert!(out.contains("No profiles yet."), "{out}");
    }

    #[test]
    fn profiles_show_real_readings_and_empty_rings() {
        let (_g, root) = env("readings", &["a", "b"]);
        manager::write_usage(&[manager::UsageEntry {
            name: "a".into(),
            session_used: Some(42.0),
            weekly_used: Some(10.0),
            weekly_reset: Some("Nov 12".into()),
            error: None,
        }])
        .unwrap();
        let out = render::<ProfilesPage>(true);
        assert!(out.contains("Session 42%"), "{out}");
        assert!(out.contains("(resets Nov 12)"), "{out}");
        // No reading for b: dashes, never an invented percent.
        assert!(out.contains("Session \u{2014}"), "{out}");
        assert!(out.contains("no MCP servers"), "{out}");
        assert!(out.contains(&debug_escaped(&root.join("a").display().to_string())), "{out}");
    }

    #[test]
    fn unreadable_profile_shows_error_card() {
        let (_g, root) = env("broken", &["a"]);
        std::fs::write(root.join("a").join(".claude.json"), "{ not json").unwrap();
        let out = render::<ProfilesPage>(true);
        assert!(out.contains("a"), "{out}");
        let card = manager::snapshot().profiles.remove(0);
        let err = card.error.expect("snapshot reports the read error");
        assert!(out.contains(&debug_escaped(&err)), "{out}");
    }
}
