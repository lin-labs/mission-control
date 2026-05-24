/// Trajectory editing state machine for the detail pane.
///
/// This module owns:
/// - `EditMode` — nav vs insert
/// - `TrajectoryEditState` — cursor, buffers, mode
/// - `handle_key` — maps key events to mutations
/// - `save` — commits an edit session to disk (snapshot + inputs + events)
use crate::mc_data::events::{Event, Kind, Source};
use crate::mc_data::inputs::{InputContext, write_input};
use crate::mc_data::paths;
use crate::mc_data::snapshots::{highest_snapshot, write_snapshot};
use crate::mc_data::trajectory::{
    Item, SECTION_CURRENT_SURFACES, SECTION_GOALS, TrajectoryDoc,
};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

// ──────────────────────────────────────────────────────────────────────────────
// Public types
// ──────────────────────────────────────────────────────────────────────────────

/// Which text buffer the cursor is in while inserting.
#[derive(Debug, Clone, PartialEq)]
pub enum InsertFocus {
    /// Editing the trajectory item text.
    Item,
    /// Editing the input-context (user explanation) buffer.
    InputCtx,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EditMode {
    /// Normal cursor movement; no text buffer open.
    Nav,
    /// Single-line text edit for a trajectory item.
    Insert { focus: InsertFocus },
}

/// A pending line-level change produced by `handle_key`.
/// Accumulated and flushed to `events.jsonl` on Esc-save.
#[derive(Debug, Clone)]
pub enum EditAction {
    /// Text of an existing item was changed.
    Edit {
        section: String,
        before: String,
        after: String,
    },
    /// A new item was inserted.
    Add { section: String, after: String },
    /// An item was deleted.
    Delete { section: String, before: String },
    /// A checkbox was toggled on.
    Check {
        section: String,
        before: String,
        after: String,
    },
    /// A checkbox was toggled off.
    Uncheck {
        section: String,
        before: String,
        after: String,
    },
    /// An item was reordered within its section.
    Move { section: String, before: String },
}

/// Per-workspace editing state.
#[derive(Debug, Clone)]
pub struct TrajectoryEditState {
    /// Index into `doc.sections` (0 = Goal, 1 = Current surfaces, 2 = Tasks).
    pub cursor_section: usize,
    /// Index into `doc.sections[cursor_section].items`.
    pub cursor_item: usize,
    pub mode: EditMode,
    /// In-flight text for the item being edited (insert mode only).
    pub edit_buffer: String,
    /// In-flight text for the user-explanation pane (insert mode only).
    pub input_ctx_buffer: String,
    /// Text of the item when insert mode was entered — used to detect no-op.
    pub edit_start_text: Option<String>,
    /// Cursor column within `edit_buffer` in insert mode (char count, not byte count).
    pub cursor_col: usize,
    /// Timestamp of the first `d` keypress, for the `dd` two-key sequence.
    pub pending_d_at: Option<std::time::Instant>,
}

impl Default for TrajectoryEditState {
    fn default() -> Self {
        Self {
            cursor_section: 0,
            cursor_item: 0,
            mode: EditMode::Nav,
            edit_buffer: String::new(),
            input_ctx_buffer: String::new(),
            edit_start_text: None,
            cursor_col: 0,
            pending_d_at: None,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Key handling
// ──────────────────────────────────────────────────────────────────────────────

/// Process one key event. Mutates `state` and `doc` in place, and returns any
/// `EditAction`s that should be emitted to events.jsonl on the next save.
///
/// Returns a `Vec` because a single keystroke can produce at most one action
/// (toggle/delete/move) — but we return a Vec for uniformity with save.
pub fn handle_key(
    state: &mut TrajectoryEditState,
    doc: &mut TrajectoryDoc,
    key: KeyEvent,
) -> Vec<EditAction> {
    match &state.mode {
        EditMode::Nav => handle_nav_key(state, doc, key),
        EditMode::Insert { .. } => handle_insert_key(state, doc, key),
    }
}

/// Returns true if the cursor is on the `## Current surfaces` section.
/// Edit-mutating keys are blocked there because the cmux-projection refresh
/// loop would immediately clobber any user changes.
fn is_current_surfaces_row(state: &TrajectoryEditState, doc: &TrajectoryDoc) -> bool {
    doc.sections
        .get(state.cursor_section)
        .map(|s| s.name == SECTION_CURRENT_SURFACES)
        .unwrap_or(false)
}

fn handle_nav_key(
    state: &mut TrajectoryEditState,
    doc: &mut TrajectoryDoc,
    key: KeyEvent,
) -> Vec<EditAction> {
    let mut actions = Vec::new();
    match key.code {
        // ── Movement ────────────────────────────────────────────────────────
        KeyCode::Char('j') | KeyCode::Down => {
            state.pending_d_at = None;
            move_cursor_down(state, doc);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            state.pending_d_at = None;
            move_cursor_up(state, doc);
        }
        KeyCode::Char('g') => {
            state.pending_d_at = None;
            state.cursor_section = first_non_empty_section(doc);
            state.cursor_item = 0;
        }
        KeyCode::Char('G') => {
            state.pending_d_at = None;
            let (s, i) = last_item_pos(doc);
            state.cursor_section = s;
            state.cursor_item = i;
        }
        // ── Editing ─────────────────────────────────────────────────────────
        KeyCode::Char(' ') => {
            state.pending_d_at = None;
            // Space (checkbox toggle) is a no-op on Current surfaces — surface
            // items aren't checkboxes and edits would be clobbered by the refresh loop.
            if is_current_surfaces_row(state, doc) {
                return actions;
            }
            if let Some(action) = toggle_checkbox(state, doc) {
                actions.push(action);
            }
        }
        KeyCode::Char('x') => {
            state.pending_d_at = None;
            // Delete is blocked on Current surfaces (refresh loop owns that section).
            if is_current_surfaces_row(state, doc) {
                return actions;
            }
            if let Some(action) = delete_item(state, doc) {
                actions.push(action);
            }
        }
        KeyCode::Char('d') => {
            if let Some(t) = state.pending_d_at {
                if t.elapsed() <= std::time::Duration::from_secs(1) {
                    // Second `d` within window — delete.
                    state.pending_d_at = None;
                    // dd is also blocked on Current surfaces.
                    if !is_current_surfaces_row(state, doc) {
                        if let Some(action) = delete_item(state, doc) {
                            actions.push(action);
                        }
                    }
                    return actions;
                }
            }
            // First `d` (or expired) — start the window.
            state.pending_d_at = Some(std::time::Instant::now());
        }
        KeyCode::Char('o') => {
            state.pending_d_at = None;
            // Insert-below is blocked on Current surfaces.
            if is_current_surfaces_row(state, doc) {
                return actions;
            }
            insert_item_below(state, doc);
        }
        KeyCode::Char('O') => {
            state.pending_d_at = None;
            // Insert-above is blocked on Current surfaces.
            if is_current_surfaces_row(state, doc) {
                return actions;
            }
            insert_item_above(state, doc);
        }
        KeyCode::Char('i') => {
            state.pending_d_at = None;
            // i to enter insert mode is blocked on Current surfaces.
            if is_current_surfaces_row(state, doc) {
                return actions;
            }
            enter_insert_mode(state, doc);
        }
        KeyCode::Enter => {
            state.pending_d_at = None;
            // Enter on Current surfaces is a no-op here; app.rs intercepts it
            // for rows with a surface_id before this function is called.
            if is_current_surfaces_row(state, doc) {
                return actions;
            }
            // On Goals & Progress: Enter opens a new item below (like `o`).
            // On Mission (and any other section): Enter edits the current item (like `i`).
            let on_tasks = doc
                .sections
                .get(state.cursor_section)
                .map(|s| s.name == SECTION_GOALS)
                .unwrap_or(false);
            if on_tasks {
                insert_item_below(state, doc);
            } else {
                enter_insert_mode(state, doc);
            }
        }
        // ── Move item within section ─────────────────────────────────────────
        KeyCode::Char('J') => {
            state.pending_d_at = None;
            // Move-down is blocked on Current surfaces.
            if is_current_surfaces_row(state, doc) {
                return actions;
            }
            if let Some(action) = move_item_down(state, doc) {
                actions.push(action);
            }
        }
        KeyCode::Char('K') => {
            state.pending_d_at = None;
            // Move-up is blocked on Current surfaces.
            if is_current_surfaces_row(state, doc) {
                return actions;
            }
            if let Some(action) = move_item_up(state, doc) {
                actions.push(action);
            }
        }
        // Esc is a no-op in nav mode
        KeyCode::Esc => {
            state.pending_d_at = None;
        }
        _ => {
            state.pending_d_at = None;
        }
    }
    actions
}

fn handle_insert_key(
    state: &mut TrajectoryEditState,
    doc: &mut TrajectoryDoc,
    key: KeyEvent,
) -> Vec<EditAction> {
    let mode_clone = state.mode.clone();
    let focus = match &mode_clone {
        EditMode::Insert { focus } => focus.clone(),
        _ => return vec![],
    };

    match key.code {
        KeyCode::Esc => {
            return commit_insert(state, doc);
        }
        KeyCode::Tab => {
            // Toggle between item buffer and input-ctx buffer
            state.mode = EditMode::Insert {
                focus: match focus {
                    InsertFocus::Item => InsertFocus::InputCtx,
                    InsertFocus::InputCtx => InsertFocus::Item,
                },
            };
        }
        KeyCode::Left => {
            if focus == InsertFocus::Item {
                state.cursor_col = state.cursor_col.saturating_sub(1);
            }
        }
        KeyCode::Right => {
            if focus == InsertFocus::Item {
                let len = state.edit_buffer.chars().count();
                if state.cursor_col < len {
                    state.cursor_col += 1;
                }
            }
        }
        KeyCode::Home => {
            if focus == InsertFocus::Item {
                state.cursor_col = 0;
            }
        }
        KeyCode::End => {
            if focus == InsertFocus::Item {
                state.cursor_col = state.edit_buffer.chars().count();
            }
        }
        KeyCode::Char('a') if key.modifiers == KeyModifiers::CONTROL => {
            if focus == InsertFocus::Item {
                state.cursor_col = 0;
            }
        }
        KeyCode::Char('e') if key.modifiers == KeyModifiers::CONTROL => {
            if focus == InsertFocus::Item {
                state.cursor_col = state.edit_buffer.chars().count();
            }
        }
        KeyCode::Backspace if key.modifiers == KeyModifiers::ALT => {
            if focus == InsertFocus::Item {
                let start = previous_word_start(&state.edit_buffer, state.cursor_col);
                if start < state.cursor_col {
                    let mut chars: Vec<char> = state.edit_buffer.chars().collect();
                    chars.drain(start..state.cursor_col);
                    state.edit_buffer = chars.into_iter().collect();
                    state.cursor_col = start;
                }
            }
        }
        // T4 Part B: Backspace on an empty goal in Goals & Progress collapses
        // the row into the previous one (intuitive Markdown editor behavior).
        // This branch MUST precede the plain-Backspace branch so the
        // empty-goal case wins. Only fires in the Item buffer (Tab-switched
        // input-ctx editing is unrelated).
        KeyCode::Backspace
            if key.modifiers == KeyModifiers::NONE
                && focus == InsertFocus::Item
                && state.edit_buffer.trim().is_empty()
                && doc
                    .sections
                    .get(state.cursor_section)
                    .map(|s| s.name == SECTION_GOALS)
                    .unwrap_or(false)
                && state.cursor_item > 0 =>
        {
            // Remove current item; jump to end of previous item; stay in insert.
            let prev_text = {
                let section = match doc.sections.get_mut(state.cursor_section) {
                    Some(s) => s,
                    None => return vec![],
                };
                if state.cursor_item >= section.items.len() {
                    return vec![];
                }
                section.items.remove(state.cursor_item);
                state.cursor_item -= 1;
                section
                    .items
                    .get(state.cursor_item)
                    .map(|i| i.text.clone())
                    .unwrap_or_default()
            };
            state.edit_buffer = prev_text.clone();
            state.cursor_col = prev_text.chars().count();
            // Keep edit_start_text aligned with the now-current item so the
            // next commit_insert diffs against the right baseline.
            state.edit_start_text = Some(prev_text);
            return vec![];
        }
        KeyCode::Backspace => {
            if focus == InsertFocus::Item {
                if state.cursor_col > 0 {
                    remove_char_at(&mut state.edit_buffer, state.cursor_col - 1);
                    state.cursor_col -= 1;
                }
            } else {
                state.input_ctx_buffer.pop();
            }
        }
        KeyCode::Delete => {
            if focus == InsertFocus::Item {
                let len = state.edit_buffer.chars().count();
                if state.cursor_col < len {
                    remove_char_at(&mut state.edit_buffer, state.cursor_col);
                }
            }
        }
        KeyCode::Char(c)
            if key.modifiers == KeyModifiers::NONE || key.modifiers == KeyModifiers::SHIFT =>
        {
            if focus == InsertFocus::Item {
                insert_char_at(&mut state.edit_buffer, state.cursor_col, c);
                state.cursor_col += 1;
            } else {
                state.input_ctx_buffer.push(c);
            }
        }
        _ => {}
    }
    vec![]
}

/// Insert a character at the given char-indexed position in `s`.
fn insert_char_at(s: &mut String, char_col: usize, c: char) {
    let byte_pos = s
        .char_indices()
        .nth(char_col)
        .map(|(b, _)| b)
        .unwrap_or(s.len());
    s.insert(byte_pos, c);
}

/// Remove the character at the given char-indexed position in `s`.
/// Returns the removed char, or `None` if out of bounds.
fn remove_char_at(s: &mut String, char_col: usize) -> Option<char> {
    let (byte_pos, _) = s.char_indices().nth(char_col)?;
    Some(s.remove(byte_pos))
}

/// Find the start of the word preceding `cursor_col` (in chars).
/// Skips trailing whitespace, then keeps going backward while chars are
/// non-whitespace. Returns the char index of the first char of the word.
fn previous_word_start(s: &str, cursor_col: usize) -> usize {
    let chars: Vec<char> = s.chars().collect();
    if cursor_col == 0 {
        return 0;
    }
    let mut i = cursor_col;
    // Skip whitespace immediately before cursor.
    while i > 0 && chars[i - 1].is_whitespace() {
        i -= 1;
    }
    // Then skip word chars.
    while i > 0 && !chars[i - 1].is_whitespace() {
        i -= 1;
    }
    i
}

// ──────────────────────────────────────────────────────────────────────────────
// Commit / save
// ──────────────────────────────────────────────────────────────────────────────

/// Commit insert mode: write edit_buffer back to doc and produce diff actions.
fn commit_insert(state: &mut TrajectoryEditState, doc: &mut TrajectoryDoc) -> Vec<EditAction> {
    let mut actions = Vec::new();
    let new_text = state.edit_buffer.clone();
    let section_name = doc
        .sections
        .get(state.cursor_section)
        .map(|s| s.name.clone())
        .unwrap_or_default();

    if let Some(section) = doc.sections.get_mut(state.cursor_section) {
        if let Some(item) = section.items.get_mut(state.cursor_item) {
            let old_text = state
                .edit_start_text
                .clone()
                .unwrap_or_else(|| item.text.clone());
            if new_text != old_text {
                // Text genuinely changed — record an Edit action.
                let before = item_display_text(item, &old_text);
                item.text = new_text.clone();
                let after = item_display_text(item, &new_text);
                actions.push(EditAction::Edit {
                    section: section_name,
                    before,
                    after,
                });
            }
            // No-op if text is identical — don't emit events.
        }
    }

    state.mode = EditMode::Nav;
    state.edit_buffer.clear();
    state.cursor_col = 0;
    state.edit_start_text = None;
    actions
}

/// Write the current doc to disk, emit events.jsonl entries, and return snapshot N.
pub fn save(
    uuid: &str,
    doc: &mut TrajectoryDoc,
    state: &TrajectoryEditState,
    edit_actions: &[EditAction],
) -> Result<u32> {
    let n = highest_snapshot(uuid)? + 1;

    // Update frontmatter snapshot number before saving.
    doc.frontmatter.snapshot = Some(n);

    // 1. Overwrite trajectory.md
    let traj_path = paths::trajectory_path(uuid);
    doc.save_to_file(&traj_path)?;

    // 2. Write snapshot
    write_snapshot(uuid, n, doc)?;

    // 3. Write input context
    let ctx = InputContext {
        user_why: if state.input_ctx_buffer.trim().is_empty() {
            None
        } else {
            Some(state.input_ctx_buffer.trim().to_string())
        },
        ..Default::default()
    };
    write_input(uuid, n, &ctx)?;

    // 4. Emit events
    if !edit_actions.is_empty() {
        let events_path = paths::events_log(uuid);
        let user_explanation = ctx.user_why.clone();

        // Build all events first, then attach user_explanation to the last one.
        let mut events: Vec<Event> = edit_actions.iter().map(|a| action_to_event(a, n)).collect();

        // Attach user explanation to most-recent event if non-empty.
        if let Some(ref expl) = user_explanation {
            if let Some(last) = events.last_mut() {
                last.user_explanation = Some(expl.clone());
            }
        }

        for ev in &events {
            crate::mc_data::events::append(&events_path, ev)?;
        }
    }

    Ok(n)
}

fn action_to_event(action: &EditAction, snapshot: u32) -> Event {
    match action {
        EditAction::Edit {
            section,
            before,
            after,
        } => Event::new_now(Source::User, Kind::Edit, section.as_str())
            .with_before(before.as_str())
            .with_after(after.as_str())
            .with_snapshot(snapshot),
        EditAction::Add { section, after } => {
            Event::new_now(Source::User, Kind::Add, section.as_str())
                .with_after(after.as_str())
                .with_snapshot(snapshot)
        }
        EditAction::Delete { section, before } => {
            Event::new_now(Source::User, Kind::Delete, section.as_str())
                .with_before(before.as_str())
                .with_snapshot(snapshot)
        }
        EditAction::Check {
            section,
            before,
            after,
        } => Event::new_now(Source::User, Kind::Check, section.as_str())
            .with_before(before.as_str())
            .with_after(after.as_str())
            .with_snapshot(snapshot),
        EditAction::Uncheck {
            section,
            before,
            after,
        } => Event::new_now(Source::User, Kind::Uncheck, section.as_str())
            .with_before(before.as_str())
            .with_after(after.as_str())
            .with_snapshot(snapshot),
        EditAction::Move { section, before } => {
            Event::new_now(Source::User, Kind::Move, section.as_str())
                .with_before(before.as_str())
                .with_snapshot(snapshot)
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Cursor helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Find the first section index that has items (for `g`).
fn first_non_empty_section(doc: &TrajectoryDoc) -> usize {
    for (i, s) in doc.sections.iter().enumerate() {
        if !s.items.is_empty() {
            return i;
        }
    }
    0
}

/// Find the last item's (section_idx, item_idx) for `G`.
fn last_item_pos(doc: &TrajectoryDoc) -> (usize, usize) {
    for i in (0..doc.sections.len()).rev() {
        if !doc.sections[i].items.is_empty() {
            return (i, doc.sections[i].items.len() - 1);
        }
    }
    (0, 0)
}

fn move_cursor_down(state: &mut TrajectoryEditState, doc: &TrajectoryDoc) {
    let n_sections = doc.sections.len();
    if n_sections == 0 {
        return;
    }
    let cur_sec = &doc.sections[state.cursor_section];

    // If current section is non-empty and there is a next item within it, move there.
    if !cur_sec.items.is_empty() && state.cursor_item + 1 < cur_sec.items.len() {
        state.cursor_item += 1;
        return;
    }

    // We are at (or past) the last item of the current section — advance to the
    // next section.  Land on it even if it is empty (cursor_item = 0 means
    // "cursor on this section's header").
    let sec = state.cursor_section + 1;
    if sec < n_sections {
        state.cursor_section = sec;
        state.cursor_item = 0;
    }
    // Already at the end — stay put.
}

fn move_cursor_up(state: &mut TrajectoryEditState, doc: &TrajectoryDoc) {
    // If inside a non-empty section and not at its first item, move up within it.
    if state.cursor_item > 0 {
        state.cursor_item -= 1;
        return;
    }
    // We are at position 0 of the current section (either on its first item or
    // on an empty section's header). Retreat to the previous section.
    if state.cursor_section == 0 {
        // Already at the top — stay put.
        return;
    }
    let prev = state.cursor_section - 1;
    if doc.sections[prev].items.is_empty() {
        // Previous section is empty: land on its header.
        state.cursor_section = prev;
        state.cursor_item = 0;
    } else {
        // Previous section has items: land on its last item.
        state.cursor_section = prev;
        state.cursor_item = doc.sections[prev].items.len() - 1;
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Item mutations
// ──────────────────────────────────────────────────────────────────────────────

fn toggle_checkbox(state: &mut TrajectoryEditState, doc: &mut TrajectoryDoc) -> Option<EditAction> {
    let section = doc.sections.get_mut(state.cursor_section)?;
    // Only Goals & Progress items have checkboxes (by convention).
    if section.name != SECTION_GOALS {
        return None;
    }
    let item = section.items.get_mut(state.cursor_item)?;
    if !item.is_checkbox {
        return None;
    }
    let currently_checked = item.checked.unwrap_or(false);
    let before = item_display_text(item, &item.text.clone());
    item.checked = Some(!currently_checked);
    let after = item_display_text(item, &item.text.clone());
    Some(if currently_checked {
        EditAction::Uncheck {
            section: section.name.clone(),
            before,
            after,
        }
    } else {
        EditAction::Check {
            section: section.name.clone(),
            before,
            after,
        }
    })
}

fn delete_item(state: &mut TrajectoryEditState, doc: &mut TrajectoryDoc) -> Option<EditAction> {
    let section = doc.sections.get_mut(state.cursor_section)?;
    if section.items.is_empty() {
        return None;
    }
    let item = section.items.remove(state.cursor_item);
    let before = item_display_text(&item, &item.text);
    // Clamp cursor after removal.
    if state.cursor_item > 0 && state.cursor_item >= section.items.len() {
        state.cursor_item -= 1;
    }
    Some(EditAction::Delete {
        section: section.name.clone(),
        before,
    })
}

fn insert_item_below(state: &mut TrajectoryEditState, doc: &mut TrajectoryDoc) {
    let section = match doc.sections.get_mut(state.cursor_section) {
        Some(s) => s,
        None => return,
    };
    let insert_pos = if section.items.is_empty() {
        0
    } else {
        state.cursor_item + 1
    };
    let is_checkbox = section.name == SECTION_GOALS || section.name == SECTION_CURRENT_SURFACES;
    section.items.insert(
        insert_pos,
        Item {
            text: String::new(),
            is_checkbox,
            checked: if is_checkbox { Some(false) } else { None },
            surface_id: None,
        },
    );
    state.cursor_item = insert_pos;
    state.mode = EditMode::Insert {
        focus: InsertFocus::Item,
    };
    state.edit_buffer = String::new();
    state.cursor_col = 0;
    state.input_ctx_buffer = String::new();
    state.edit_start_text = Some(String::new());
}

fn insert_item_above(state: &mut TrajectoryEditState, doc: &mut TrajectoryDoc) {
    let section = match doc.sections.get_mut(state.cursor_section) {
        Some(s) => s,
        None => return,
    };
    let insert_pos = if section.items.is_empty() {
        0
    } else {
        state.cursor_item
    };
    let is_checkbox = section.name == SECTION_GOALS || section.name == SECTION_CURRENT_SURFACES;
    section.items.insert(
        insert_pos,
        Item {
            text: String::new(),
            is_checkbox,
            checked: if is_checkbox { Some(false) } else { None },
            surface_id: None,
        },
    );
    state.cursor_item = insert_pos;
    state.mode = EditMode::Insert {
        focus: InsertFocus::Item,
    };
    state.edit_buffer = String::new();
    state.cursor_col = 0;
    state.input_ctx_buffer = String::new();
    state.edit_start_text = Some(String::new());
}

fn enter_insert_mode(state: &mut TrajectoryEditState, doc: &mut TrajectoryDoc) {
    let section = match doc.sections.get_mut(state.cursor_section) {
        Some(s) => s,
        None => return,
    };

    // Auto-create the first item if section is empty.
    if section.items.is_empty() {
        let is_tasks = section.name == SECTION_GOALS;
        section.items.push(Item {
            text: String::new(),
            is_checkbox: is_tasks,
            checked: if is_tasks { Some(false) } else { None },
            surface_id: None,
        });
        state.cursor_item = 0;
    }

    // Clamp cursor to a valid index if it drifted (e.g. after a deletion).
    if state.cursor_item >= section.items.len() {
        state.cursor_item = section.items.len() - 1;
    }

    let item = &section.items[state.cursor_item];
    state.mode = EditMode::Insert {
        focus: InsertFocus::Item,
    };
    state.edit_buffer = item.text.clone();
    state.cursor_col = state.edit_buffer.chars().count();
    state.input_ctx_buffer = String::new();
    state.edit_start_text = Some(item.text.clone());
}

fn move_item_down(state: &mut TrajectoryEditState, doc: &mut TrajectoryDoc) -> Option<EditAction> {
    let section = doc.sections.get_mut(state.cursor_section)?;
    let idx = state.cursor_item;
    if idx + 1 >= section.items.len() {
        return None;
    }
    let before = item_display_text(&section.items[idx], &section.items[idx].text.clone());
    section.items.swap(idx, idx + 1);
    state.cursor_item += 1;
    Some(EditAction::Move {
        section: section.name.clone(),
        before,
    })
}

fn move_item_up(state: &mut TrajectoryEditState, doc: &mut TrajectoryDoc) -> Option<EditAction> {
    let section = doc.sections.get_mut(state.cursor_section)?;
    let idx = state.cursor_item;
    if idx == 0 {
        return None;
    }
    let before = item_display_text(&section.items[idx], &section.items[idx].text.clone());
    section.items.swap(idx, idx - 1);
    state.cursor_item -= 1;
    Some(EditAction::Move {
        section: section.name.clone(),
        before,
    })
}

// ──────────────────────────────────────────────────────────────────────────────
// Formatting helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Produce the display string for an item (as it would appear in the markdown),
/// using the provided `text` rather than `item.text` so callers can compute
/// before/after with different text.
pub fn item_display_text(item: &Item, text: &str) -> String {
    if item.is_checkbox {
        let mark = if item.checked.unwrap_or(false) {
            "[x]"
        } else {
            "[ ]"
        };
        format!("- {mark} {text}")
    } else {
        format!("- {text}")
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mc_data::trajectory::TrajectoryDoc;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    const SAMPLE: &str = "---
workspace: test-ws
---

## Mission
- Build investment agent

## Current surfaces
- claude · mbp · working

## Goals & Progress
- [x] sprint-01 done
- [ ] sprint-02
- [ ] sprint-03
";

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn shift_key(c: char) -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::SHIFT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn make_doc() -> TrajectoryDoc {
        let mut doc = TrajectoryDoc::parse(SAMPLE).unwrap();
        doc.ensure_sections();
        doc
    }

    // ── Cursor navigation ────────────────────────────────────────────────────

    #[test]
    fn j_moves_within_section() {
        let doc = make_doc();
        let mut state = TrajectoryEditState::default();
        // Start at Goal section (section 0, item 0).
        assert_eq!(state.cursor_section, 0);
        assert_eq!(state.cursor_item, 0);

        handle_key(&mut state, &mut doc.clone(), key(KeyCode::Char('j')));
        // Goal has 1 item, so cursor stays (no next item in section, but next section = Current surfaces with 1 item).
        assert_eq!(state.cursor_section, 1);
        assert_eq!(state.cursor_item, 0);
    }

    #[test]
    fn j_crosses_section_boundary() {
        let doc = make_doc();
        let mut state = TrajectoryEditState {
            cursor_section: 0,
            cursor_item: 0,
            ..Default::default()
        };
        // Goal has 1 item. Moving down should land on Current surfaces item 0.
        handle_key(&mut state, &mut doc.clone(), key(KeyCode::Char('j')));
        assert_eq!(state.cursor_section, 1);
        assert_eq!(state.cursor_item, 0);
    }

    #[test]
    fn j_lands_on_empty_section_header_then_skips_on_next_j() {
        let mut doc = make_doc();
        // Clear Current surfaces items.
        doc.sections[1].items.clear();
        let mut state = TrajectoryEditState {
            cursor_section: 0,
            cursor_item: 0,
            ..Default::default()
        };
        // Goal item 0 → j → land on empty Current surfaces header.
        handle_key(&mut state, &mut doc, key(KeyCode::Char('j')));
        assert_eq!(state.cursor_section, 1);
        assert_eq!(state.cursor_item, 0);
        // j again → Tasks item 0.
        handle_key(&mut state, &mut doc, key(KeyCode::Char('j')));
        assert_eq!(state.cursor_section, 2);
        assert_eq!(state.cursor_item, 0);
    }

    #[test]
    fn k_moves_up_across_sections() {
        let doc = make_doc();
        let mut state = TrajectoryEditState {
            cursor_section: 2,
            cursor_item: 0,
            ..Default::default()
        };
        // Tasks item 0 → up → Current surfaces item 0 (last item).
        handle_key(&mut state, &mut doc.clone(), key(KeyCode::Char('k')));
        assert_eq!(state.cursor_section, 1);
        assert_eq!(state.cursor_item, 0);
    }

    #[test]
    fn g_goes_to_first_non_empty_section() {
        let doc = make_doc();
        let mut state = TrajectoryEditState {
            cursor_section: 2,
            cursor_item: 1,
            ..Default::default()
        };
        handle_key(&mut state, &mut doc.clone(), key(KeyCode::Char('g')));
        assert_eq!(state.cursor_section, 0);
        assert_eq!(state.cursor_item, 0);
    }

    #[test]
    fn big_g_goes_to_last_item() {
        let doc = make_doc();
        let mut state = TrajectoryEditState::default();
        handle_key(&mut state, &mut doc.clone(), shift_key('G'));
        // Tasks has 3 items (sprint-01, sprint-02, sprint-03), last index = 2.
        assert_eq!(state.cursor_section, 2);
        assert_eq!(state.cursor_item, 2);
    }

    // ── Insert mode ──────────────────────────────────────────────────────────

    #[test]
    fn i_enters_insert_mode_with_current_text() {
        let doc = make_doc();
        let mut state = TrajectoryEditState::default(); // Goal, item 0
        handle_key(&mut state, &mut doc.clone(), key(KeyCode::Char('i')));
        assert!(matches!(state.mode, EditMode::Insert { .. }));
        assert_eq!(state.edit_buffer, "Build investment agent");
        assert_eq!(
            state.edit_start_text.as_deref(),
            Some("Build investment agent")
        );
    }

    #[test]
    fn typing_appends_to_edit_buffer() {
        let doc = make_doc();
        let mut state = TrajectoryEditState::default();
        let mut doc_mut = doc.clone();
        handle_key(&mut state, &mut doc_mut, key(KeyCode::Char('i')));
        handle_key(&mut state, &mut doc_mut, key(KeyCode::Char('!')));
        assert_eq!(state.edit_buffer, "Build investment agent!");
    }

    #[test]
    fn esc_commits_edit_and_returns_to_nav() {
        let doc = make_doc();
        let mut state = TrajectoryEditState::default();
        let mut doc_mut = doc.clone();

        // Enter insert, type new text, press Esc.
        handle_key(&mut state, &mut doc_mut, key(KeyCode::Char('i')));
        // Clear buffer and type new text.
        state.edit_buffer = "New goal text".to_string();
        let actions = handle_key(&mut state, &mut doc_mut, key(KeyCode::Esc));

        assert_eq!(state.mode, EditMode::Nav);
        // Doc updated.
        assert_eq!(doc_mut.sections[0].items[0].text, "New goal text");
        // One edit action produced.
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], EditAction::Edit { .. }));
    }

    #[test]
    fn esc_on_unchanged_text_emits_no_action() {
        let doc = make_doc();
        let mut state = TrajectoryEditState::default();
        let mut doc_mut = doc.clone();
        handle_key(&mut state, &mut doc_mut, key(KeyCode::Char('i')));
        // Don't type anything — esc immediately.
        let actions = handle_key(&mut state, &mut doc_mut, key(KeyCode::Esc));
        assert_eq!(actions.len(), 0);
    }

    #[test]
    fn tab_toggles_insert_focus() {
        let doc = make_doc();
        let mut state = TrajectoryEditState::default();
        let mut doc_mut = doc.clone();
        handle_key(&mut state, &mut doc_mut, key(KeyCode::Char('i')));
        assert!(matches!(
            state.mode,
            EditMode::Insert {
                focus: InsertFocus::Item
            }
        ));

        handle_key(&mut state, &mut doc_mut, key(KeyCode::Tab));
        assert!(matches!(
            state.mode,
            EditMode::Insert {
                focus: InsertFocus::InputCtx
            }
        ));

        handle_key(&mut state, &mut doc_mut, key(KeyCode::Tab));
        assert!(matches!(
            state.mode,
            EditMode::Insert {
                focus: InsertFocus::Item
            }
        ));
    }

    #[test]
    fn typing_in_input_ctx_focus_goes_to_input_ctx_buffer() {
        let doc = make_doc();
        let mut state = TrajectoryEditState::default();
        let mut doc_mut = doc.clone();
        handle_key(&mut state, &mut doc_mut, key(KeyCode::Char('i')));
        handle_key(&mut state, &mut doc_mut, key(KeyCode::Tab)); // switch to InputCtx
        handle_key(&mut state, &mut doc_mut, key(KeyCode::Char('w')));
        handle_key(&mut state, &mut doc_mut, key(KeyCode::Char('h')));
        handle_key(&mut state, &mut doc_mut, key(KeyCode::Char('y')));
        assert_eq!(state.input_ctx_buffer, "why");
        // Edit buffer should be untouched.
        assert_eq!(state.edit_buffer, "Build investment agent");
    }

    // ── o / O ────────────────────────────────────────────────────────────────

    #[test]
    fn o_inserts_item_below_cursor_in_insert_mode() {
        let doc = make_doc();
        let mut state = TrajectoryEditState {
            cursor_section: 0,
            cursor_item: 0,
            ..Default::default()
        };
        let mut doc_mut = doc.clone();
        handle_key(&mut state, &mut doc_mut, key(KeyCode::Char('o')));
        // A new empty item should be at index 1 (below cursor).
        assert_eq!(doc_mut.sections[0].items.len(), 2);
        assert_eq!(doc_mut.sections[0].items[1].text, "");
        assert_eq!(state.cursor_item, 1);
        assert!(matches!(state.mode, EditMode::Insert { .. }));
    }

    #[test]
    fn big_o_inserts_item_above_cursor() {
        let doc = make_doc();
        let mut state = TrajectoryEditState {
            cursor_section: 0,
            cursor_item: 0,
            ..Default::default()
        };
        let mut doc_mut = doc.clone();
        handle_key(&mut state, &mut doc_mut, shift_key('O'));
        assert_eq!(doc_mut.sections[0].items.len(), 2);
        assert_eq!(doc_mut.sections[0].items[0].text, "");
        assert_eq!(state.cursor_item, 0);
    }

    #[test]
    fn o_then_type_then_esc_emits_add_action() {
        let doc = make_doc();
        let mut state = TrajectoryEditState {
            cursor_section: 0,
            cursor_item: 0,
            ..Default::default()
        };
        let mut doc_mut = doc.clone();
        handle_key(&mut state, &mut doc_mut, key(KeyCode::Char('o')));
        state.edit_buffer = "New goal item".to_string();
        let actions = handle_key(&mut state, &mut doc_mut, key(KeyCode::Esc));
        // edit_start_text is "", new text is "New goal item" → an Edit action
        // (the item was created with "" and changed to "New goal item").
        // Actually it starts as "" via insert_item_below, so we get an Edit action.
        assert!(!actions.is_empty());
    }

    // ── x (delete) ───────────────────────────────────────────────────────────

    #[test]
    fn x_deletes_current_item_and_emits_delete() {
        let doc = make_doc();
        let mut state = TrajectoryEditState {
            cursor_section: 2, // Tasks
            cursor_item: 0,
            ..Default::default()
        };
        let mut doc_mut = doc.clone();
        let original_text = doc_mut.sections[2].items[0].text.clone();
        let actions = handle_key(&mut state, &mut doc_mut, key(KeyCode::Char('x')));

        assert_eq!(doc_mut.sections[2].items.len(), 2); // was 3, now 2
        assert_eq!(actions.len(), 1);
        if let EditAction::Delete { before, .. } = &actions[0] {
            assert!(before.contains(&original_text));
        } else {
            panic!("expected Delete action, got {:?}", actions[0]);
        }
    }

    // ── Space (toggle) ───────────────────────────────────────────────────────

    #[test]
    fn space_toggles_unchecked_to_checked_in_tasks() {
        let doc = make_doc();
        let mut state = TrajectoryEditState {
            cursor_section: 2,
            cursor_item: 1, // sprint-02 (unchecked)
            ..Default::default()
        };
        let mut doc_mut = doc.clone();
        let actions = handle_key(&mut state, &mut doc_mut, key(KeyCode::Char(' ')));

        assert_eq!(doc_mut.sections[2].items[1].checked, Some(true));
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], EditAction::Check { .. }));
    }

    #[test]
    fn space_toggles_checked_to_unchecked() {
        let doc = make_doc();
        let mut state = TrajectoryEditState {
            cursor_section: 2,
            cursor_item: 0, // sprint-01 (checked)
            ..Default::default()
        };
        let mut doc_mut = doc.clone();
        let actions = handle_key(&mut state, &mut doc_mut, key(KeyCode::Char(' ')));

        assert_eq!(doc_mut.sections[2].items[0].checked, Some(false));
        assert!(matches!(actions[0], EditAction::Uncheck { .. }));
    }

    #[test]
    fn space_noop_in_goal_section() {
        let doc = make_doc();
        let mut state = TrajectoryEditState::default(); // Goal section
        let mut doc_mut = doc.clone();
        let actions = handle_key(&mut state, &mut doc_mut, key(KeyCode::Char(' ')));
        assert!(actions.is_empty());
    }

    // ── Empty-section auto-create (i / o / O) ────────────────────────────────

    #[test]
    fn pressing_i_on_empty_goal_section_creates_first_item_and_enters_insert() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections(); // all 3 sections empty
        let mut state = TrajectoryEditState::default();
        state.cursor_section = 0; // Mission
        state.cursor_item = 0;
        let actions = handle_key(&mut state, &mut doc, key(KeyCode::Char('i')));
        assert!(matches!(state.mode, EditMode::Insert { .. }));
        let goal = doc.section("Mission").unwrap();
        assert_eq!(goal.items.len(), 1);
        assert_eq!(goal.items[0].text, "");
        assert!(!goal.items[0].is_checkbox);
        // No action yet — actions fire on Esc-commit.
        assert!(actions.is_empty());
    }

    #[test]
    fn pressing_i_on_empty_tasks_section_creates_a_checkbox_item() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        let mut state = TrajectoryEditState::default();
        // section index for "Goals & Progress" is 2 in canonical order
        state.cursor_section = 2;
        state.cursor_item = 0;
        handle_key(&mut state, &mut doc, key(KeyCode::Char('i')));
        let tasks = doc.section("Goals & Progress").unwrap();
        assert_eq!(tasks.items.len(), 1);
        assert!(tasks.items[0].is_checkbox);
        assert_eq!(tasks.items[0].checked, Some(false));
    }

    #[test]
    fn pressing_o_on_empty_section_creates_first_item() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        let mut state = TrajectoryEditState::default();
        state.cursor_section = 0;
        state.cursor_item = 0;
        handle_key(&mut state, &mut doc, key(KeyCode::Char('o')));
        let goal = doc.section("Mission").unwrap();
        assert_eq!(goal.items.len(), 1);
        assert!(matches!(state.mode, EditMode::Insert { .. }));
    }

