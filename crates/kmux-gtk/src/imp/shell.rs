//! The native GTK window shell.
//!
//! Native libadwaita chrome: an `adw::HeaderBar`, a collapsible sessions sidebar
//! (`adw::OverlaySplitView`), and a pane tab strip (`adw::TabBar`/`TabView`).
//! Only the terminal `DrawingArea` (painted by `render.rs`) looks like a
//! terminal. Every widget here is a *view* of `AppCore`, reconciled by the pump
//! (`header::sync`, `tabs::sync`, `sidebar::sync`).

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk4::{
    Application, Box as GtkBox, Button, DrawingArea, Label, ListBox, MenuButton, Orientation,
    Overlay, ScrolledWindow, SelectionMode, Stack,
};

use super::sidebar::SidebarState;
use super::tabs::TabState;

/// All persistent widgets plus the per-region reconcile state that the pump and
/// signal handlers touch. Held in an `Rc` and shared across closures.
pub struct Shell {
    pub window: adw::ApplicationWindow,
    pub split: adw::OverlaySplitView,
    pub tab_view: adw::TabView,
    pub tab_bar: adw::TabBar,
    /// Swaps between the tab content ("panes"), an empty-state page ("empty"),
    /// and the process overview ("overview", issue #122).
    pub content_stack: Stack,
    /// The process-overview list (issue #122), reconciled by `overview::sync`.
    pub overview_list: ListBox,
    /// Hosts the HUD OSD over the grid.
    pub overlay: Overlay,
    /// Connecting/disconnected banner above the content.
    pub banner: adw::Banner,
    /// Hosts transient status-message toasts.
    pub toasts: adw::ToastOverlay,
    /// The single shared terminal grid, reparented into the active pane's tab.
    pub drawing: DrawingArea,

    // Header
    pub title: adw::WindowTitle,
    pub server_btn: Button,
    pub conn_btn: Button,
    /// Transport-protocol indicator (issue #69): shows the active transport and,
    /// double-clicked, opens the override chooser. Restyled when overridden.
    pub transport_btn: Button,
    pub lock_btn: Button,
    pub menu_btn: MenuButton,
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
    // ── Header ── (interactive buttons route through `win.*` gio actions)
    let title = adw::WindowTitle::new("kmux", "");
    let sidebar_toggle = Button::from_icon_name("sidebar-show-symbolic");
    sidebar_toggle.set_tooltip_text(Some("Toggle sidebar (F9)"));
    sidebar_toggle.set_action_name(Some("win.toggle-sidebar"));
    let command_btn = Button::from_icon_name("system-search-symbolic");
    command_btn.set_tooltip_text(Some("Command palette (Ctrl+Shift+P)"));
    command_btn.set_action_name(Some("win.command"));
    let server_btn = Button::from_icon_name("network-server-symbolic");
    server_btn.set_tooltip_text(Some("Open launcher (sessions & remotes)"));
    let conn_btn = Button::from_icon_name("network-idle-symbolic");
    let transport_btn = Button::with_label("");
    transport_btn.add_css_class("flat");
    transport_btn.set_tooltip_text(Some("Transport protocol — double-click to override"));
    transport_btn.set_visible(false);
    let lock_btn = Button::from_icon_name("changes-prevent-symbolic");
    lock_btn.set_tooltip_text(Some("Input is locked"));
    lock_btn.add_css_class("warning");
    lock_btn.set_visible(false);
    let menu_btn = MenuButton::new();
    menu_btn.set_icon_name("open-menu-symbolic");
    menu_btn.set_tooltip_text(Some("Main menu"));

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&title));
    header.pack_start(&sidebar_toggle);
    header.pack_start(&command_btn);
    header.pack_end(&menu_btn);
    header.pack_end(&conn_btn);
    header.pack_end(&transport_btn);
    header.pack_end(&lock_btn);
    header.pack_end(&server_btn);

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

    // Swap the live tab content with an empty-state page or the process overview.
    let (overview_box, overview_list) = super::overview::build();
    let content_stack = Stack::new();
    content_stack.set_vexpand(true);
    content_stack.add_named(&tab_view, Some("panes"));
    content_stack.add_named(&empty, Some("empty"));
    content_stack.add_named(&overview_box, Some("overview"));
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
        split,
        tab_view,
        tab_bar,
        content_stack,
        overview_list,
        overlay,
        banner,
        toasts,
        drawing: drawing.clone(),
        title,
        server_btn,
        conn_btn,
        transport_btn,
        lock_btn,
        menu_btn,
        header_sig: RefCell::new(None),
        new_session_btn,
        sidebar_list,
        tabs: TabState::new(),
        sidebar: SidebarState::new(),
    })
}
