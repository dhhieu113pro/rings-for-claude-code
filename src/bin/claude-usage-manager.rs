#![windows_subsystem = "windows"]
//! Native manager window. One process, one window: a second launch hands its page to the
//! running instance (named mutex + event + `manager-nav.txt`) and exits.
use claude_usage_widget::config;
use claude_usage_widget::manager::{self, ManagerState, Page};
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
}

struct ManagerRoot {
    page: Page,
}

/// Blocks (in the background) until another launch signals; yields the requested page.
fn wait_for_navigation(context: &ComponentContext<ManagerRoot>) {
    context.spawn_background(|token| {
        let ev = unsafe { CreateEventW(None, false, false, w!("Local\\ClaudeUsageManagerNavigate")) };
        let Ok(ev) = ev else { return ManagerMsg::Widget(Page::Profiles) };
        loop {
            if token.is_cancelled() {
                return ManagerMsg::Select(Page::Profiles);
            }
            if unsafe { WaitForSingleObject(ev, 500) } == WAIT_OBJECT_0 {
                let page = std::fs::read_to_string(nav_file())
                    .ok()
                    .and_then(|t| Page::parse(t.trim()))
                    .unwrap_or(Page::Profiles);
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
        Self { page: input.first }
    }

    fn update(&mut self, message: ManagerMsg, context: &ComponentContext<Self>) {
        match message {
            ManagerMsg::Select(p) => self.page = p,
            ManagerMsg::Widget(p) => {
                self.page = p;
                wait_for_navigation(context);
                focus_window();
            }
        }
    }

    fn view(&self, _input: &ManagerInput, context: &mut ViewContext<Self>) -> View {
        context.window_title(TITLE);
        let items = [Page::Profiles, Page::Mcp, Page::Settings].map(|p| {
            KeyedView::new(
                p.arg(),
                NavigationViewItem::new()
                    .tag(p.arg())
                    .is_selected(p == self.page)
                    .slot(NavigationViewItemSlot::Content, TextBlock::new().text(p.label())),
            )
        });
        let content: View = match self.page {
            Page::Profiles => View::component::<ProfilesPage>(()),
            other => TextBlock::new().text(format!("{} (coming soon)", other.label())).into(),
        };
        NavigationView::new()
            .pane_display_mode(NavigationViewPaneDisplayMode::Left)
            .is_settings_visible(false)
            .is_back_button_visible(NavigationViewBackButtonVisible::Collapsed)
            .on_selected_tag_changed(context.callback(|tag: Option<String>| {
                ManagerMsg::Select(tag.and_then(|t| Page::parse(&t)).unwrap_or(Page::Profiles))
            }))
            .slots([
                SlotView::collection(NavigationViewSlot::MenuItems, items),
                SlotView::new(NavigationViewSlot::Content, content),
            ])
    }
}


// ---------------------------------------------------------------- Profiles page

fn percent_text(used: Option<f32>) -> String {
    // A ring shows a real reading or nothing, never a made-up number.
    used.map(|u| format!("{:.0}%", u.clamp(0.0, 100.0))).unwrap_or_else(|| "\u{2014}".into())
}

#[derive(Clone)]
enum ProfilesMsg {
    Reload,
    Edit(String),
    Remove(String),
}

struct ProfilesPage {
    state: ManagerState,
}

impl Component for ProfilesPage {
    type Input = ();
    type Message = ProfilesMsg;

    fn create(_: &(), _: &ComponentContext<Self>) -> Self {
        Self { state: manager::snapshot() }
    }

    fn update(&mut self, message: ProfilesMsg, _: &ComponentContext<Self>) {
        match message {
            ProfilesMsg::Reload => self.state = manager::snapshot(),
            // Dialogs arrive with the next step.
            ProfilesMsg::Edit(_) | ProfilesMsg::Remove(_) => {}
        }
    }

    fn view(&self, _: &(), context: &mut ViewContext<Self>) -> View {
        if self.state.profiles.is_empty() {
            return StackPanel::new().spacing(8.0).children((
                TextBlock::new().text("No profiles yet."),
            ));
        }
        let cards: Vec<KeyedView> = self
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
                let card = StackPanel::new().spacing(4.0).children((
                    TextBlock::new().text(p.name.clone()),
                    TextBlock::new().text(p.dir.clone()),
                    TextBlock::new().text(usage),
                    TextBlock::new().text(mcps),
                    TextBlock::new().text(p.error.clone().unwrap_or_default()),
                    StackPanel::new().spacing(8.0).orientation(Orientation::Horizontal).children((
                        Button::new().on_click(context.message(ProfilesMsg::Edit(p.name.clone()))).content("Edit"),
                        Button::new().on_click(context.message(ProfilesMsg::Remove(p.name.clone()))).content("Remove"),
                    )),
                ));
                KeyedView::new(p.name.clone(), card)
            })
            .collect();
        StackPanel::new().spacing(12.0).keyed_children(cards)
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let page = args
        .windows(2)
        .find(|w| w[0] == "--page")
        .and_then(|w| Page::parse(&w[1]))
        .unwrap_or(Page::Profiles);

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

    if App::run_component::<ManagerRoot>(ManagerInput { first: page }).is_err() {
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
    fn profiles_empty_state() {
        let (_g, _root) = env("empty", &[]);
        let out = render::<ProfilesPage>(());
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
        let out = render::<ProfilesPage>(());
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
        let out = render::<ProfilesPage>(());
        assert!(out.contains("a"), "{out}");
        let card = manager::snapshot().profiles.remove(0);
        let err = card.error.expect("snapshot reports the read error");
        assert!(out.contains(&debug_escaped(&err)), "{out}");
    }
}
