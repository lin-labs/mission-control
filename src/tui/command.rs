//! State machine for mc's vim-like `:command` bar.
//!
//! The active state lives in `App::input_mode`. This module owns:
//!  - `InputMode` (Normal vs Command)
//!  - `CommandLine` (buffer + cursor + status)
//!  - `StatusLine` (Ok / Err)
//!  - the editing primitives the event loop calls into

use crate::commands::{longest_common_prefix, matches};

#[derive(Debug, Clone)]
pub enum InputMode {
    Normal,
    Command(CommandLine),
}

impl Default for InputMode {
    fn default() -> Self {
        InputMode::Normal
    }
}

#[derive(Debug, Clone, Default)]
pub struct CommandLine {
    pub buffer: String,
    pub cursor: usize,
    pub status: Option<StatusLine>,
    /// True after one Tab when there were ≥2 matches; the next Tab shows the
    /// match list inline. Reset by any non-Tab edit.
    pub tab_armed: bool,
}

#[derive(Debug, Clone)]
pub enum StatusLine {
    Ok(String),
    Err(String),
}

impl CommandLine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_char(&mut self, c: char) {
        self.buffer.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        self.tab_armed = false;
        self.status = None;
    }

    /// Delete the char before the cursor. Returns true if a char was deleted,
    /// false if the buffer was empty (caller should exit Command mode).
    pub fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        // Find previous char boundary.
        let mut prev = self.cursor - 1;
        while !self.buffer.is_char_boundary(prev) {
            prev -= 1;
        }
        self.buffer.drain(prev..self.cursor);
        self.cursor = prev;
        self.tab_armed = false;
        self.status = None;
        true
    }

    pub fn cursor_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let mut prev = self.cursor - 1;
        while !self.buffer.is_char_boundary(prev) {
            prev -= 1;
        }
        self.cursor = prev;
    }

    pub fn cursor_right(&mut self) {
        if self.cursor >= self.buffer.len() {
            return;
        }
        let mut next = self.cursor + 1;
        while next < self.buffer.len() && !self.buffer.is_char_boundary(next) {
            next += 1;
        }
        self.cursor = next;
    }

    pub fn cursor_home(&mut self) {
        self.cursor = 0;
    }

    pub fn cursor_end(&mut self) {
        self.cursor = self.buffer.len();
    }

    /// Result of one Tab press.
    pub fn tab(&mut self) -> TabOutcome {
        // Only complete the first token (the command name).
        let token: &str = self.buffer.split_whitespace().next().unwrap_or("");
        // If user typed past the command name (a space already), do nothing.
        if self.buffer.contains(' ') && !token.is_empty() {
            return TabOutcome::NoOp;
        }
        let hits = matches(token);
        match hits.len() {
            0 => {
                self.status = Some(StatusLine::Err("no match".to_string()));
                TabOutcome::NoMatch
            }
            1 => {
                let full = hits[0];
                self.buffer = format!("{} ", full);
                self.cursor = self.buffer.len();
                self.tab_armed = false;
                TabOutcome::Completed(full)
            }
            _ => {
                let lcp = longest_common_prefix(&hits);
                let extended = lcp.len() > token.len();
                if extended {
                    self.buffer = lcp.clone();
                    self.cursor = self.buffer.len();
                    self.tab_armed = true;
                    TabOutcome::ExtendedPrefix(lcp)
                } else if self.tab_armed {
                    // Second consecutive Tab with no further extension — show list.
                    self.status = Some(StatusLine::Ok(hits.join("  ")));
                    self.tab_armed = false;
                    TabOutcome::ShowMatches(hits)
                } else {
                    self.tab_armed = true;
                    TabOutcome::ExtendedPrefix(lcp)
                }
            }
        }
    }

    /// The dim "ghost" suffix shown after the cursor: when the buffer (its
    /// first token) is a strict prefix of exactly one command, the remaining
    /// characters; otherwise empty.
    pub fn ghost(&self) -> &'static str {
        let token: &str = self.buffer.split_whitespace().next().unwrap_or("");
        if self.buffer.contains(' ') {
            return "";
        }
        let hits = matches(token);
        if hits.len() != 1 {
            return "";
        }
        let full = hits[0];
        if full.len() <= token.len() {
            return "";
        }
        &full[token.len()..]
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TabOutcome {
    NoOp,
    NoMatch,
    Completed(&'static str),
    ExtendedPrefix(String),
    ShowMatches(Vec<&'static str>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_backspace() {
        let mut cl = CommandLine::new();
        cl.insert_char('s');
        cl.insert_char('u');
        cl.insert_char('m');
        assert_eq!(cl.buffer, "sum");
        assert_eq!(cl.cursor, 3);
        assert!(cl.backspace());
        assert_eq!(cl.buffer, "su");
        assert_eq!(cl.cursor, 2);
    }

    #[test]
    fn backspace_on_empty_returns_false() {
        let mut cl = CommandLine::new();
        assert!(!cl.backspace());
    }

    #[test]
    fn cursor_moves() {
        let mut cl = CommandLine::new();
        for c in "summ".chars() {
            cl.insert_char(c);
        }
        cl.cursor_left();
        assert_eq!(cl.cursor, 3);
        cl.cursor_home();
        assert_eq!(cl.cursor, 0);
        cl.cursor_end();
        assert_eq!(cl.cursor, 4);
        cl.cursor_right();
        assert_eq!(cl.cursor, 4);
    }

    #[test]
    fn tab_unique_match_completes_with_space() {
        let mut cl = CommandLine::new();
        for c in "sum".chars() {
            cl.insert_char(c);
        }
        let outcome = cl.tab();
        assert_eq!(outcome, TabOutcome::Completed("summarize"));
        assert_eq!(cl.buffer, "summarize ");
        assert_eq!(cl.cursor, 10);
    }

    #[test]
    fn tab_no_match_sets_error_status() {
        let mut cl = CommandLine::new();
        for c in "zzz".chars() {
            cl.insert_char(c);
        }
        let outcome = cl.tab();
        assert_eq!(outcome, TabOutcome::NoMatch);
        assert!(matches!(cl.status, Some(StatusLine::Err(_))));
    }

    #[test]
    fn ghost_when_unique_prefix() {
        let mut cl = CommandLine::new();
        for c in "sum".chars() {
            cl.insert_char(c);
        }
        assert_eq!(cl.ghost(), "marize");
    }

    #[test]
    fn ghost_empty_when_no_match() {
        let mut cl = CommandLine::new();
        for c in "zzz".chars() {
            cl.insert_char(c);
        }
        assert_eq!(cl.ghost(), "");
    }

    #[test]
    fn ghost_empty_after_space() {
        let mut cl = CommandLine::new();
        for c in "summarize ".chars() {
            cl.insert_char(c);
        }
        assert_eq!(cl.ghost(), "");
    }
}
