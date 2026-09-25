//! The exported driver object: one `KmuxDriver` per Swift window.
//!
//! Split out of `lib.rs`, which was 3,639 lines. Nothing here changed; the
//! generated Swift bindings are byte-identical either way, which is how the
//! move was checked.

use super::*;

/// Opaque, thread-confined handle wrapping a [`FrontendDriver`] and the tokio
/// runtime its background tasks run on. See the module docs for the threading
/// contract.
#[derive(uniffi::Object)]
pub struct KmuxDriver {
    rt: Runtime,
    /// `pub(crate)` so [`super::renderer::KmuxRenderer`] can read the driver's
    /// grids and layout while it holds the lock. Both objects are confined to
    /// the Swift main thread; the mutex is what uniffi's `Object` requires, not
    /// a concurrency claim.
    pub(crate) inner: Mutex<FrontendDriver>,
}

#[uniffi::export]
impl KmuxDriver {
    /// Build a driver and kick off the initial connection (per `config`).
    #[uniffi::constructor]
    pub fn new(config: DriverConfig) -> Result<Arc<Self>, FfiError> {
        // Generate the instance id once and share it between logging and the
        // core, so the client log file and the daemon correlate by the same id.
        let instance_id = generate_instance_id();
        init_ffi_logging(&instance_id);
        // Identify this process as the Swift frontend so every Auth frame reports
        // `frontend = swift` for daemon-side attribution and `kmux clients`.
        kmux_client::set_frontend_kind(kmux_protocol::messages::FrontendKind::Swift);
        tracing::info!(
            server = ?config.server,
            session = ?config.session,
            cols = config.cols,
            rows = config.rows,
            "kmux-ffi: constructing KmuxDriver"
        );
        let rt = Runtime::new().map_err(|e| {
            tracing::error!(error = %e, "kmux-ffi: tokio runtime init failed");
            FfiError::Init {
                message: e.to_string(),
            }
        })?;
        let core = build_core(&config, instance_id);
        // `FrontendDriver::new` spawns the initial bootstrap, so build it with
        // the runtime entered.
        let driver = {
            let _guard = rt.enter();
            FrontendDriver::new(core)
        };
        Ok(Arc::new(Self {
            rt,
            inner: Mutex::new(driver),
        }))
    }

    /// The ABI version this library was built with (see [`KMUX_FFI_ABI_VERSION`]).
    pub fn abi_version(&self) -> u32 {
        KMUX_FFI_ABI_VERSION
    }

    /// Run one pump iteration and return the effects to act on. Call once per
    /// frame. The runtime is entered so the driver's outcome arm can spawn the
    /// SSH supervisor.
    pub fn tick(&self) -> Vec<FfiEffect> {
        let _guard = self.rt.enter();
        // Reconnect results spawn from the driver, so keep a runtime guard.
        let _guard = self.rt.enter();
        // Reconnect results spawn from the driver, so keep a runtime guard.
        let _guard = self.rt.enter();
        // Reconnect results spawn from the driver, so keep a runtime guard.
        let _guard = self.rt.enter();
        // Reconnect results spawn from the driver, so keep a runtime guard.
        let _guard = self.rt.enter();
        // Reconnect results spawn from the driver, so keep a runtime guard.
        let _guard = self.rt.enter();
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.tick().into_iter().map(FfiEffect::from).collect()
    }

    /// Dispatch a curated action; returns any resulting effects. Reconnect /
    /// server-switch are applied internally by the driver.
    pub fn dispatch(&self, action: FfiAction) -> Vec<FfiEffect> {
        let act = Action::from(action);
        // A dispatch can spawn (Reconnect rebuilds the bootstrap task;
        // RecentServers::save uses spawn_blocking), so hold a runtime guard
        // even though the dispatch itself is synchronous.
        let _guard = self.rt.enter();
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.dispatch_action(act)
            .into_iter()
            .map(FfiEffect::from)
            .collect()
    }

    /// Rebuild the connection channels and reconnect to the current target.
    pub fn reconnect(&self) {
        let _guard = self.rt.enter();
        self.inner
            .lock()
            .expect("driver mutex poisoned")
            .reconnect();
    }

    /// Forward raw bytes to the active pane's PTY.
    pub fn send_input(&self, bytes: Vec<u8>) {
        self.inner
            .lock()
            .expect("driver mutex poisoned")
            .send_input(bytes);
    }

    /// Send a printable character as a structured key event. `text` is the
    /// character the keystroke produces (e.g. macOS `charactersIgnoringModifiers`);
    /// `mods` carries the active modifiers. The daemon encodes the bytes under the
    /// live terminal mode, so the frontend never hand-rolls escape sequences.
    /// No-op for empty `text`.
    pub fn send_char(&self, text: String, mods: FfiKeyMods) {
        let Some(ch) = text.chars().next() else {
            return;
        };
        let (code, text, unshifted_codepoint) = char_to_proto_key(ch);
        self.send_key_event(KeyEvent {
            code,
            mods: mods.to_proto(),
            action: KeyAction::Press,
            text,
            unshifted_codepoint,
        });
    }

    /// Send a named key (Enter, arrows, function keys, …) as a structured key
    /// event. See [`send_char`](Self::send_char) for the encoding contract.
    pub fn send_named_key(&self, key: FfiNamedKey, mods: FfiKeyMods) {
        self.send_key_event(KeyEvent {
            code: key.to_code(),
            mods: mods.to_proto(),
            action: KeyAction::Press,
            text: String::new(),
            unshifted_codepoint: 0,
        });
    }

    /// Feed clipboard text back as a paste (in response to
    /// [`FfiEffect::RequestPaste`]).
    pub fn feed_paste(&self, text: String) {
        self.inner
            .lock()
            .expect("driver mutex poisoned")
            .feed_paste(text);
    }

