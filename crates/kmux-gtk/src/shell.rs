//! The native GTK window shell.
//!
//! Replaces the TUI-style four-bar `VBox` (`chrome.rs`) with native libadwaita
//! chrome: an `adw::HeaderBar`, a collapsible sessions sidebar
//! (`adw::OverlaySplitView`), and a pane tab strip (`adw::TabBar`/`TabView`).
//! Only the terminal `DrawingArea` (painted by `render.rs`) looks like a
//! terminal. Every widget here is a *view* of `AppCore`, reconciled by the pump
//! (`header::sync`, `tabs::sync`, `sidebar::sync`).

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk4::{
    Application, Box as GtkBox, Button, DrawingArea, Label, ListBox, Orientation, Overlay,
    ScrolledWindow, SelectionMode, Stack,
};

use crate::sidebar::SidebarState;
use crate::tabs::TabState;

/// All persistent widgets plus the per-region reconcile state that the pump and
/// signal handlers touch. Held in an `Rc` and shared across closures.
pub struct Shell {
    pub window: adw::ApplicationWindow,
    pub tab_view: adw::TabView,
    pub tab_bar: adw::TabBar,
    /// Swaps between the tab content ("panes") and an empty-state page ("empty").
    pub content_stack: Stack,
    /// Hosts the HUD OSD over the grid (and, transitionally, the modal overlays).
    pub overlay: Overlay,
    /// The single shared terminal grid, reparented into the active pane's tab.
    pub drawing: DrawingArea,

    // Header
    pub title: adw::WindowTitle,
    pub server_btn: Button,
    pub conn_btn: Button,
    pub lock_btn: Button,
    pub header_sig: RefCell<Option<String>>,

    // Sidebar
    pub new_session_btn: Button,
    pub sidebar_list: ListBox,

    // Per-region reconcile state
    pub tabs: TabState,
    pub sidebar: SidebarState,
}

/// Build the window shell around the (already-created) shared `drawing`.
pub fn build(app: &Application, drawing: &DrawingArea) -> Rc<Shell> {
    // ── Header ──
    let title = adw::WindowTitle::new("kmux", "");
    let sidebar_toggle = Button::from_icon_name("sidebar-show-symbolic");
    sidebar_toggle.set_tooltip_text(Some("Toggle sidebar (F9)"));
    let server_btn = Button::from_icon_name("network-server-symbolic");
    server_btn.set_tooltip_text(Some("Switch server"));
    let conn_btn = Button::from_icon_name("network-idle-symbolic");
    let lock_btn = Button::from_icon_name("changes-prevent-symbolic");
    lock_btn.set_tooltip_text(Some("Input is locked"));
    lock_btn.add_css_class("warning");
    lock_btn.set_visible(false);

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&title));
    header.pack_start(&sidebar_toggle);
    header.pack_end(&conn_btn);
    header.pack_end(&server_btn);
    header.pack_end(&lock_btn);

    // ── Pane tab strip + view ──
    let tab_view = adw::TabView::new();
    let tab_bar = adw::TabBar::builder()
        .view(&tab_view)
        .autohide(false)
        .build();
    tab_bar.set_visible(false);

    let empty = adw::StatusPage::builder()
        .icon_name("utilities-terminal-symbolic")
        .title("No session")
        .description("Create a session from the sidebar to get started.")
        .build();

    // Swap the live tab content with an empty-state page.
    let content_stack = Stack::new();
    content_stack.set_vexpand(true);
    content_stack.add_named(&tab_view, Some("panes"));
    content_stack.add_named(&empty, Some("empty"));
    content_stack.set_visible_child_name("empty");

    let banner = adw::Banner::new("");

    let content = GtkBox::new(Orientation::Vertical, 0);
    content.append(&banner);
    content.append(&tab_bar);
    content.append(&content_stack);

    // The overlay wraps the whole content area so the HUD and (transitionally)
    // the modal overlays stay visible in the empty state too; toasts on top.
    let overlay = Overlay::new();
    overlay.set_child(Some(&content));
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&overlay));

    // ── Sessions sidebar (its own header + scrolled list) ──
    let new_session_btn = Button::from_icon_name("list-add-symbolic");
    new_session_btn.set_tooltip_text(Some("New session"));
    let sidebar_header = adw::HeaderBar::new();
    sidebar_header.set_title_widget(Some(&adw::WindowTitle::new("Sessions", "")));
    sidebar_header.set_show_end_title_buttons(false);
    sidebar_header.set_show_start_title_buttons(false);
    sidebar_header.pack_end(&new_session_btn);

    let sidebar_list = ListBox::new();
    sidebar_list.add_css_class("navigation-sidebar");
    sidebar_list.set_selection_mode(SelectionMode::Single);
    let placeholder = Label::new(Some("No sessions"));
    placeholder.add_css_class("dim-label");
    placeholder.set_margin_top(12);
    sidebar_list.set_placeholder(Some(&placeholder));
    let sidebar_scroll = ScrolledWindow::new();
    sidebar_scroll.set_vexpand(true);
    sidebar_scroll.set_child(Some(&sidebar_list));

    let sidebar = adw::ToolbarView::new();
    sidebar.add_top_bar(&sidebar_header);
    sidebar.set_content(Some(&sidebar_scroll));

    // ── Split view + toolbar view + window ──
    let split = adw::OverlaySplitView::new();
    split.set_sidebar(Some(&sidebar));
    split.set_content(Some(&toasts));
    split.set_show_sidebar(true);
    split.set_max_sidebar_width(280.0);

    {
        let split = split.clone();
        sidebar_toggle.connect_clicked(move |_| {
            split.set_show_sidebar(!split.shows_sidebar());
        });
    }

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&split));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("kmux")
        .default_width(960)
        .default_height(620)
        .build();
    window.set_content(Some(&toolbar));

    Rc::new(Shell {
        window,
        tab_view,
        tab_bar,
        content_stack,
        overlay,
        drawing: drawing.clone(),
        title,
        server_btn,
        conn_btn,
        lock_btn,
        header_sig: RefCell::new(None),
        new_session_btn,
        sidebar_list,
        tabs: TabState::new(),
        sidebar: SidebarState::new(),
    })
}
