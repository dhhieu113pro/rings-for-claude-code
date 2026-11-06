#![windows_subsystem = "windows"]
//! Native manager window. One process, one window: a second launch hands its page to the
//! running instance (named mutex + event + `manager-nav.txt`) and exits.
use claude_usage_widget::config;
use claude_usage_widget::manager::Page;
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
        NavigationView::new()
            .pane_display_mode(NavigationViewPaneDisplayMode::Left)
            .is_settings_visible(false)
            .is_back_button_visible(NavigationViewBackButtonVisible::Collapsed)
            .on_selected_tag_changed(context.callback(|tag: Option<String>| {
                ManagerMsg::Select(tag.and_then(|t| Page::parse(&t)).unwrap_or(Page::Profiles))
            }))
            .slots([
                SlotView::collection(NavigationViewSlot::MenuItems, items),
                SlotView::new(
                    NavigationViewSlot::Content,
                    TextBlock::new().text(format!("{} (coming soon)", self.page.label())),
                ),
            ])
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
