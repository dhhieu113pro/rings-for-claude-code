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

#[derive(Clone, PartialEq, Debug)]
enum Dialog {
    None,
    New { name: String, servers: Vec<String> },
    ConfirmRemove(String),
    Created(String),
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

    fn dismiss(&mut self) {
        self.dialog = Dialog::None;
        self.error = None;
    }
}

#[derive(Clone)]
enum ProfilesMsg {
    OpenNew,
    Name(String),
    PickServer(String, bool),
    Remove(String),
    Edit(String),
    /// Result of whichever dialog is open.
    Closed(ContentDialogResult),
}

struct ProfilesPage {
    model: ProfilesModel,
}

impl Component for ProfilesPage {
    type Input = ();
    type Message = ProfilesMsg;

    fn create(_: &(), _: &ComponentContext<Self>) -> Self {
        Self { model: ProfilesModel::load() }
    }

    fn update(&mut self, message: ProfilesMsg, _: &ComponentContext<Self>) {
        let m = &mut self.model;
        match message {
            ProfilesMsg::OpenNew => m.open_new(),
            ProfilesMsg::Name(t) => m.set_name(t),
            ProfilesMsg::PickServer(s, on) => m.pick_server(&s, on),
            ProfilesMsg::Remove(n) => m.ask_remove(n),
            ProfilesMsg::Edit(_) => {} // next step
            ProfilesMsg::Closed(result) => {
                let primary = result == ContentDialogResult::Primary;
                match (&m.dialog, primary) {
                    (Dialog::New { .. }, true) => m.submit_new(),
                    (Dialog::ConfirmRemove(_), true) => m.confirm_remove(),
                    _ => m.dismiss(),
                }
            }
        }
    }

    fn view(&self, _: &(), context: &mut ViewContext<Self>) -> View {
        let m = &self.model;
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

        let list: View = if cards.is_empty() {
            TextBlock::new().text("No profiles yet.").into()
        } else {
            StackPanel::new().spacing(12.0).keyed_children(cards)
        };
        let page_error = if m.dialog == Dialog::None { m.error.clone().unwrap_or_default() } else { String::new() };

        StackPanel::new().spacing(12.0).children((
            Button::new().on_click(context.message(ProfilesMsg::OpenNew)).content("+ New profile"),
            TextBlock::new().text(page_error),
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
                        TextBlock::new().text(error),
                    )))
            }
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
        let out = render::<ProfilesPage>(());
        assert!(!out.contains("\"a\""), "{out}");
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
        pump.mount_view(View::component::<ProfilesPage>(())).unwrap();
        pump.dispatch_components(16).unwrap();
        let out = format!("{:?}", pump.runtime().commands());
        assert!(out.contains("+ New profile") && out.contains("Remove"), "{out}");
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
