//! The Registry page of the `configure` interface: search the registry, add libraries and tune the ones in use.

use std::collections::HashMap;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use super::tui::{checkbox, toggle_color};
use super::Dep;
use crate::registry::{self, Details, Entry};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    List,
    Details,
}

/// Rows of the editor for a registry dependency in use, before its features.
const VERSION_ROW: usize = 0;
const PUBLIC_ROW: usize = 1;
const DEFAULTS_ROW: usize = 2;
const FIRST_FEATURE_ROW: usize = 3;

pub struct RegistryView {
    loaded: bool,
    error: String,
    entries: Vec<Entry>,
    filter: String,
    cursor: usize,
    list: ListState,
    list_area: Rect,
    pane: Pane,
    row: usize,
    details: HashMap<(String, String), Option<Details>>,
    status: String,
}

impl RegistryView {
    pub fn new() -> RegistryView {
        RegistryView {
            loaded: false,
            error: String::new(),
            entries: Vec::new(),
            filter: String::new(),
            cursor: 0,
            list: ListState::default(),
            list_area: Rect::default(),
            pane: Pane::List,
            row: 0,
            details: HashMap::new(),
            status: String::new(),
        }
    }

    pub fn needs_load(&self) -> bool {
        !self.loaded
    }

    /// Reads the registry; this can take a moment on the first run, when it has to be cloned.
    pub fn load(&mut self) {
        match registry::catalog() {
            Ok(entries) => self.entries = entries,
            Err(err) => self.error = err.to_string(),
        }
        self.loaded = true;
    }

    /// Whether typed characters go to the search box.
    pub fn is_typing(&self) -> bool {
        self.pane == Pane::List
    }

    /// Esc backs out of the editor or clears the search before it means quit.
    pub fn wants_escape(&self) -> bool {
        self.pane == Pane::Details || !self.filter.is_empty()
    }

    pub fn escape(&mut self) {
        if self.pane == Pane::Details {
            self.pane = Pane::List;
        } else {
            self.filter.clear();
            self.cursor = 0;
        }
    }

    /// Shows one library, for jumping here from the dependency list.
    pub fn select(&mut self, name: &str) {
        self.filter = name.to_string();
        self.cursor = 0;
        self.pane = Pane::List;
    }

    fn shown(&self) -> Vec<usize> {
        let filter = self.filter.to_lowercase();
        (0..self.entries.len())
            .filter(|&index| self.entries[index].name.to_lowercase().contains(&filter))
            .collect()
    }

    fn current(&self) -> Option<&Entry> {
        self.shown()
            .get(self.cursor)
            .map(|&index| &self.entries[index])
    }

    fn move_cursor(&mut self, step: isize) {
        let last = self.shown().len().saturating_sub(1) as isize;
        self.cursor = (self.cursor as isize + step).clamp(0, last) as usize;
        self.status.clear();
    }

    pub fn scroll(&mut self, step: isize) {
        if self.pane == Pane::List {
            self.move_cursor(step);
        } else {
            self.row = (self.row as isize + step.signum()).max(0) as usize;
        }
    }

    pub fn click(&mut self, position: Position, deps: &mut Vec<Dep>) {
        if !self.list_area.contains(position) {
            return;
        }
        let row = self.list.offset() + (position.y - self.list_area.y) as usize;
        if row < self.shown().len() {
            self.pane = Pane::List;
            self.cursor = row;
            self.toggle(deps);
        }
    }

    pub fn on_key(&mut self, key: KeyEvent, deps: &mut Vec<Dep>) {
        match self.pane {
            Pane::List => self.on_list_key(key, deps),
            Pane::Details => self.on_details_key(key, deps),
        }
    }

