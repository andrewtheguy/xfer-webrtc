//! Filesystem browser for the send wizard: navigate directories and
//! multi-select files and/or folders.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};

use crate::archive::send_display_name;

pub struct Browser {
    cwd: PathBuf,
    entries: Vec<Entry>,
    list_state: ListState,
    selected: BTreeSet<PathBuf>,
    show_hidden: bool,
    error: Option<String>,
}

struct Entry {
    name: String,
    path: PathBuf,
    is_dir: bool,
}

/// What a key press did to the browser. On `Confirm`, read the final
/// selection via [`Browser::selection`].
pub enum BrowserStep {
    Stay,
    Back,
    Confirm,
}

impl Browser {
    pub fn new() -> Result<Self> {
        let cwd = std::env::current_dir().context("Cannot determine the current directory")?;
        let mut browser = Self {
            cwd,
            entries: Vec::new(),
            list_state: ListState::default(),
            selected: BTreeSet::new(),
            show_hidden: false,
            error: None,
        };
        browser.refresh();
        Ok(browser)
    }

    fn refresh(&mut self) {
        self.entries.clear();
        self.error = None;
        let read = match std::fs::read_dir(&self.cwd) {
            Ok(read) => read,
            Err(e) => {
                self.error = Some(format!("Cannot read {}: {e}", self.cwd.display()));
                self.list_state.select(None);
                return;
            }
        };
        for dir_entry in read.flatten() {
            let Ok(name) = dir_entry.file_name().into_string() else {
                continue; // non-UTF-8 names cannot travel the wire anyway
            };
            if !self.show_hidden && name.starts_with('.') {
                continue;
            }
            let path = dir_entry.path();
            let is_dir =
                dir_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) || (path.is_dir()); // resolve symlinked dirs for navigation
            self.entries.push(Entry { name, path, is_dir });
        }
        self.entries.sort_by(|a, b| {
            (!a.is_dir, a.name.to_lowercase()).cmp(&(!b.is_dir, b.name.to_lowercase()))
        });
        self.list_state.select(if self.entries.is_empty() {
            None
        } else {
            Some(0)
        });
    }

    fn cursor_entry(&self) -> Option<&Entry> {
        self.entries.get(self.list_state.selected()?)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> BrowserStep {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.list_state.select_previous(),
            KeyCode::Down | KeyCode::Char('j') => self.list_state.select_next(),
            KeyCode::Enter | KeyCode::Right => {
                if let Some(entry) = self.cursor_entry()
                    && entry.is_dir
                {
                    self.cwd = entry.path.clone();
                    self.refresh();
                }
            }
            KeyCode::Left | KeyCode::Backspace => {
                if let Some(parent) = self.cwd.parent() {
                    self.cwd = parent.to_path_buf();
                    self.refresh();
                }
            }
            KeyCode::Char(' ') => {
                if let Some(entry) = self.cursor_entry() {
                    let path = entry.path.clone();
                    if !self.selected.remove(&path) {
                        self.selected.insert(path);
                    }
                    self.list_state.select_next();
                }
            }
            KeyCode::Char('.') => {
                self.show_hidden = !self.show_hidden;
                self.refresh();
            }
            KeyCode::Tab | KeyCode::Char('s') => {
                if !self.selected.is_empty() {
                    return BrowserStep::Confirm;
                }
            }
            KeyCode::Esc => return BrowserStep::Back,
            _ => {}
        }
        BrowserStep::Stay
    }

    pub fn selection(&self) -> Vec<PathBuf> {
        self.selected.iter().cloned().collect()
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect) {
        let [header, list_area, footer] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Fill(1),
            Constraint::Length(2),
        ])
        .areas(area);

        f.render_widget(
            Paragraph::new(format!("Select what to send — {}", self.cwd.display())).bold(),
            header,
        );

        if let Some(error) = &self.error {
            f.render_widget(Paragraph::new(error.as_str()).red(), list_area);
        } else if self.entries.is_empty() {
            f.render_widget(Paragraph::new("(empty directory)").dim(), list_area);
        } else {
            let items: Vec<ListItem> = self
                .entries
                .iter()
                .map(|entry| {
                    let mark = if self.selected.contains(&entry.path) {
                        "[x]"
                    } else {
                        "[ ]"
                    };
                    let suffix = if entry.is_dir { "/" } else { "" };
                    let item = ListItem::new(format!(" {mark} {}{suffix}", entry.name));
                    if entry.is_dir {
                        item.style(Style::default().add_modifier(Modifier::BOLD))
                    } else {
                        item
                    }
                })
                .collect();
            let list =
                List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
            f.render_stateful_widget(list, list_area, &mut self.list_state);
        }

        let summary = if self.selected.is_empty() {
            "Nothing selected yet".to_string()
        } else {
            format!(
                "{} selected · will be sent as \"{}\"",
                self.selected.len(),
                send_display_name(&self.selection())
            )
        };
        let hints = "↑/↓ move · Enter open · ←/Backspace up · Space select · . hidden · Tab confirm · Esc back";
        f.render_widget(Paragraph::new(format!("{summary}\n{hints}")).dim(), footer);
    }
}