    /// Report a new content size immediately (no debounce).
    pub fn set_term_size(&self, rows: u16, cols: u16, pixel_width: u16, pixel_height: u16) {
        self.inner
            .lock()
            .expect("driver mutex poisoned")
            .set_term_size(TermSize {
                rows,
                cols,
                pixel_width,
                pixel_height,
            });
    }

    /// Report a new content size, debounced (applied from a later [`tick`]).
    ///
    /// [`tick`]: KmuxDriver::tick
    pub fn request_resize(&self, rows: u16, cols: u16, pixel_width: u16, pixel_height: u16) {
        self.inner
            .lock()
            .expect("driver mutex poisoned")
            .request_resize(TermSize {
                rows,
                cols,
                pixel_width,
                pixel_height,
            });
    }

    /// Report whether the app is backgrounded/inactive, for auto-pause (issue
    /// #68). Backgrounding arms a short debounce before the connection pauses;
    /// foregrounding resumes immediately. Drive this from SwiftUI's `scenePhase`
    /// (and/or `NSWindow.occlusionState`). A manual pause is unaffected.
    pub fn set_window_background(&self, backgrounded: bool) {
        self.inner
            .lock()
            .expect("driver mutex poisoned")
            .set_window_background(backgrounded);
    }

    /// Current connection pause state for a status indicator (issue #68).
    pub fn pause_state(&self) -> FfiPauseState {
        self.inner
            .lock()
            .expect("driver mutex poisoned")
            .core()
            .pause_reason()
            .into()
    }

    /// Toggle a pane's exemption from *auto*-pause (issue #68): it keeps
    /// streaming when the window is backgrounded. Drives the pane context-menu
    /// toggle (a manual pause still pauses it).
    pub fn toggle_pane_no_auto_pause(&self, pane_id: String) -> Vec<FfiEffect> {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.mutate(|core| core.toggle_pane_no_auto_pause(&pane_id));
        vec![FfiEffect::NeedsRender]
    }

    /// Toggle a whole session's exemption from auto-pause (issue #68); every
    /// pane in the session inherits it. Drives the session context-menu toggle.
    pub fn toggle_session_no_auto_pause(&self, word_id: String) -> Vec<FfiEffect> {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.mutate(|core| core.toggle_session_no_auto_pause(&word_id));
        vec![FfiEffect::NeedsRender]
    }

    /// Whether `word_id` is marked exempt from auto-pause at the session level
    /// (session menu checkmark; issue #68).
    pub fn session_no_auto_pause(&self, word_id: String) -> bool {
        self.inner
            .lock()
            .expect("driver mutex poisoned")
            .core()
            .session_no_auto_pause(&word_id)
    }