    fn on_list_key(&mut self, key: KeyEvent, deps: &mut Vec<Dep>) {
        match key.code {
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Down => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-10),
            KeyCode::PageDown => self.move_cursor(10),
            KeyCode::Home => self.move_cursor(isize::MIN / 2),
            KeyCode::End => self.move_cursor(isize::MAX / 2),
            KeyCode::Enter | KeyCode::Char(' ') => self.toggle(deps),
            KeyCode::Right => {
                let in_use = self
                    .current()
                    .is_some_and(|entry| in_use(deps, &entry.name));
                if in_use {
                    self.pane = Pane::Details;
                    self.row = VERSION_ROW;
                }
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.cursor = 0;
            }
            KeyCode::Char(typed)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.filter.push(typed);
                self.cursor = 0;
            }
            _ => {}
        }
    }

    /// Adds the library at its newest series, or switches a registry dependency already listed on or off.
    fn toggle(&mut self, deps: &mut Vec<Dep>) {
        let Some(entry) = self.current() else { return };
        let (name, newest) = (entry.name.clone(), entry.series().remove(0));
        match deps.iter_mut().find(|dep| dep.key == name) {
            Some(dep) if dep.version.is_some() => dep.on = !dep.on,
            Some(_) => self.status = format!("'{name}' is already a folder dependency"),
            None => deps.push(Dep {
                key: name,
                path: String::new(),
                version: Some(newest),
                on: true,
                configured: true,
                public: false,
                features: Vec::new(),
                default_features: true,
                link: None,
            }),
        }
    }

    fn on_details_key(&mut self, key: KeyEvent, deps: &mut [Dep]) {
        let Some(entry) = self.current() else { return };
        let name = entry.name.clone();
        let series = entry.series();
        let Some(dep) = deps
            .iter_mut()
            .find(|dep| dep.key == name && dep.on && dep.version.is_some())
        else {
            self.pane = Pane::List;
            return;
        };
        let requirement = dep.version.clone().unwrap_or_default();
        let defaults: Vec<String> =
            self.details_for(&name, &requirement)
                .map_or(Vec::new(), |found| {
                    found
                        .features
                        .iter()
                        .filter(|(_, default)| *default)
                        .map(|(feature, _)| feature.clone())
                        .collect()
                });
        let features: Vec<String> =
            self.details_for(&name, &requirement)
                .map_or(Vec::new(), |found| {
                    found
                        .features
                        .iter()
                        .map(|(feature, _)| feature.clone())
                        .collect()
                });
        let rows = FIRST_FEATURE_ROW + features.len();
        self.row = self.row.min(rows - 1);
        let Some(dep) = deps.iter_mut().find(|dep| dep.key == name) else {
            return;
        };
        match key.code {
            KeyCode::Up => self.row = self.row.saturating_sub(1),
            KeyCode::Down => self.row = (self.row + 1).min(rows - 1),
            KeyCode::Left => self.pane = Pane::List,
            KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ') if self.row == VERSION_ROW => {
                let at = series
                    .iter()
                    .position(|found| Some(found) == dep.version.as_ref())
                    .unwrap_or(0);
                dep.version = Some(series[(at + 1) % series.len()].clone());
            }
            KeyCode::Enter | KeyCode::Char(' ') => match self.row {
                VERSION_ROW => {}
                PUBLIC_ROW => dep.public = !dep.public,
                DEFAULTS_ROW => dep.default_features = !dep.default_features,
                row => toggle_feature(dep, &features[row - FIRST_FEATURE_ROW], &defaults),
            },
            _ => {}
        }
    }

    fn details_for(&mut self, name: &str, requirement: &str) -> Option<&Details> {
        self.details
            .entry((name.to_string(), requirement.to_string()))
            .or_insert_with(|| registry::details(name, requirement).ok())
            .as_ref()
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, deps: &[Dep]) {
        if !self.loaded {
            frame.render_widget(
                Paragraph::new(" Loading the registry...")
                    .block(Block::bordered().title(" Registry ")),
                area,
            );
            return;
        }
        if !self.error.is_empty() {
            let lines = vec![
                Line::styled("Cannot read the registry:", Style::new().fg(Color::Red)),
                Line::raw(self.error.clone()),
            ];
            frame.render_widget(
                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .block(Block::bordered().title(" Registry ")),
                area,
            );
            return;
        }
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
                .areas(area);
        self.draw_list(frame, left, deps);
        self.draw_details(frame, right, deps);
    }

    fn draw_list(&mut self, frame: &mut Frame, area: Rect, deps: &[Dep]) {
        let block = Block::bordered().title(" Registry ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let [search, list_area] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(inner);
        self.list_area = list_area;
        let cursor = if self.pane == Pane::List { "_" } else { "" };
        let hint = if self.filter.is_empty() {
            Span::styled("type to search", Style::new().fg(Color::DarkGray))
        } else {
            Span::raw(self.filter.clone())
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw("Search: "),
                hint,
                Span::raw(cursor),
            ])),
            search,
        );

        let shown = self.shown();
        let items: Vec<ListItem> = shown
            .iter()
            .map(|&index| {
                let entry = &self.entries[index];
                let dep = deps
                    .iter()
                    .find(|dep| dep.key == entry.name && dep.version.is_some());
                let on = dep.is_some_and(|dep| dep.on);
                let mut spans = vec![
                    Span::styled(checkbox(on), Style::new().fg(toggle_color(on))),
                    Span::raw(format!(" {}", entry.name)),
                    Span::styled(
                        format!("  {}", entry.versions[0]),
                        Style::new().fg(Color::DarkGray),
                    ),
                ];
                if let Some(dep) = dep.filter(|dep| dep.on) {
                    spans.push(Span::styled(
                        format!("  using {}", dep.version.as_deref().unwrap_or("")),
                        Style::new().fg(Color::Cyan),
                    ));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        if items.is_empty() {
            let text = if self.entries.is_empty() {
                "The registry has no libraries."
            } else {
                "No library matches."
            };
            frame.render_widget(
                Paragraph::new(text).style(Style::new().fg(Color::Gray)),
                list_area,
            );
            return;
        }
        let highlight = if self.pane == Pane::List {
            Modifier::REVERSED
        } else {
            Modifier::BOLD
        };
        self.list.select(Some(self.cursor.min(shown.len() - 1)));
        frame.render_stateful_widget(
            List::new(items).highlight_style(Style::new().add_modifier(highlight)),
            list_area,
            &mut self.list,
        );
    }

    fn draw_details(&mut self, frame: &mut Frame, area: Rect, deps: &[Dep]) {
        let block = Block::bordered().title(" Details ");
        let Some(entry) = self.current() else {
            frame.render_widget(block, area);
            return;
        };
        let (name, versions, series) = (entry.name.clone(), entry.versions.clone(), entry.series());
        let dep = deps
            .iter()
            .find(|dep| dep.key == name && dep.on && dep.version.is_some());
        let requirement = dep
            .and_then(|dep| dep.version.clone())
            .unwrap_or_else(|| series[0].clone());
        let details = self.details_for(&name, &requirement);
        let features: Vec<(String, bool)> =
            details.map_or(Vec::new(), |found| found.features.clone());
        let source = details
            .map(|found| format!("{} @ {}", found.git, found.version))
            .unwrap_or_default();

        let dim = Style::new().fg(Color::Gray);
        let mut lines = vec![
            Line::styled(name.clone(), Style::new().add_modifier(Modifier::BOLD)),
            Line::styled(format!("versions: {}", versions.join(", ")), dim),
            Line::styled(format!("source:   {source}"), dim),
            Line::raw(""),
        ];
        let selected = |row: usize| {
            if self.pane == Pane::Details && self.row == row {
                Style::new().add_modifier(Modifier::REVERSED)
            } else {
                Style::new()
            }
        };
        let toggle = |row: usize, on: bool, text: String| {
            Line::from(vec![Span::styled(
                format!("{} {text}", checkbox(on)),
                Style::new().fg(toggle_color(on)),
            )
            .patch_style(selected(row))])
        };
        match dep {
            Some(dep) => {
                lines.push(Line::styled(
                    "In this project (Right to edit, Left or Esc back):",
                    Style::new().fg(Color::Cyan),
                ));
                lines.push(Line::styled(
                    format!(
                        "  version  {}   (Space/Right: {})",
                        dep.version.as_deref().unwrap_or(""),
                        series.join(", ")
                    ),
                    selected(VERSION_ROW),
                ));
                lines.push(toggle(
                    PUBLIC_ROW,
                    dep.public,
                    "public: re-export its headers".into(),
                ));
                lines.push(toggle(
                    DEFAULTS_ROW,
                    dep.default_features,
                    "default features".into(),
                ));
                for (offset, (feature, default)) in features.iter().enumerate() {
                    let on = dep.features.contains(feature) || (dep.default_features && *default);
                    let suffix = if *default { " (default)" } else { "" };
                    lines.push(toggle(
                        FIRST_FEATURE_ROW + offset,
                        on,
                        format!("feature {feature}{suffix}"),
                    ));
                }
            }
            None => {
                lines.push(Line::styled(
                    format!("Space adds it as {}", series[0]),
                    Style::new().fg(Color::Cyan),
                ));
                if !features.is_empty() {
                    lines.push(Line::raw(""));
                    lines.push(Line::raw("features:"));
                    for (feature, default) in &features {
                        lines.push(Line::styled(
                            format!("  {feature}{}", if *default { " (default)" } else { "" }),
                            dim,
                        ));
                    }
                }
            }
        }
        if !self.status.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                self.status.clone(),
                Style::new().fg(Color::Yellow),
            ));
        }
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(block),
            area,
        );
    }
}