    #[test]
    fn cursor_clamps_to_last_item_when_out_of_bounds() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        // Manually populate Goal with 2 items.
        let goal = &mut doc.sections[0];
        goal.items.push(Item {
            text: "first".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: None,
        });
        goal.items.push(Item {
            text: "second".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: None,
        });
        let mut state = TrajectoryEditState::default();
        state.cursor_section = 0;
        state.cursor_item = 99; // out of bounds
        handle_key(&mut state, &mut doc, key(KeyCode::Char('i')));
        // Should have clamped to index 1 (last item, "second").
        assert_eq!(state.cursor_item, 1);
        if let EditMode::Insert { .. } = state.mode {
        } else {
            panic!("not in insert mode");
        }
        assert_eq!(state.edit_buffer, "second");
    }

    // ── T8: within-line cursor in insert mode ────────────────────────────────

    #[test]
    fn arrow_left_moves_cursor_within_buffer() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        doc.sections[0].items.push(Item {
            text: "hello".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: None,
        });
        let mut state = TrajectoryEditState::default();
        state.cursor_section = 0;
        state.cursor_item = 0;
        handle_key(&mut state, &mut doc, key(KeyCode::Char('i'))); // enter insert
        assert_eq!(state.cursor_col, 5); // end of "hello"
        handle_key(&mut state, &mut doc, key(KeyCode::Left));
        assert_eq!(state.cursor_col, 4);
    }

    #[test]
    fn arrow_right_clamps_to_buffer_length() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        doc.sections[0].items.push(Item {
            text: "hi".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: None,
        });
        let mut state = TrajectoryEditState::default();
        state.cursor_section = 0;
        state.cursor_item = 0;
        handle_key(&mut state, &mut doc, key(KeyCode::Char('i')));
        assert_eq!(state.cursor_col, 2); // end of "hi"
        // Right at the end should stay at 2
        handle_key(&mut state, &mut doc, key(KeyCode::Right));
        assert_eq!(state.cursor_col, 2);
    }

    #[test]
    fn home_jumps_to_zero_end_jumps_to_len() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        doc.sections[0].items.push(Item {
            text: "abcde".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: None,
        });
        let mut state = TrajectoryEditState::default();
        state.cursor_section = 0;
        state.cursor_item = 0;
        handle_key(&mut state, &mut doc, key(KeyCode::Char('i')));
        assert_eq!(state.cursor_col, 5);
        handle_key(&mut state, &mut doc, key(KeyCode::Home));
        assert_eq!(state.cursor_col, 0);
        handle_key(&mut state, &mut doc, key(KeyCode::End));
        assert_eq!(state.cursor_col, 5);
    }

    #[test]
    fn typing_inserts_at_cursor_position() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        doc.sections[0].items.push(Item {
            text: "ac".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: None,
        });
        let mut state = TrajectoryEditState::default();
        state.cursor_section = 0;
        state.cursor_item = 0;
        handle_key(&mut state, &mut doc, key(KeyCode::Char('i')));
        // cursor is at 2 (end of "ac")
        handle_key(&mut state, &mut doc, key(KeyCode::Left)); // cursor at 1
        handle_key(&mut state, &mut doc, key(KeyCode::Char('b'))); // insert 'b' at col 1
        assert_eq!(state.edit_buffer, "abc");
        assert_eq!(state.cursor_col, 2);
    }

    #[test]
    fn backspace_removes_char_before_cursor() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        doc.sections[0].items.push(Item {
            text: "abc".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: None,
        });
        let mut state = TrajectoryEditState::default();
        state.cursor_section = 0;
        state.cursor_item = 0;
        handle_key(&mut state, &mut doc, key(KeyCode::Char('i')));
        // cursor at 3 (end of "abc")
        handle_key(&mut state, &mut doc, key(KeyCode::Left)); // cursor at 2
        handle_key(&mut state, &mut doc, key(KeyCode::Backspace)); // removes 'b' (char at col 1)
        assert_eq!(state.edit_buffer, "ac");
        assert_eq!(state.cursor_col, 1);
    }

    #[test]
    fn delete_removes_char_at_cursor() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        doc.sections[0].items.push(Item {
            text: "abc".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: None,
        });
        let mut state = TrajectoryEditState::default();
        state.cursor_section = 0;
        state.cursor_item = 0;
        handle_key(&mut state, &mut doc, key(KeyCode::Char('i')));
        // cursor at 3
        handle_key(&mut state, &mut doc, key(KeyCode::Left)); // cursor at 2
        handle_key(&mut state, &mut doc, key(KeyCode::Left)); // cursor at 1
        handle_key(&mut state, &mut doc, key(KeyCode::Delete)); // removes 'b' at col 1
        assert_eq!(state.edit_buffer, "ac");
        assert_eq!(state.cursor_col, 1); // cursor didn't move
    }

    #[test]
    fn cursor_handles_utf8_chars() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        doc.sections[0].items.push(Item {
            text: String::new(),
            is_checkbox: false,
            checked: None,
            surface_id: None,
        });
        let mut state = TrajectoryEditState::default();
        state.cursor_section = 0;
        state.cursor_item = 0;
        handle_key(&mut state, &mut doc, key(KeyCode::Char('i')));
        handle_key(&mut state, &mut doc, key(KeyCode::Char('ä')));
        // 'ä' is 2 bytes but 1 char — cursor_col should be 1
        assert_eq!(state.cursor_col, 1);
        assert_eq!(state.edit_buffer, "ä");
        // Verify edit_buffer byte length is > 1 (it's multi-byte UTF-8)
        assert!(state.edit_buffer.len() > 1);
    }

    // ── T9: `dd` two-key sequence ────────────────────────────────────────────

    #[test]
    fn dd_within_one_second_deletes_item() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        doc.sections[0].items.push(Item {
            text: "kill me".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: None,
        });
        let mut state = TrajectoryEditState::default();
        handle_key(&mut state, &mut doc, key(KeyCode::Char('d')));
        assert!(state.pending_d_at.is_some());
        assert_eq!(doc.sections[0].items.len(), 1, "first d should NOT delete");
        let actions = handle_key(&mut state, &mut doc, key(KeyCode::Char('d')));
        assert_eq!(doc.sections[0].items.len(), 0);
        assert!(!actions.is_empty()); // delete action emitted
        assert!(state.pending_d_at.is_none());
    }

    #[test]
    fn single_d_does_nothing_destructive() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        doc.sections[0].items.push(Item {
            text: "keep me".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: None,
        });
        let mut state = TrajectoryEditState::default();
        handle_key(&mut state, &mut doc, key(KeyCode::Char('d')));
        assert_eq!(doc.sections[0].items.len(), 1, "single d should NOT delete");
    }

    #[test]
    fn d_followed_by_non_d_clears_pending() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        doc.sections[0].items.push(Item {
            text: "x".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: None,
        });
        let mut state = TrajectoryEditState::default();
        handle_key(&mut state, &mut doc, key(KeyCode::Char('d')));
        handle_key(&mut state, &mut doc, key(KeyCode::Char('j'))); // any other key
        assert!(
            state.pending_d_at.is_none(),
            "d-pending should clear on non-d"
        );
    }

    #[test]
    fn x_still_deletes_in_one_keypress() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        doc.sections[0].items.push(Item {
            text: "x".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: None,
        });
        let mut state = TrajectoryEditState::default();
        handle_key(&mut state, &mut doc, key(KeyCode::Char('x')));
        assert_eq!(doc.sections[0].items.len(), 0);
    }

    // ── Part 1: block edit keys on Current surfaces ──────────────────────────

    #[test]
    fn i_is_blocked_on_current_surfaces_row() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        // Add a fake surface row.
        doc.sections[1].items.push(Item {
            text: "claude · mbp · working · stuff".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: Some("sid-1".to_string()),
        });
        let mut state = TrajectoryEditState::default();
        state.cursor_section = 1; // Current surfaces
        state.cursor_item = 0;
        let actions = handle_key(&mut state, &mut doc, key(KeyCode::Char('i')));
        assert!(actions.is_empty());
        assert!(
            !matches!(state.mode, EditMode::Insert { .. }),
            "i must NOT enter insert mode on Current surfaces"
        );
    }

    #[test]
    fn x_is_blocked_on_current_surfaces_row() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        doc.sections[1].items.push(Item {
            text: "shell · mbp · idle".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: Some("sid-1".to_string()),
        });
        let mut state = TrajectoryEditState::default();
        state.cursor_section = 1;
        state.cursor_item = 0;
        handle_key(&mut state, &mut doc, key(KeyCode::Char('x')));
        // Item must still be there.
        assert_eq!(doc.sections[1].items.len(), 1);
    }

    #[test]
    fn dd_is_blocked_on_current_surfaces_row() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        doc.sections[1].items.push(Item {
            text: "shell · mbp · idle".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: Some("sid-2".to_string()),
        });
        let mut state = TrajectoryEditState::default();
        state.cursor_section = 1;
        state.cursor_item = 0;
        handle_key(&mut state, &mut doc, key(KeyCode::Char('d')));
        let actions = handle_key(&mut state, &mut doc, key(KeyCode::Char('d')));
        // Item must still be there; no delete action.
        assert_eq!(doc.sections[1].items.len(), 1);
        assert!(actions.is_empty());
    }

    #[test]
    fn o_is_blocked_on_current_surfaces_row() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        doc.sections[1].items.push(Item {
            text: "surface".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: Some("sid-3".to_string()),
        });
        let mut state = TrajectoryEditState::default();
        state.cursor_section = 1;
        state.cursor_item = 0;
        handle_key(&mut state, &mut doc, key(KeyCode::Char('o')));
        // No new item inserted; still in nav mode.
        assert_eq!(doc.sections[1].items.len(), 1);
        assert!(matches!(state.mode, EditMode::Nav));
    }

    #[test]
    fn i_still_works_on_goal_section() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        let mut state = TrajectoryEditState::default();
        state.cursor_section = 0; // Goal
        state.cursor_item = 0;
        handle_key(&mut state, &mut doc, key(KeyCode::Char('i')));
        assert!(matches!(state.mode, EditMode::Insert { .. }));
    }

    #[test]
    fn i_still_works_on_tasks_section() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        let mut state = TrajectoryEditState::default();
        state.cursor_section = 2; // Goals & Progress
        state.cursor_item = 0;
        handle_key(&mut state, &mut doc, key(KeyCode::Char('i')));
        assert!(matches!(state.mode, EditMode::Insert { .. }));
    }

    #[test]
    fn cursor_movement_still_works_on_current_surfaces() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        doc.sections[1].items.push(Item {
            text: "a".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: Some("s1".to_string()),
        });
        doc.sections[1].items.push(Item {
            text: "b".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: Some("s2".to_string()),
        });
        let mut state = TrajectoryEditState::default();
        state.cursor_section = 1;
        state.cursor_item = 0;
        handle_key(&mut state, &mut doc, key(KeyCode::Char('j')));
        assert_eq!(
            state.cursor_item, 1,
            "j must still move cursor on Current surfaces"
        );
    }

    // ── T-phase3: empty-section cursor navigation ────────────────────────────

    #[test]
    fn j_traverses_empty_sections() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections(); // all 3 sections present, all empty
        let mut state = TrajectoryEditState::default();
        state.cursor_section = 0; // Goal
        state.cursor_item = 0;
        // j → Current surfaces (still cursor_item = 0)
        handle_key(&mut state, &mut doc, key(KeyCode::Char('j')));
        assert_eq!(state.cursor_section, 1);
        assert_eq!(state.cursor_item, 0);
        // j → Goals & Progress
        handle_key(&mut state, &mut doc, key(KeyCode::Char('j')));
        assert_eq!(state.cursor_section, 2);
        // j again clamps at last section
        handle_key(&mut state, &mut doc, key(KeyCode::Char('j')));
        assert_eq!(state.cursor_section, 2);
    }

    #[test]
    fn k_traverses_empty_sections_backward() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        let mut state = TrajectoryEditState::default();
        state.cursor_section = 2;
        state.cursor_item = 0;
        handle_key(&mut state, &mut doc, key(KeyCode::Char('k')));
        assert_eq!(state.cursor_section, 1);
        handle_key(&mut state, &mut doc, key(KeyCode::Char('k')));
        assert_eq!(state.cursor_section, 0);
        // clamp at top
        handle_key(&mut state, &mut doc, key(KeyCode::Char('k')));
        assert_eq!(state.cursor_section, 0);
    }

    #[test]
    fn j_from_last_item_of_goal_lands_on_empty_current_surfaces_header() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        doc.sections[0].items.push(Item {
            text: "g1".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: None,
        });
        let mut state = TrajectoryEditState::default();
        state.cursor_section = 0;
        state.cursor_item = 0;
        handle_key(&mut state, &mut doc, key(KeyCode::Char('j')));
        // Current surfaces is empty → cursor lands there with item=0
        assert_eq!(state.cursor_section, 1);
        assert_eq!(state.cursor_item, 0);
    }

    #[test]
    fn k_from_first_item_of_tasks_lands_on_empty_current_surfaces_header() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        doc.sections[2].items.push(Item {
            text: "t1".to_string(),
            is_checkbox: true,
            checked: Some(false),
            surface_id: None,
        });
        let mut state = TrajectoryEditState::default();
        state.cursor_section = 2;
        state.cursor_item = 0;
        handle_key(&mut state, &mut doc, key(KeyCode::Char('k')));
        assert_eq!(state.cursor_section, 1);
        assert_eq!(state.cursor_item, 0);
    }

    #[test]
    fn i_on_empty_goal_header_after_j_navigation_still_creates_first_item() {
        // Verify the existing T3 logic still kicks in.
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        let mut state = TrajectoryEditState::default();
        state.cursor_section = 0;
        state.cursor_item = 0;
        handle_key(&mut state, &mut doc, key(KeyCode::Char('i')));
        assert!(matches!(state.mode, EditMode::Insert { .. }));
        assert_eq!(doc.sections[0].items.len(), 1);
    }

    // ── Part 2 helpers: description ↔ Goal round-trip ────────────────────────

    #[test]
    fn goal_items_render_to_multi_line_description() {
        let items = vec![
            Item {
                text: "first goal".to_string(),
                is_checkbox: false,
                checked: None,
                surface_id: None,
            },
            Item {
                text: "second refinement".to_string(),
                is_checkbox: false,
                checked: None,
                surface_id: None,
            },
        ];
        let desc = items
            .iter()
            .map(|i| i.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(desc, "first goal\nsecond refinement");
    }

    #[test]
    fn description_parses_to_goal_items() {
        let desc = "first\nsecond\n\nthird";
        let items: Vec<String> = desc
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.trim().to_string())
            .collect();
        assert_eq!(items, vec!["first", "second", "third"]);
    }

    // ── Alt+Backspace (Option+Delete) ─────────────────────────────────────────

    #[test]
    fn alt_backspace_deletes_previous_word() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        doc.sections[0].items.push(Item {
            text: "hello world how are you".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: None,
        });
        let mut state = TrajectoryEditState::default();
        state.cursor_section = 0;
        state.cursor_item = 0;
        handle_key(&mut state, &mut doc, key(KeyCode::Char('i')));
        // cursor_col now at chars().count() == 23 (end of "hello world how are you")
        assert_eq!(state.cursor_col, 23);
        let ev = crossterm::event::KeyEvent::new(KeyCode::Backspace, crossterm::event::KeyModifiers::ALT);
        handle_key(&mut state, &mut doc, ev);
        // Should have deleted "you" — "hello world how are " remains
        assert!(state.edit_buffer.ends_with("are "), "got: {:?}", state.edit_buffer);
        assert_eq!(state.cursor_col, state.edit_buffer.chars().count());
    }

    #[test]
    fn alt_backspace_at_col_zero_is_noop() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        doc.sections[0].items.push(Item {
            text: "hello".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: None,
        });
        let mut state = TrajectoryEditState::default();
        handle_key(&mut state, &mut doc, key(KeyCode::Char('i')));
        state.cursor_col = 0;
        let ev = crossterm::event::KeyEvent::new(KeyCode::Backspace, crossterm::event::KeyModifiers::ALT);
        handle_key(&mut state, &mut doc, ev);
        assert_eq!(state.edit_buffer, "hello");
        assert_eq!(state.cursor_col, 0);
    }

    // ── Ctrl+A / Ctrl+E ───────────────────────────────────────────────────────

    #[test]
    fn ctrl_a_jumps_to_line_head() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        doc.sections[0].items.push(Item {
            text: "abc".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: None,
        });
        let mut state = TrajectoryEditState::default();
        handle_key(&mut state, &mut doc, key(KeyCode::Char('i')));
        // cursor at end (3)
        assert_eq!(state.cursor_col, 3);
        let ev = crossterm::event::KeyEvent::new(KeyCode::Char('a'), crossterm::event::KeyModifiers::CONTROL);
        handle_key(&mut state, &mut doc, ev);
        assert_eq!(state.cursor_col, 0);
    }

    #[test]
    fn ctrl_e_jumps_to_line_tail() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        doc.sections[0].items.push(Item {
            text: "abc".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: None,
        });
        let mut state = TrajectoryEditState::default();
        handle_key(&mut state, &mut doc, key(KeyCode::Char('i')));
        state.cursor_col = 0; // jump to head manually
        let ev = crossterm::event::KeyEvent::new(KeyCode::Char('e'), crossterm::event::KeyModifiers::CONTROL);
        handle_key(&mut state, &mut doc, ev);
        assert_eq!(state.cursor_col, 3);
    }

    // ── Enter on Tasks / Goal sections ────────────────────────────────────────

    #[test]
    fn enter_on_tasks_section_creates_new_item_and_enters_insert() {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        // Add one task so the section isn't empty.
        doc.sections[2].items.push(Item {
            text: "first".to_string(),
            is_checkbox: true,
            checked: Some(false),
            surface_id: None,
        });
        let mut state = TrajectoryEditState::default();
        state.cursor_section = 2; // Goals & Progress
        state.cursor_item = 0;
        handle_key(&mut state, &mut doc, key(KeyCode::Enter));
        // A new item should exist below "first".
        assert_eq!(doc.sections[2].items.len(), 2);
        assert!(matches!(state.mode, EditMode::Insert { .. }));
        assert_eq!(state.cursor_item, 1); // cursor moved to new item
    }

    #[test]
    fn enter_on_goal_section_edits_current_item() {
        // Confirm we did NOT break Goal section's Enter behavior.
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        doc.sections[0].items.push(Item {
            text: "goal item".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: None,
        });
        let mut state = TrajectoryEditState::default();
        state.cursor_section = 0;
        state.cursor_item = 0;
        handle_key(&mut state, &mut doc, key(KeyCode::Enter));
        // Goal still has 1 item (we entered insert on the existing one, didn't create new).
        assert_eq!(doc.sections[0].items.len(), 1);
        assert!(matches!(state.mode, EditMode::Insert { .. }));
    }

    // ── T4 Part B: Backspace deletes empty goal in insert mode ───────────────

    /// Build a doc with N goal items, the last one empty, cursor on the last.
    fn doc_with_goals(items: &[&str]) -> TrajectoryDoc {
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        // Goals & Progress is section 2 after ensure_sections.
        for t in items {
            doc.sections[2].items.push(Item {
                text: t.to_string(),
                is_checkbox: true,
                checked: Some(false),
                surface_id: None,
            });
        }
        doc
    }

    #[test]
    fn backspace_on_empty_goal_deletes_and_jumps_to_previous() {
        let mut doc = doc_with_goals(&["sprint-01", "sprint-02", ""]);
        // Enter insert mode on the empty 3rd item.
        let mut state = TrajectoryEditState::default();
        state.cursor_section = 2;
        state.cursor_item = 2;
        handle_key(&mut state, &mut doc, key(KeyCode::Char('i')));
        assert!(matches!(state.mode, EditMode::Insert { .. }));
        assert_eq!(state.edit_buffer, "");

        // Backspace: empty current goal + previous exists → collapse.
        handle_key(&mut state, &mut doc, key(KeyCode::Backspace));

        // The empty item is removed.
        assert_eq!(doc.sections[2].items.len(), 2);
        // Cursor moved to previous item (index 1, "sprint-02").
        assert_eq!(state.cursor_item, 1);
        // Edit buffer holds the previous item's text.
        assert_eq!(state.edit_buffer, "sprint-02");
        // Cursor is at end of previous text.
        assert_eq!(state.cursor_col, "sprint-02".chars().count());
        // Still in insert mode.
        assert!(matches!(state.mode, EditMode::Insert { .. }));
    }

    #[test]
    fn backspace_on_empty_goal_first_item_is_noop() {
        let mut doc = doc_with_goals(&[""]);
        let mut state = TrajectoryEditState::default();
        state.cursor_section = 2;
        state.cursor_item = 0;
        handle_key(&mut state, &mut doc, key(KeyCode::Char('i')));
        assert!(matches!(state.mode, EditMode::Insert { .. }));
        assert_eq!(state.edit_buffer, "");

        handle_key(&mut state, &mut doc, key(KeyCode::Backspace));

        // Item still exists; nothing changed.
        assert_eq!(doc.sections[2].items.len(), 1);
        assert_eq!(state.cursor_item, 0);
        assert_eq!(state.edit_buffer, "");
        // Still in insert mode.
        assert!(matches!(state.mode, EditMode::Insert { .. }));
    }

    #[test]
    fn backspace_on_goal_with_content_acts_as_char_delete() {
        // Existing-behavior regression test: Backspace on a non-empty buffer
        // should still delete the previous character.
        let mut doc = doc_with_goals(&["sprint-01", "abc"]);
        let mut state = TrajectoryEditState::default();
        state.cursor_section = 2;
        state.cursor_item = 1;
        handle_key(&mut state, &mut doc, key(KeyCode::Char('i')));
        assert_eq!(state.edit_buffer, "abc");
        // Move cursor to end.
        state.cursor_col = 3;

        handle_key(&mut state, &mut doc, key(KeyCode::Backspace));

        // Buffer trimmed by one char; no item removed.
        assert_eq!(state.edit_buffer, "ab");
        assert_eq!(state.cursor_col, 2);
        assert_eq!(doc.sections[2].items.len(), 2);
        assert!(matches!(state.mode, EditMode::Insert { .. }));
    }

    #[test]
    fn backspace_on_empty_non_goal_section_is_plain_char_delete() {
        // Mission section (index 0): empty buffer + Backspace should NOT
        // delete the item — the special-case only fires in Goals & Progress.
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections();
        doc.sections[0].items.push(Item {
            text: "first mission".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: None,
        });
        doc.sections[0].items.push(Item {
            text: "".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: None,
        });
        let mut state = TrajectoryEditState::default();
        state.cursor_section = 0;
        state.cursor_item = 1;
        handle_key(&mut state, &mut doc, key(KeyCode::Char('i')));
        assert_eq!(state.edit_buffer, "");
        handle_key(&mut state, &mut doc, key(KeyCode::Backspace));
        // Item was NOT removed (Mission isn't Goals & Progress).
        assert_eq!(doc.sections[0].items.len(), 2);
        assert_eq!(state.cursor_item, 1);
    }
}
