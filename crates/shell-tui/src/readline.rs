//! Line editor — raw-mode terminal input with full editing capability.

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{self, ClearType},
};
use shell_core::state::ShellState;
use shell_hooks::registry::HookRegistry;

use crate::{complete::Completer, history::History, prompt};

pub enum ReadlineEvent {
    Line(String),
    Eof,
    Interrupted,
}

pub struct Readline {
    history: History,
}

impl Readline {
    pub fn new() -> Self {
        let mut history = History::new();
        history.load();
        Self { history }
    }

    /// Block until the user submits a line. Returns the line content.
    pub fn read_line(
        &mut self,
        state: &mut ShellState,
        hooks: &mut HookRegistry,
    ) -> std::io::Result<ReadlineEvent> {
        prompt::render(state);

        terminal::enable_raw_mode()?;
        let result = self.edit_loop(state, hooks);
        terminal::disable_raw_mode()?;

        // Always emit a newline after the line editor exits.
        println!();

        result
    }

    fn edit_loop(
        &mut self,
        state: &mut ShellState,
        hooks: &mut HookRegistry,
    ) -> std::io::Result<ReadlineEvent> {
        let mut buf = String::new();
        let mut cursor_pos: usize = 0; // byte offset into buf

        loop {
            let Event::Key(KeyEvent {
                code, modifiers, ..
            }) = event::read()?
            else {
                continue;
            };

            match (code, modifiers) {
                // Submit
                (KeyCode::Enter, _) => {
                    if !buf.trim().is_empty() {
                        self.history.push(buf.clone());
                    }
                    return Ok(ReadlineEvent::Line(buf));
                }

                // EOF / Ctrl-D
                (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                    if buf.is_empty() {
                        return Ok(ReadlineEvent::Eof);
                    }
                }

                // Interrupt / Ctrl-C
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                    return Ok(ReadlineEvent::Interrupted);
                }

                // Backspace
                (KeyCode::Backspace, _) => {
                    if cursor_pos > 0 {
                        // Remove the char before the cursor.
                        let ch_start = buf[..cursor_pos]
                            .char_indices()
                            .last()
                            .map(|(i, _)| i)
                            .unwrap_or(0);
                        buf.drain(ch_start..cursor_pos);
                        cursor_pos = ch_start;
                        self.redraw(&buf, cursor_pos)?;
                    }
                }

                // Delete
                (KeyCode::Delete, _) => {
                    if cursor_pos < buf.len() {
                        let ch_end = buf[cursor_pos..]
                            .char_indices()
                            .nth(1)
                            .map(|(i, _)| cursor_pos + i)
                            .unwrap_or(buf.len());
                        buf.drain(cursor_pos..ch_end);
                        self.redraw(&buf, cursor_pos)?;
                    }
                }

                // Left arrow
                (KeyCode::Left, _) => {
                    if cursor_pos > 0 {
                        cursor_pos = buf[..cursor_pos]
                            .char_indices()
                            .last()
                            .map(|(i, _)| i)
                            .unwrap_or(0);
                        self.redraw(&buf, cursor_pos)?;
                    }
                }

                // Right arrow
                (KeyCode::Right, _) => {
                    if cursor_pos < buf.len() {
                        let next = buf[cursor_pos..]
                            .char_indices()
                            .nth(1)
                            .map(|(i, _)| cursor_pos + i)
                            .unwrap_or(buf.len());
                        cursor_pos = next;
                        self.redraw(&buf, cursor_pos)?;
                    }
                }

                // Up arrow — history prev
                (KeyCode::Up, _) => {
                    if let Some(entry) = self.history.prev() {
                        buf = entry.to_owned();
                        cursor_pos = buf.len();
                        self.redraw(&buf, cursor_pos)?;
                    }
                }

                // Down arrow — history next
                (KeyCode::Down, _) => {
                    let entry = self.history.next().unwrap_or("").to_owned();
                    buf = entry;
                    cursor_pos = buf.len();
                    self.redraw(&buf, cursor_pos)?;
                }

                // Home / Ctrl-A
                (KeyCode::Home, _) | (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
                    cursor_pos = 0;
                    self.redraw(&buf, cursor_pos)?;
                }

                // End / Ctrl-E
                (KeyCode::End, _) | (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
                    cursor_pos = buf.len();
                    self.redraw(&buf, cursor_pos)?;
                }

                // Kill to end of line / Ctrl-K
                (KeyCode::Char('k'), KeyModifiers::CONTROL) => {
                    buf.truncate(cursor_pos);
                    self.redraw(&buf, cursor_pos)?;
                }

                // Tab — completion
                (KeyCode::Tab, _) => {
                    let candidates = Completer::complete(&buf, cursor_pos, state, hooks);
                    if candidates.len() == 1 {
                        // Single match — inline complete
                        let word_start = buf[..cursor_pos]
                            .rfind(|c: char| c.is_whitespace())
                            .map(|i| i + 1)
                            .unwrap_or(0);
                        buf.replace_range(word_start..cursor_pos, &candidates[0]);
                        cursor_pos = word_start + candidates[0].len();
                        self.redraw(&buf, cursor_pos)?;
                    } else if !candidates.is_empty() {
                        self.show_completions(&candidates)?;
                        prompt::render(state);
                        self.redraw(&buf, cursor_pos)?;
                    }
                }

                // Printable character
                (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                    buf.insert(cursor_pos, ch);
                    cursor_pos += ch.len_utf8();
                    self.redraw(&buf, cursor_pos)?;
                }

                _ => {}
            }
        }
    }

    fn redraw(&self, buf: &str, cursor_pos: usize) -> std::io::Result<()> {
        use std::io::Write;
        let mut out = std::io::stdout();
        execute!(
            out,
            cursor::MoveToColumn(3), // "▸ " is 3 columns wide (arrow + space)
            terminal::Clear(ClearType::UntilNewLine),
            crossterm::style::Print(buf),
            cursor::MoveToColumn(3 + cursor_pos as u16),
        )?;
        out.flush()
    }

    fn show_completions(&self, candidates: &[String]) -> std::io::Result<()> {
        use crate::theme::{COLOR_COMPLETION_ITEM, COLOR_COMPLETION_SEL};
        use crossterm::style::{Print, ResetColor, SetForegroundColor};
        println!();
        for (i, c) in candidates.iter().enumerate() {
            let color = if i == 0 {
                COLOR_COMPLETION_SEL
            } else {
                COLOR_COMPLETION_ITEM
            };
            execute!(
                std::io::stdout(),
                SetForegroundColor(color),
                Print(format!("  {c}\n")),
                ResetColor,
            )?;
        }
        Ok(())
    }
}

impl Default for Readline {
    fn default() -> Self {
        Self::new()
    }
}