fn in_use(deps: &[Dep], name: &str) -> bool {
    deps.iter()
        .any(|dep| dep.key == name && dep.on && dep.version.is_some())
}

/// Switches one feature. Turning off a default feature turns the defaults off and keeps the others by name.
fn toggle_feature(dep: &mut Dep, feature: &str, defaults: &[String]) {
    let on = dep.features.iter().any(|found| found == feature)
        || (dep.default_features && defaults.iter().any(|found| found == feature));
    if !on {
        dep.features.push(feature.to_string());
        return;
    }
    dep.features.retain(|found| found != feature);
    if dep.default_features && defaults.iter().any(|found| found == feature) {
        dep.default_features = false;
        for other in defaults.iter().filter(|other| *other != feature) {
            if !dep.features.contains(other) {
                dep.features.push(other.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dep() -> Dep {
        Dep {
            key: "imgui".into(),
            path: String::new(),
            version: Some("1".into()),
            on: true,
            configured: true,
            public: false,
            features: Vec::new(),
            default_features: true,
            link: None,
        }
    }

    #[test]
    fn turning_off_a_default_feature_keeps_the_other_defaults() {
        let defaults = ["dx11".to_string(), "win32".to_string()];
        let mut dep = dep();
        toggle_feature(&mut dep, "dx11", &defaults);
        assert!(!dep.default_features && dep.features == ["win32"]);
        toggle_feature(&mut dep, "opengl", &defaults);
        toggle_feature(&mut dep, "dx11", &defaults);
        assert_eq!(dep.features, ["win32", "opengl", "dx11"]);
        toggle_feature(&mut dep, "opengl", &defaults);
        assert_eq!(dep.features, ["win32", "dx11"]);
    }
}