    /// Cheap grid identity for change detection (`None` if no active pane).
    pub fn grid_info(&self) -> Option<GridInfo> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        d.active_grid().map(|g| GridInfo {
            rows: g.rows as u32,
            cols: g.cols as u32,
            generation: g.generation(),
            cells_generation: g.cells_generation(),
        })
    }

    /// The active grid packed for rendering (`None` if no active pane).
    pub fn grid_snapshot(&self) -> Option<GridSnapshot> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let grid = d.active_grid()?;
        let cells = packed::encode_cells(grid, &d.palette);
        let c = grid.cursor();
        Some(GridSnapshot {
            rows: grid.rows as u32,
            cols: grid.cols as u32,
            cursor: FfiCursor {
                row: c.row as u32,
                col: c.col as u32,
                shape: packed::cursor_shape_code(c.shape),
                visible: c.visible,
                blink: c.blink,
            },
            cells,
        })
    }

    /// The active palette (for the renderer + native chrome).
    pub fn theme(&self) -> FfiTheme {
        FfiTheme::from(&self.inner.lock().expect("driver mutex poisoned").palette)
    }

    /// The active terminal appearance (font family/size/style, OpenType
    /// features, cell adjustments) the renderer builds its `NSFont` from.
    pub fn appearance(&self) -> FfiAppearance {
        FfiAppearance::from(&self.inner.lock().expect("driver mutex poisoned").appearance)
    }

    /// The current connection state + badge label.
    pub fn connection(&self) -> FfiConnInfo {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let state = d.mgr.connection_state();
        FfiConnInfo {
            status: FfiConnStatus::from(state),
            label: state.badge_label(),
            transport_overridden: d.mgr.transport_override().is_some(),
        }
    }

    /// Whether a pane is in its soft-close grace window (issue #86), so the
    /// frontend can show an "Undo" affordance.
    pub fn soft_close_pending(&self) -> bool {
        self.inner
            .lock()
            .expect("driver mutex poisoned")
            .has_pending_close()
    }

    /// The session list, with the active session flagged.
    pub fn sessions(&self) -> Vec<FfiSession> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let active = d.mgr.active_session().map(ToString::to_string);
        d.mgr
            .session_list()
            .iter()
            .map(|e| FfiSession {
                active: active.as_deref() == Some(e.meta.word_id.as_str()),
                word_id: e.meta.word_id.clone(),
                name: e.meta.name.clone(),
                cwd: e.meta.cwd.clone(),
                peer: e.peer.clone(),
            })
            .collect()
    }

    /// The process-overview rows (issue #122): a flat, depth-tagged
    /// Session → Tab → Pane → Process tree joined with the latest CPU/memory
    /// snapshot. Polled by the Swift `ProcessOverviewView` while
    /// [`FfiMode::ProcessOverview`] is active; the driver re-requests the
    /// snapshot at ~1 Hz.
    pub fn overview_rows(&self) -> Vec<FfiOverviewRow> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        d.overview_rows()
            .into_iter()
            .map(|r| FfiOverviewRow {
                depth: r.depth,
                kind: overview_kind_to_ffi(r.kind),
                label: r.label,
                detail: r.detail,
                cpu_percent: r.cpu_percent,
                mem_bytes: r.mem_bytes,
                pid: r.pid,
                peer: r.peer,
            })
            .collect()
    }

    /// The connected clients of the active session (issue #146). Polled by the
    /// Swift `ConnectedClientsView` while [`FfiMode::ConnectedClients`] is active;
    /// the driver re-requests the list at ~1 Hz.
    pub fn client_rows(&self) -> Vec<FfiClientRow> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        d.client_rows()
            .into_iter()
            .map(|c| FfiClientRow {
                client_id: c.client_id.0,
                label: c.label,
                machine_id: c.machine_id,
                hostname: c.hostname,
                username: c.username,
                transport: c.transport,
                panes: c.attached_panes,
                is_self: c.is_self,
            })
            .collect()
    }

    /// Kick the client connection `client_id` from the session whose list is
    /// currently shown (issue #146). The list refreshes on the next poll.
    pub fn kick_client(&self, client_id: u64) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.mutate(|core| core.kick_listed_client(kmux_protocol::messages::ClientId(client_id)));
    }

    /// The panes (tabs) of the active session, with the active pane flagged.
    pub fn panes(&self) -> Vec<FfiPane> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let active = d.mgr.active_pane_id().map(ToString::to_string);
        d.mgr
            .active_session_panes()
            .iter()
            .map(|p| FfiPane {
                active: active.as_deref() == Some(p.pane_id.as_str()),
                id: p.pane_id.clone(),
                label: pane_label(p.pane_index, &p.title),
            })
            .collect()
    }

    /// Focus a pane by id (a tab click). Returns any resulting effects.
    pub fn select_pane(&self, id: String) -> Vec<FfiEffect> {
        let _guard = self.rt.enter();
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.apply_top_bar_action(TopBarAction::SelectPane(id))
            .into_iter()
            .map(FfiEffect::from)
            .collect()
    }

    // ── Tiling (Session → Tab → Pane) ────────────────────────────────────────

    /// The tabs of the active session, with the viewed tab flagged.
    pub fn tabs(&self) -> Vec<FfiTab> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let active = d.mgr.active_tab();
        let word = d
            .mgr
            .active_session()
            .map(ToString::to_string)
            .unwrap_or_default();
        d.mgr
            .active_session_tabs()
            .iter()
            .map(|t| {
                // A tab is paused if any of its panes is paused (issue #68).
                let paused = t
                    .layout
                    .leaves()
                    .iter()
                    .any(|idx| d.core().is_pane_paused(&format_pane_id(&word, *idx)));
                let focused_pane = format_pane_id(&word, t.focused_pane);
                let pane_title = d
                    .mgr
                    .pane_info(&focused_pane)
                    .map(|pane| pane.title.as_str())
                    .unwrap_or_default();
                let needs_attention = t
                    .layout
                    .leaves()
                    .iter()
                    .any(|idx| d.mgr.pane_needs_attention(&format_pane_id(&word, *idx)));
                FfiTab {
                    tab_index: t.tab_index,
                    name: tab_label(t.tab_index, &t.name, pane_title),
                    active: active == Some(t.tab_index),
                    paused,
                    needs_attention,
                }
            })
            .collect()
    }

    /// View a tab of the active session by index (a tab-strip click): attaches
    /// its pane set and focuses its pane. Signals a render.
    pub fn select_tab(&self, tab_index: u32) -> Vec<FfiEffect> {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.mutate(|core| core.mgr.select_tab(tab_index));
        vec![FfiEffect::NeedsRender]
    }

    /// Focus a tiled pane by id within the active tab (a click on a tile, or a
    /// keyboard focus move resolved frontend-side). Publishes the shared focus to
    /// the server. Signals a render.
    pub fn focus_pane(&self, pane_id: String) -> Vec<FfiEffect> {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.mutate(|core| core.mgr.focus_pane(pane_id));
        vec![FfiEffect::NeedsRender]
    }

    /// Resolve the active tab's layout tree into per-pane cell rectangles within
    /// an `area_cols × area_rows` content area, via the shared `kmux_app::layout`
    /// resolver (so every frontend computes identical geometry — the determinism
    /// contract that keeps PTYs from thrashing). Empty when there is no active
    /// tab. The frontend tiles one terminal view per rect, then pushes the
    /// resolved sizes back via [`set_pane_sizes`](Self::set_pane_sizes).
    pub fn layout(&self, area_cols: u16, area_rows: u16) -> Vec<FfiPaneRect> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let Some(word) = d.mgr.active_session().map(ToString::to_string) else {
            return Vec::new();
        };
        let focused = d.mgr.active_pane_id().and_then(pane_index);
        // `render_layout` collapses to the focused pane when zoomed.
        let Some(layout) = d.mgr.render_layout() else {
            return Vec::new();
        };
        kmux_app::layout::resolve_layout(
            &layout,
            area_cols,
            area_rows,
            &kmux_app::layout::LayoutConfig::default(),
        )
        .into_iter()
        .map(|r| {
            let pane_id = format_pane_id(&word, r.pane_index);
            let (progress_state, progress) = d
                .mgr
                .pane_info(&pane_id)
                .map_or((FfiProgressState::Remove, None), |p| {
                    (p.progress_state.into(), p.progress)
                });
            let paused = d.core().is_pane_paused(&pane_id);
            let no_auto_pause = d.core().pane_no_auto_pause(&pane_id);
            FfiPaneRect {
                pane_id,
                pane_index: r.pane_index,
                col: r.col as u32,
                row: r.row as u32,
                cols: r.cols as u32,
                rows: r.rows as u32,
                focused: focused == Some(r.pane_index),
                progress_state,
                progress,
                paused,
                no_auto_pause,
            }
        })
        .collect()
    }

    /// Push the resolved per-pane sizes for the visible set; each changed pane's
    /// PTY is resized to its tile. Compute these from [`layout`](Self::layout)
    /// rects × the cell pixel size (mirrors the GTK `tiles::push_sizes`).
    pub fn set_pane_sizes(&self, sizes: Vec<FfiPaneSize>) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let mapped = sizes
            .into_iter()
            .map(|s| {
                (
                    s.pane_id,
                    TermSize {
                        rows: s.rows,
                        cols: s.cols,
                        pixel_width: s.pixel_width,
                        pixel_height: s.pixel_height,
                    },
                )
            })
            .collect();
        d.mgr_mut().set_pane_sizes(mapped);
    }

    /// Enumerate the active tab's draggable dividers within an
    /// `area_cols × area_rows` content area, via the shared
    /// `kmux_app::layout::resolve_dividers` (so divider geometry matches the
    /// tiles from [`layout`](Self::layout)). Empty when there is no active tab
    /// or the focused pane is zoomed (a single tile has no boundary). The
    /// frontend hit-tests a pointer against the `hit_*` strip for the resize
    /// cursor + drag start.
    pub fn dividers(&self, area_cols: u16, area_rows: u16) -> Vec<FfiDivider> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let Some(layout) = d.mgr.render_layout() else {
            return Vec::new();
        };
        kmux_app::layout::resolve_dividers(
            &layout,
            area_cols,
            area_rows,
            &kmux_app::layout::LayoutConfig::default(),
        )
        .into_iter()
        .map(FfiDivider::from_layout)
        .collect()
    }

    /// Resize a split by dragging its `divider` so the boundary sits at
    /// `pointer_cell` (cells along the divider's drag axis). Recomputes the new
    /// ratios against the current tree via `kmux_app::layout::ratios_for_drag`
    /// and sends `SetLayoutRatios` (the same wire path as keyboard resize; the
    /// server clamps, renormalizes, and broadcasts). No-op (empty effects) when
    /// the split was reshaped or the move clamps to nothing. Signals a render.
    pub fn apply_divider_drag(&self, divider: FfiDivider, pointer_cell: u32) -> Vec<FfiEffect> {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let Some(layout) = d.mgr.render_layout() else {
            return Vec::new();
        };
        let divider = divider.into_layout();
        let Some(ratios) =
            kmux_app::layout::ratios_for_drag(&layout, &divider, pointer_cell as u16)
        else {
            return Vec::new();
        };
        d.mutate(|core| core.mgr.set_layout_ratios(divider.path, ratios));
        vec![FfiEffect::NeedsRender]
    }

    /// Reset the split a `divider` belongs to back to even children (a
    /// double-click on the divider). No-op when the divider's split is gone.
    /// Signals a render.
    pub fn reset_divider(&self, divider: FfiDivider) -> Vec<FfiEffect> {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let Some(layout) = d.mgr.render_layout() else {
            return Vec::new();
        };
        let path = divider.path;
        let Some(ratios) = kmux_app::layout::even_ratios_at(&layout, &path) else {
            return Vec::new();
        };
        d.mutate(|core| core.mgr.set_layout_ratios(path, ratios));
        vec![FfiEffect::NeedsRender]
    }

    /// Rename a tab of the active session (a native rename sheet). Signals a
    /// render.
    pub fn rename_tab(&self, tab_index: u32, name: String) -> Vec<FfiEffect> {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.mutate(|core| core.mgr.rename_tab(tab_index, &name));
        vec![FfiEffect::NeedsRender]
    }

    /// Move a tab to a zero-based position in the active session.
    pub fn reorder_tab(&self, tab_index: u32, new_position: u32) {
        self.inner
            .lock()
            .expect("driver mutex poisoned")
            .mgr_mut()
            .reorder_tab(tab_index, new_position);
    }

    /// Cheap grid identity for a specific pane (per-tile change detection).
    pub fn grid_info_for(&self, pane_id: String) -> Option<GridInfo> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        d.mgr.buffer(&pane_id).map(|g| GridInfo {
            rows: g.rows as u32,
            cols: g.cols as u32,
            generation: g.generation(),
            cells_generation: g.cells_generation(),
        })
    }

    /// A specific pane's grid packed for rendering (`None` if not attached).
    pub fn grid_snapshot_for(&self, pane_id: String) -> Option<GridSnapshot> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let grid = d.mgr.buffer(&pane_id)?;
        let cells = packed::encode_cells(grid, &d.palette);
        let c = grid.cursor();
        Some(GridSnapshot {
            rows: grid.rows as u32,
            cols: grid.cols as u32,
            cursor: FfiCursor {
                row: c.row as u32,
                col: c.col as u32,
                shape: packed::cursor_shape_code(c.shape),
                visible: c.visible,
                blink: c.blink,
            },
            cells,
        })
    }

    /// A specific pane's selection spans (for its selection wash).
    pub fn selection_for(&self, pane_id: String) -> Vec<FfiSelectionSpan> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let Some(grid) = d.mgr.buffer(&pane_id) else {
            return Vec::new();
        };
        grid.visible_selection_spans()
            .into_iter()
            .map(|(row, col_start, col_end)| FfiSelectionSpan {
                row: row as u32,
                col_start: col_start as u32,
                col_end: col_end as u32,
            })
            .collect()
    }

    /// A specific pane's scrollback position (for its scroll indicator).
    pub fn scroll_info_for(&self, pane_id: String) -> FfiScrollInfo {
        let d = self.inner.lock().expect("driver mutex poisoned");
        match d.mgr.buffer(&pane_id) {
            Some(g) => FfiScrollInfo {
                offset: g.scroll_offset() as u32,
                total: g.total_scrollback_display_rows() as u32,
            },
            None => FfiScrollInfo {
                offset: 0,
                total: 0,
            },
        }
    }

    /// Set a normal (drag) selection between two *visible* viewport cells. The
    /// cells are mapped scroll-aware, so this works while scrolled into history.
    pub fn set_selection(&self, anchor_row: u32, anchor_col: u32, end_row: u32, end_col: u32) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        if let Some(grid) = d.mgr_mut().active_grid_mut() {
            let anchor = grid.visible_to_abs(anchor_row as usize, anchor_col as usize);
            let end = grid.visible_to_abs(end_row as usize, end_col as usize);
            grid.set_selection(Some(Selection {
                anchor,
                end,
                mode: SelectionMode::Normal,
            }));
        }
    }

    /// Select the word at a *visible* viewport cell (double-click).
    pub fn select_word_at(&self, row: u32, col: u32) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        if let Some(grid) = d.mgr_mut().active_grid_mut() {
            let pos = grid.visible_to_abs(row as usize, col as usize);
            let (anchor, end) = grid.find_word_boundaries(pos);
            grid.set_selection(Some(Selection {
                anchor,
                end,
                mode: SelectionMode::Word,
            }));
        }
    }

    /// Select the whole line at a *visible* viewport row (triple-click).
    pub fn select_line_at(&self, row: u32) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        if let Some(grid) = d.mgr_mut().active_grid_mut() {
            let cols = grid.cols;
            let abs_row = grid.visible_to_abs(row as usize, 0).row;
            grid.set_selection(Some(Selection {
                anchor: GridPos {
                    row: abs_row,
                    col: 0,
                },
                end: GridPos {
                    row: abs_row,
                    col: cols.saturating_sub(1),
                },
                mode: SelectionMode::Line,
            }));
        }
    }

    /// Clear the active selection.
    pub fn clear_selection(&self) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        if let Some(grid) = d.mgr_mut().active_grid_mut() {
            grid.clear_selection();
        }
    }

    /// The active selection as per-visible-row spans (for the selection wash),
    /// empty when there is no selection. Scroll- and wrap-aware, so the wash
    /// paints over scrollback rows while scrolled into history.
    pub fn selection(&self) -> Vec<FfiSelectionSpan> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let Some(grid) = d.mgr.active_grid() else {
            return Vec::new();
        };
        grid.visible_selection_spans()
            .into_iter()
            .map(|(row, col_start, col_end)| FfiSelectionSpan {
                row: row as u32,
                col_start: col_start as u32,
                col_end: col_end as u32,
            })
            .collect()
    }

    /// Mouse-wheel scroll at a *visible* viewport cell. Forwards an SGR/X10 wheel
    /// event to the PTY when the pane has mouse reporting on; otherwise scrolls
    /// local scrollback. `lines` > 0 scrolls up (into history). Mirrors the GTK
    /// frontend's `scroll_pane`.
    pub fn scroll_at(&self, col: u32, row: u32, lines: i32) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let Some(pane_id) = d.mgr.active_pane_id().map(ToString::to_string) else {
            return;
        };
        let use_pty = d
            .mgr
            .buffer(&pane_id)
            .is_some_and(|g| g.modes().mouse_report());
        if use_pty {
            let sgr = d
                .mgr
                .buffer(&pane_id)
                .is_some_and(|g| g.modes().sgr_mouse());
            let bytes = encode_mouse_scroll(col as u16 + 1, row as u16 + 1, lines, sgr);
            if !bytes.is_empty() {
                d.send_input(bytes);
            }
        } else if let Some(grid) = d.mgr_mut().buffer_mut(&pane_id) {
            if lines > 0 {
                grid.scroll_up(lines as usize);
            } else {
                grid.scroll_down((-lines) as usize);
            }
        }
    }

    /// Forward a mouse button/drag/release to the active pane's inner program
    /// when it has enabled mouse tracking, returning `true` iff it was sent (the
    /// frontend then skips its own client-side text selection). `col`/`row` are
    /// 0-based *visible* viewport cells (converted to the 1-based terminal
    /// coordinates here, like [`KmuxDriver::scroll_at`]). `button_held` gates
    /// motion under button-event tracking (mode 1002). A shift-held event is
    /// never forwarded — Shift is the local-selection bypass. Mirrors the GTK
    /// frontend's `report_mouse` calls; the policy lives in
    /// `SessionManager::report_mouse`.
    pub fn mouse_event(
        &self,
        col: u32,
        row: u32,
        button: FfiMouseButton,
        kind: FfiMouseKind,
        mods: FfiMouseMods,
        button_held: bool,
    ) -> bool {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let ev = MouseEvent {
            button: button.to_client(),
            kind: kind.to_client(),
            col: col as u16 + 1,
            row: row as u16 + 1,
            mods: mods.to_client(),
        };
        d.mgr_mut().report_mouse(button_held, ev)
    }

    /// Scroll the active pane's *local* scrollback by `lines` display rows
    /// (`> 0` = up into history). Unlike [`KmuxDriver::scroll_at`] this never
    /// forwards to the PTY — used for drag auto-scroll, which must reveal
    /// scrollback for selection regardless of PTY mouse reporting (mirrors the
    /// GTK frontend's direct `grid.scroll_up/scroll_down`).
    pub fn scroll_lines(&self, lines: i32) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        if let Some(grid) = d.mgr_mut().active_grid_mut() {
            if lines > 0 {
                grid.scroll_up(lines as usize);
            } else {
                grid.scroll_down((-lines) as usize);
            }
        }
    }

    /// Scrollback position for the scroll indicator.
    pub fn scroll_info(&self) -> FfiScrollInfo {
        let d = self.inner.lock().expect("driver mutex poisoned");
        match d.mgr.active_grid() {
            Some(g) => FfiScrollInfo {
                offset: g.scroll_offset() as u32,
                total: g.total_scrollback_display_rows() as u32,
            },
            None => FfiScrollInfo {
                offset: 0,
                total: 0,
            },
        }
    }

    /// Autocomplete hints for an arbitrary `/`-command-palette input, without
    /// changing the current mode. For a native palette that owns its own text
    /// field instead of driving `Mode::Command` char-by-char.
    pub fn command_hints(&self, input: String) -> Vec<FfiCommandHint> {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        // A query, so no render is requested. `hints_for` owns the mode swap it
        // needs; this used to do it here, in the FFI layer, by reaching into
        // `AppCore::mode` directly.
        cmd::hint::hints_for(d.core_for_query(), &input)
            .into_iter()
            .map(|h| FfiCommandHint {
                display: h.display,
                summary: h.summary.to_string(),
                replacement: h.replacement,
                append_space: h.append_space,
            })
            .collect()
    }

    /// Parse and execute a `/`-command line in one shot (reconnect / server
    /// switch applied internally), returning any resulting effects.
    pub fn run_command(&self, input: String) -> Vec<FfiEffect> {
        // A dispatch can spawn (Reconnect rebuilds the bootstrap task;
        // RecentServers::save uses spawn_blocking), so hold a runtime guard
        // even though the dispatch itself is synchronous.
        let _guard = self.rt.enter();
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.mutate(|core| {
            core.mode = Mode::Command(CommandState {
                buffer: input.clone(),
                cursor: input.len(),
                ..CommandState::default()
            });
        });
        d.dispatch_action(Action::CommandSubmit)
            .into_iter()
            .map(FfiEffect::from)
            .collect()
    }

    /// The currently open picker (session / server / directory), or `None`.
    pub fn picker(&self) -> Option<FfiPicker> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let core = d.core();
        match core.mode {
            Mode::SessionPicker => {
                let mut entries = vec![FfiPickerEntry {
                    label: "[+] New session".to_string(),
                    detail: String::new(),
                }];
                for e in core.session_picker_matches() {
                    entries.push(FfiPickerEntry {
                        label: core.mgr.display_name_for(&e.meta.word_id),
                        detail: e.meta.cwd.clone(),
                    });
                }
                Some(FfiPicker {
                    kind: FfiPickerKind::Session,
                    query: core.session_picker_search.clone(),
                    selected: core.session_picker_selected as u32,
                    entries,
                })
            }
            Mode::DirectoryPicker => {
                // The directory picker is a *browser* of the daemon host's
                // filesystem. The richer per-row state is exposed via
                // `dir_browser()`; this generic getter keeps the picker sheet
                // presenting and shows readable row labels.
                let entries = core
                    .dir_browser_rows()
                    .into_iter()
                    .map(|row| FfiPickerEntry {
                        label: dir_row_label(&row),
                        detail: String::new(),
                    })
                    .collect();
                Some(FfiPicker {
                    kind: FfiPickerKind::Directory,
                    query: core.dir_picker_buffer.clone(),
                    selected: core.dir_picker_selected as u32,
                    entries,
                })
            }
            _ => None,
        }
    }

    /// The directory browser's full state (rows with their kind, the browsed
    /// directory, filter, selection, and any listing error), or `None` when the
    /// directory browser is not open. Backs the native directory-browser UI.
    pub fn dir_browser(&self) -> Option<FfiDirBrowser> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let core = d.core();
        if !matches!(core.mode, Mode::DirectoryPicker) {
            return None;
        }
        let rows = core
            .dir_browser_rows()
            .into_iter()
            .map(|row| {
                let label = dir_row_label(&row);
                let (kind, path) = match row {
                    DirBrowserRow::CreateHere { cwd } => (FfiDirRowKind::CreateHere, cwd),
                    DirBrowserRow::Up { parent } => (FfiDirRowKind::Up, parent),
                    DirBrowserRow::Enter { path, .. } => (FfiDirRowKind::Enter, path),
                };
                FfiDirRow { kind, label, path }
            })
            .collect();
        Some(FfiDirBrowser {
            cwd: core.dir_browser_cwd.clone(),
            query: core.dir_picker_buffer.clone(),
            selected: core.dir_picker_selected as u32,
            rows,
            error: core.dir_browser_error().map(ToString::to_string),
        })
    }

    /// The unified session launcher's full state (issue #121), or `None` when it
    /// is not open. Driven by the generic picker methods (`set_picker_search`,
    /// `set_picker_selected`, `activate_picker`, `cancel_picker`) plus the
    /// `submit_*` helpers below.
    pub fn launch_picker(&self) -> Option<FfiLaunchPicker> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let core = d.core();
        if !matches!(core.mode, Mode::LaunchPicker) {
            return None;
        }
        let rows = core
            .launch_rows()
            .into_iter()
            .map(launch_row_to_ffi)
            .collect();
        Some(FfiLaunchPicker {
            query: core.launch_search.clone(),
            selected: core.launch_selected as u32,
            rows,
        })
    }

    /// Open the unified session launcher (the new-session button).
    pub fn open_launch_picker(&self) -> Vec<FfiEffect> {
        let _guard = self.rt.enter();
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.apply_top_bar_action(TopBarAction::OpenLaunchPicker)
            .into_iter()
            .map(FfiEffect::from)
            .collect()
    }

    /// Build a peer from the add-remote form, register + connect it, and persist
    /// SSH ones (issue #121). Returns an error message when the form is
    /// incomplete (and leaves the form open), or `None` on success.
    pub fn submit_add_remote(&self, form: FfiAddRemoteForm) -> Option<String> {
        let _guard = self.rt.enter();
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.mutate(|core| core.submit_add_remote(form.into())).err()
    }

    /// Create a new session on a federated `peer` at `cwd` (issue #121). An empty
    /// `cwd` lets the remote daemon resolve a default. Closes the prompt.
    pub fn submit_remote_new_session(&self, peer: String, cwd: String) {
        let _guard = self.rt.enter();
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.mutate(|core| core.submit_remote_new_session(peer, cwd));
    }

    /// Disconnect a federated remote (issue #121): drop its link and forget it.
    pub fn disconnect_remote(&self, peer: String) {
        let _guard = self.rt.enter();
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.mutate(|core| core.disconnect_remote(&peer));
    }

    /// Open the session picker.
    pub fn open_session_picker(&self) -> Vec<FfiEffect> {
        let _guard = self.rt.enter();
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.apply_top_bar_action(TopBarAction::OpenSessionPicker)
            .into_iter()
            .map(FfiEffect::from)
            .collect()
    }

    /// Set the open picker's search/filter text (resets the selection to row 0).
    pub fn set_picker_search(&self, text: String) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.mutate(|core| core.set_picker_search(text));
    }

    /// Set the open picker's highlighted row (hover/click).
    pub fn set_picker_selected(&self, index: u32) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.mutate(|core| core.set_picker_selected(index as usize));
    }

    /// Activate the open picker's current selection (click / Enter). May switch
    /// servers (server picker) or select a session.
    pub fn activate_picker(&self) -> Vec<FfiEffect> {
        let _guard = self.rt.enter();
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.activate_picker_selection()
            .into_iter()
            .map(FfiEffect::from)
            .collect()
    }

    /// Submit the directory browser's selected row: create-here returns to
    /// normal interaction, navigation (Up / into a subdir, or a typed absolute
    /// path) refreshes the listing in place. Also honors a typed absolute path
    /// in the filter when it matches no listed row.
    pub fn submit_directory(&self) -> Vec<FfiEffect> {
        // A dispatch can spawn (Reconnect rebuilds the bootstrap task;
        // RecentServers::save uses spawn_blocking), so hold a runtime guard
        // even though the dispatch itself is synchronous.
        let _guard = self.rt.enter();
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.dispatch_action(Action::DirPickerSubmit)
            .into_iter()
            .map(FfiEffect::from)
            .collect()
    }

    /// Activate directory-browser row `index` (a tap): selects it, then submits.
    /// `CreateHere` creates the session and dismisses; Up / a subdirectory
    /// navigate and keep the browser open (it refreshes when the listing lands).
    pub fn dir_browser_activate(&self, index: u32) -> Vec<FfiEffect> {
        // A dispatch can spawn (Reconnect rebuilds the bootstrap task;
        // RecentServers::save uses spawn_blocking), so hold a runtime guard
        // even though the dispatch itself is synchronous.
        let _guard = self.rt.enter();
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.mutate(|core| core.set_picker_selected(index as usize));
        d.dispatch_action(Action::DirPickerSubmit)
            .into_iter()
            .map(FfiEffect::from)
            .collect()
    }

    /// Create a new session in the directory currently being browsed (the
    /// `CreateHere` affordance), regardless of the highlighted row.
    pub fn dir_browser_open_here(&self) -> Vec<FfiEffect> {
        // A dispatch can spawn (Reconnect rebuilds the bootstrap task;
        // RecentServers::save uses spawn_blocking), so hold a runtime guard
        // even though the dispatch itself is synchronous.
        let _guard = self.rt.enter();
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.mutate(|core| core.set_picker_selected(0));
        d.dispatch_action(Action::DirPickerSubmit)
            .into_iter()
            .map(FfiEffect::from)
            .collect()
    }

    /// Close any open picker / overlay (back to normal interaction).
    pub fn cancel_picker(&self) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.mutate(|core| core.mode = Mode::Normal);
    }

    /// Rename a session by word id (trims surrounding whitespace).
    pub fn rename_session(&self, word_id: String, name: String) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.mutate(|core| core.mgr.rename_session(&word_id, name.trim()));
    }

    /// Request confirmation before closing a session by word id.
    pub fn close_session(&self, word_id: String) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.mutate(|core| core.confirm_close_session(&word_id));
    }

    /// Confirm the pending session close, if a close confirmation is open.
    pub fn confirm_close_session(&self) -> Vec<FfiEffect> {
        // A dispatch can spawn (Reconnect rebuilds the bootstrap task;
        // RecentServers::save uses spawn_blocking), so hold a runtime guard
        // even though the dispatch itself is synchronous.
        let _guard = self.rt.enter();
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.dispatch_action(Action::ConfirmCloseSession)
            .into_iter()
            .map(FfiEffect::from)
            .collect()
    }

    /// Whether the performance HUD ticker is shown.
    pub fn hud_visible(&self) -> bool {
        self.inner
            .lock()
            .expect("driver mutex poisoned")
            .hud_visible
    }

    /// Whether the metrics inspector overlay is open.
    pub fn metrics_visible(&self) -> bool {
        self.inner
            .lock()
            .expect("driver mutex poisoned")
            .metrics_overlay_visible
    }

    /// Whether the connection inspector overlay is open (issue #60).
    pub fn connection_visible(&self) -> bool {
        self.inner
            .lock()
            .expect("driver mutex poisoned")
            .connection_overlay_visible
    }

    /// Whether the render-debug overlay is shown.
    pub fn render_debug_visible(&self) -> bool {
        self.inner
            .lock()
            .expect("driver mutex poisoned")
            .render_debug_visible()
    }

    /// The live connection / session / handshake details for the connection
    /// inspector. Built from the toolkit-neutral `ConnectionInfo`.
    pub fn connection_details(&self) -> FfiConnectionDetails {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let info = d.core().connection_info();
        FfiConnectionDetails {
            server: info.server,
            is_local: info.is_local,
            endpoint: info.endpoint,
            state: info.state,
            connected: info.connected,
            transport: info.transport,
            connection_id: info.connection_id,
            client_id: info.client_id,
            server_version: info.server_version,
            protocol_version: info.protocol_version,
            accept_invalid_certs: info.accept_invalid_certs,
            rtt: info.rtt.map(|r| FfiRtt {
                ewma_ms: r.ewma_ms,
                recent_avg_ms: r.recent_avg_ms,
                recent_max_ms: r.recent_max_ms,
                samples: r.samples,
            }),
            transports: info
                .transports
                .into_iter()
                .map(|t| FfiTransportTraffic {
                    label: t.label,
                    bytes_in: t.bytes_in,
                    bytes_out: t.bytes_out,
                    msgs_in: t.msgs_in,
                    msgs_out: t.msgs_out,
                })
                .collect(),
        }
    }

    /// A snapshot of the client-side performance metrics.
    pub fn metrics(&self) -> FfiMetrics {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let core = d.core();
        let snap = core.mgr.metrics.snapshot(core.force_snapshot_mode);
        let c = &snap.counters;
        FfiMetrics {
            net_apply_avg_ms: snap.net_apply_avg_ms,
            net_apply_max_ms: snap.net_apply_max_ms,
            apply_avg_ms: snap.apply_avg_ms,
            batch_avg: snap.batch_avg,
            last_diff_ops: snap.last_diff_ops as u64,
            last_large_diff_ms: snap.last_large_diff_ms,
            snapshot_mode: snap.snapshot_mode,
            stale_discards: c.stale_discards,
            seqno_gaps: c.seqno_gaps,
            lag_events: c.lag_events,
            resyncs: c.resyncs,
            show_perf_counters: core.show_perf_counters,
            net_latency_ms: core.net_latency_ms(),
            latency_stale: core.net_latency_stale(),
            render_fps: core.render_fps(),
        }
    }

    /// What the renderer is handed for the focused pane this frame, for the
    /// render-debug overlay. Swift passes its content-area pixel size, scale,
    /// renderer leaf, and cell geometry; the cursor's pixel rects are computed
    /// here via [`kmux_render::cursor_geometry`] so they match the renderer.
    pub fn render_debug(
        &self,
        frame_width: u32,
        frame_height: u32,
        scale: f32,
        renderer: String,
        cell_w: f32,
        cell_h: f32,
    ) -> FfiRenderDebug {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let snap = d.render_debug_snapshot(frame_width, frame_height, scale, &renderer);
        let cell = kmux_render::CellMetrics::new(cell_w, cell_h);

        let mut out = FfiRenderDebug {
            frame_width: snap.frame_width,
            frame_height: snap.frame_height,
            scale: snap.scale,
            renderer: snap.renderer,
            blink_on: snap.blink_on,
            cursor_thickness: cell.cursor_thickness,
            has_pane: false,
            pane_id: String::new(),
            grid_cols: 0,
            grid_rows: 0,
            scroll_offset: 0,
            has_cursor: false,
            cursor_col: 0,
            cursor_row: 0,
            cursor_shape: 0,
            cursor_blink: false,
            cursor_visible: false,
            cursor_is_drawn: false,
            cursor_in_range: false,
            cursor_cell_x: 0.0,
            cursor_cell_y: 0.0,
            cursor_rects: Vec::new(),
        };

        if let Some(p) = snap.pane {
            out.has_pane = true;
            out.pane_id = p.pane_id;
            out.grid_cols = p.grid_cols as u32;
            out.grid_rows = p.grid_rows as u32;
            out.scroll_offset = p.scroll_offset as u64;
            if let Some(c) = p.cursor {
                let cv = CursorView {
                    col: c.col,
                    row: c.row,
                    shape: c.shape,
                    blink: c.blink,
                    visible: c.visible,
                };
                let geo =
                    kmux_render::cursor_geometry(&cv, (0.0, 0.0), p.grid_cols, p.grid_rows, &cell);
                out.has_cursor = true;
                out.cursor_col = c.col as u32;
                out.cursor_row = c.row as u32;
                out.cursor_shape = packed::cursor_shape_code(c.shape);
                out.cursor_blink = c.blink;
                out.cursor_visible = c.visible;
                out.cursor_is_drawn = c.is_drawn;
                out.cursor_in_range = geo.in_range;
                out.cursor_cell_x = geo.cell_origin.0;
                out.cursor_cell_y = geo.cell_origin.1;
                out.cursor_rects = geo
                    .rects
                    .into_iter()
                    .map(|r| FfiCursorRect {
                        x: r.x,
                        y: r.y,
                        w: r.w,
                        h: r.h,
                    })
                    .collect();
            }
        }
        out
    }

    /// The built-in theme names (for a Preferences theme picker).
    pub fn available_themes(&self) -> Vec<String> {
        theme::BUILTIN_THEMES
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    /// Switch the active palette to a built-in theme by name (no-op if unknown).
    /// The driver emits `PaletteChanged` from the next [`tick`](Self::tick).
    pub fn set_theme(&self, name: String) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        if let Some(t) = theme::builtin_theme(&name) {
            d.mutate(|core| core.palette = t);
        }
    }

    /// Whether the cursor is shown on the current frame (blink phase).
    pub fn blink_on(&self) -> bool {
        self.inner.lock().expect("driver mutex poisoned").blink_on()
    }

    /// Whether the inner-pane cursor is allowed to blink (Preferences toggle).
    pub fn cursor_blink_enabled(&self) -> bool {
        self.inner
            .lock()
            .expect("driver mutex poisoned")
            .cursor_blink_enabled
    }

    /// Enable/disable cursor blinking live and persist it to `config.toml`. When
    /// disabled the cursor is drawn steady; the driver pins the blink phase solid
    /// on the next [`tick`](Self::tick).
    pub fn set_cursor_blink_enabled(&self, enabled: bool) {
        {
            let mut d = self.inner.lock().expect("driver mutex poisoned");
            if d.cursor_blink_enabled == enabled {
                return;
            }
            d.mutate(|core| core.cursor_blink_enabled = enabled);
        }
        // Persist (load-modify-save so theme/font are preserved), mirroring the
        // GTK preferences window.
        let mut cfg = config::load();
        cfg.cursor_blink = Some(enabled);
        if let Err(e) = config::save(&cfg) {
            tracing::error!("failed to persist cursor_blink: {e}");
        }
    }

    /// Which interaction mode / overlay is active.
    pub fn mode(&self) -> FfiMode {
        mode_to_ffi(&self.inner.lock().expect("driver mutex poisoned").mode)
    }
}

impl KmuxDriver {
    /// Forward one structured key event and reset the blink phase, snapping the
    /// viewport to the live bottom first (mirrors the GTK key handler). Not
    /// exported: `send_char` / `send_named_key` are the public entry points.
    fn send_key_event(&self, ev: KeyEvent) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.scroll_to_bottom();
        d.send_keys(vec![ev]);
    }
}
