//! Full-screen terminal UI for `nomake init`: browse the project tree and mark what gets built.

use std::io::stdout;

use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use super::registry::RegistryView;
use super::{kind_name, Computed, Model, NodeKind, State};
use crate::config::Kind;
use crate::util::absolute;
use crate::Result;

const LABEL_WIDTH: usize = 16;
const TEXT_PREFIX: u16 = 2 + LABEL_WIDTH as u16 + 2;
const KINDS: [Kind; 3] = [Kind::Executable, Kind::Static, Kind::Shared];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Project,
    Sources,
    Includes,
    Registry,
    Review,
}

impl Tab {
    const ALL: [Tab; 5] = [
        Tab::Project,
        Tab::Sources,
        Tab::Includes,
        Tab::Registry,
        Tab::Review,
    ];

    fn title(self) -> &'static str {
        match self {
            Tab::Project => "Project",
            Tab::Sources => "Sources",
            Tab::Includes => "Includes",
            Tab::Registry => "Registry",
            Tab::Review => "Review",
        }
    }

    fn position(self) -> usize {
        Tab::ALL.iter().position(|tab| *tab == self).unwrap()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Name,
    Kind,
    Std,
    Defines,
    Libs,
    Export,
    Dep(usize),
    Register,
}

impl Field {
    fn is_text(self) -> bool {
        matches!(
            self,
            Field::Name | Field::Std | Field::Defines | Field::Libs
        )
    }
}

#[derive(Default)]
struct TreeView {
    state: ListState,
    rows: Vec<usize>,
    cursor: usize,
    area: Rect,
}

struct App<'a> {
    model: &'a mut Model,
    tab: Tab,
    expanded: Vec<bool>,
    sources: TreeView,
    includes: TreeView,
    focus: usize,
    cursor: usize,
    field_rects: Vec<(Rect, Field)>,
    tab_rects: Vec<Rect>,
    review_scroll: u16,
    write_button: Rect,
    confirm_quit: bool,
    open_dep: Option<usize>,
    registry: RegistryView,
    adding: Option<String>,
    add_error: String,
}

/// Runs the UI; `true` means the user asked to write `nomake.toml`.
pub fn run(model: &mut Model) -> Result<bool> {
    let mut terminal = ratatui::init();
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableMouseCapture);
        previous_hook(info);
    }));
    execute!(stdout(), EnableMouseCapture)?;
    let result = App::new(model).event_loop(&mut terminal);
    execute!(stdout(), DisableMouseCapture)?;
    ratatui::restore();
    result
}

fn byte_offset(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map_or(text.len(), |(offset, _)| offset)
}

fn text_field(model: &mut Model, field: Field) -> Option<&mut String> {
    match field {
        Field::Name => Some(&mut model.name),
        Field::Std => Some(&mut model.std),
        Field::Defines => Some(&mut model.defines),
        Field::Libs => Some(&mut model.libs),
        _ => None,
    }
}

fn state_color(state: State) -> Color {
    match state {
        State::In => Color::Green,
        State::Out => Color::Red,
        State::Partial => Color::Yellow,
    }
}

pub fn toggle_color(on: bool) -> Color {
    if on {
        Color::Green
    } else {
        Color::Red
    }
}

pub fn checkbox(on: bool) -> &'static str {
    if on {
        "[x]"
    } else {
        "[ ]"
    }
}

fn toml_lines(text: &str) -> Vec<Line<'static>> {
    text.lines()
        .map(|line| {
            if line.starts_with('[') {
                Line::styled(
                    line.to_string(),
                    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                )
            } else if let Some((key, value)) = line.split_once(" = ") {
                Line::from(vec![
                    Span::styled(key.to_string(), Style::new().fg(Color::Yellow)),
                    Span::styled(" = ", Style::new().fg(Color::DarkGray)),
                    Span::styled(value.to_string(), Style::new().fg(Color::Green)),
                ])
            } else {
                Line::raw(line.to_string())
            }
        })
        .collect()
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

impl<'a> App<'a> {
    fn new(model: &'a mut Model) -> App<'a> {
        let count = model.nodes.len();
        let mut expanded = vec![false; count];
        expanded[0] = true;
        for index in 0..count {
            let node = &model.nodes[index];
            if model.choice[index] == Some(false) && node.depth >= 2 {
                let mut ancestor = node.parent;
                while ancestor != 0 {
                    expanded[ancestor] = true;
                    ancestor = model.nodes[ancestor].parent;
                }
            }
        }
        App {
            model,
            tab: Tab::Project,
            expanded,
            sources: TreeView::default(),
            includes: TreeView::default(),
            focus: 0,
            cursor: 0,
            field_rects: Vec::new(),
            tab_rects: Vec::new(),
            review_scroll: 0,
            write_button: Rect::default(),
            confirm_quit: false,
            open_dep: None,
            registry: RegistryView::new(),
            adding: None,
            add_error: String::new(),
        }
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<bool> {
        self.reset_cursor();
        loop {
            terminal.draw(|frame| self.draw(frame))?;
            if self.tab == Tab::Registry && self.registry.needs_load() {
                self.registry.load();
                continue;
            }
            if let Some(write) = self.next_outcome()? {
                return Ok(write);
            }
            if let Some(index) = self.open_dep.take() {
                self.edit_dep(terminal, index)?;
            }
        }
    }

    /// Runs a nested editor on a dependency's folder; saving it also switches the dependency on.
    fn edit_dep(&mut self, terminal: &mut DefaultTerminal, index: usize) -> Result<()> {
        if self.model.deps[index].version.is_some() {
            let key = self.model.deps[index].key.clone();
            self.registry.select(&key);
            self.set_tab(Tab::Registry);
            return Ok(());
        }
        let dir = absolute(&self.model.root.join(&self.model.deps[index].path));
        let mut nested = Model::open(&dir, None)?;
        nested.parent_toml = None;
        nested.register = false;
        if App::new(&mut nested).event_loop(terminal)? {
            nested.write()?;
            let dep = &mut self.model.deps[index];
            dep.configured = true;
            dep.on = true;
        }
        self.reset_cursor();
        Ok(())
    }

    fn next_outcome(&mut self) -> Result<Option<bool>> {
        loop {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => return Ok(self.on_key(key)),
                Event::Mouse(mouse) if mouse.kind != MouseEventKind::Moved => {
                    return Ok(self.on_mouse(mouse))
                }
                Event::Resize(..) => return Ok(None),
                _ => {}
            }
        }
    }

    fn view(&self) -> &TreeView {
        if self.tab == Tab::Includes {
            &self.includes
        } else {
            &self.sources
        }
    }

    fn view_mut(&mut self) -> &mut TreeView {
        if self.tab == Tab::Includes {
            &mut self.includes
        } else {
            &mut self.sources
        }
    }

    fn fields(&self) -> Vec<Field> {
        let mut fields = vec![
            Field::Name,
            Field::Kind,
            Field::Std,
            Field::Defines,
            Field::Libs,
            Field::Export,
        ];
        fields.extend((0..self.model.deps.len()).map(Field::Dep));
        if self.model.parent_toml.is_some() {
            fields.push(Field::Register);
        }
        fields
    }

    fn focused_field(&self) -> Field {
        self.fields()[self.focus]
    }

    fn is_typing(&self) -> bool {
        match self.tab {
            Tab::Project => self.focused_field().is_text(),
            Tab::Registry => self.registry.is_typing(),
            _ => false,
        }
    }

    fn reset_cursor(&mut self) {
        let field = self.focused_field();
        self.cursor = text_field(self.model, field).map_or(0, |text| text.chars().count());
    }

    fn is_visible(&self, index: usize) -> bool {
        let node = &self.model.nodes[index];
        match (self.tab, node.kind) {
            (Tab::Includes, NodeKind::Dir) => !node.locked && node.hdr_total > 0,
            (Tab::Includes, NodeKind::Header) => true,
            (Tab::Includes, NodeKind::Source) => false,
            (_, NodeKind::Dir) => node.locked || node.src_total > 0,
            (_, NodeKind::Source) => true,
            (_, NodeKind::Header) => false,
        }
    }

    fn visible_rows(&self) -> Vec<usize> {
        let mut rows = Vec::new();
        self.push_rows(0, &mut rows);
        rows
    }

    fn push_rows(&self, index: usize, rows: &mut Vec<usize>) {
        rows.push(index);
        if !self.expanded[index] {
            return;
        }
        for &child in &self.model.nodes[index].children {
            if self.is_visible(child) {
                self.push_rows(child, rows);
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Option<bool> {
        if self.confirm_quit {
            return self.on_quit_key(key);
        }
        if self.adding.is_some() {
            return self.on_add_key(key);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let typing = self.is_typing();
        match key.code {
            KeyCode::Char('s') if ctrl => Some(true),
            KeyCode::Char('c') if ctrl => self.ask_quit(),
            KeyCode::Esc if self.tab == Tab::Registry && self.registry.wants_escape() => {
                self.registry.escape();
                None
            }
            KeyCode::Esc => self.ask_quit(),
            KeyCode::Tab => self.switch_tab(1),
            KeyCode::BackTab => self.switch_tab(-1),
            KeyCode::Char('q') if !typing => self.ask_quit(),
            KeyCode::Char('a') if ctrl && self.tab == Tab::Project => {
                self.adding = Some(String::new());
                self.add_error.clear();
                None
            }
            KeyCode::Char(digit @ '1'..='5') if !typing => {
                self.set_tab(Tab::ALL[digit as usize - '1' as usize]);
                None
            }
            _ => {
                match self.tab {
                    Tab::Project => self.on_project_key(key),
                    Tab::Sources | Tab::Includes => self.on_tree_key(key),
                    Tab::Registry => self.registry.on_key(key, &mut self.model.deps),
                    Tab::Review => return self.on_review_key(key),
                }
                None
            }
        }
    }

    fn on_quit_key(&mut self, key: KeyEvent) -> Option<bool> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => Some(false),
            _ => {
                self.confirm_quit = false;
                None
            }
        }
    }

    fn ask_quit(&mut self) -> Option<bool> {
        self.confirm_quit = true;
        None
    }

    fn on_add_key(&mut self, key: KeyEvent) -> Option<bool> {
        let text = self.adding.as_mut()?;
        match key.code {
            KeyCode::Esc => self.adding = None,
            KeyCode::Enter => self.finish_add(),
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char(typed) => text.push(typed),
            _ => {}
        }
        None
    }

    fn finish_add(&mut self) {
        let typed = self.adding.clone().unwrap_or_default();
        match self.model.add_dep(typed.trim()) {
            Ok(index) => {
                self.adding = None;
                if self.model.deps[index].configured {
                    self.model.deps[index].on = true;
                } else {
                    self.open_dep = Some(index);
                }
                self.focus = self
                    .fields()
                    .iter()
                    .position(|field| *field == Field::Dep(index))
                    .unwrap_or(0);
            }
            Err(err) => self.add_error = err.to_string(),
        }
    }

    fn switch_tab(&mut self, step: isize) -> Option<bool> {
        let next = (self.tab.position() as isize + step).rem_euclid(Tab::ALL.len() as isize);
        self.set_tab(Tab::ALL[next as usize]);
        None
    }

    fn set_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.reset_cursor();
    }

    fn on_review_key(&mut self, key: KeyEvent) -> Option<bool> {
        match key.code {
            KeyCode::Enter => return Some(true),
            KeyCode::Up => self.review_scroll = self.review_scroll.saturating_sub(1),
            KeyCode::Down => self.review_scroll += 1,
            KeyCode::PageUp => self.review_scroll = self.review_scroll.saturating_sub(10),
            KeyCode::PageDown => self.review_scroll += 10,
            _ => {}
        }
        None
    }

    fn on_project_key(&mut self, key: KeyEvent) {
        let field = self.focused_field();
        match key.code {
            KeyCode::Up => self.move_focus(-1),
            KeyCode::Down => self.move_focus(1),
            KeyCode::Enter if field.is_text() => self.move_focus(1),
            code if field.is_text() => self.edit_text(field, code),
            KeyCode::Left => self.change_field(field, -1),
            KeyCode::Enter => match field {
                Field::Dep(index) => self.open_dep = Some(index),
                _ => self.change_field(field, 1),
            },
            KeyCode::Right | KeyCode::Char(' ') => self.change_field(field, 1),
            _ => {}
        }
    }

    fn move_focus(&mut self, step: isize) {
        let last = self.fields().len() as isize - 1;
        self.focus = (self.focus as isize + step).clamp(0, last) as usize;
        self.reset_cursor();
    }

    fn edit_text(&mut self, field: Field, code: KeyCode) {
        let cursor = self.cursor;
        let Some(text) = text_field(self.model, field) else {
            return;
        };
        let length = text.chars().count();
        match code {
            KeyCode::Char(typed) => {
                text.insert(byte_offset(text, cursor), typed);
                self.cursor += 1;
            }
            KeyCode::Backspace if cursor > 0 => {
                text.remove(byte_offset(text, cursor - 1));
                self.cursor -= 1;
            }
            KeyCode::Delete if cursor < length => {
                text.remove(byte_offset(text, cursor));
            }
            KeyCode::Left => self.cursor = cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (cursor + 1).min(length),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = length,
            _ => {}
        }
    }

    fn change_field(&mut self, field: Field, step: isize) {
        match field {
            Field::Kind => {
                let current = KINDS
                    .iter()
                    .position(|kind| *kind == self.model.kind)
                    .unwrap();
                let next = (current as isize + step).rem_euclid(KINDS.len() as isize);
                self.model.kind = KINDS[next as usize];
                self.model.export = self.model.kind != Kind::Executable;
            }
            Field::Export => self.model.export = !self.model.export,
            Field::Dep(index) => {
                let dep = &mut self.model.deps[index];
                if dep.configured {
                    dep.on = !dep.on;
                } else {
                    self.open_dep = Some(index);
                }
            }
            Field::Register => self.model.register = !self.model.register,
            _ => {}
        }
    }

    fn on_tree_key(&mut self, key: KeyEvent) {
        let index = self.view().cursor;
        let rows = self.visible_rows().len() as isize;
        match key.code {
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Down => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-10),
            KeyCode::PageDown => self.move_cursor(10),
            KeyCode::Home => self.move_cursor(-rows),
            KeyCode::End => self.move_cursor(rows),
            KeyCode::Left => self.fold_or_parent(index),
            KeyCode::Right => self.unfold_or_enter(index),
            KeyCode::Char(' ') => self.toggle_current(index),
            KeyCode::Enter if self.is_open_dir(index) => {
                self.expanded[index] = !self.expanded[index]
            }
            KeyCode::Enter => self.toggle_current(index),
            KeyCode::Char('*') => self.expanded.fill(true),
            KeyCode::Char('e') if self.tab == Tab::Includes => {
                self.model.export = !self.model.export
            }
            _ => {}
        }
    }

    fn is_open_dir(&self, index: usize) -> bool {
        let node = &self.model.nodes[index];
        node.kind == NodeKind::Dir && !node.locked
    }

    fn move_cursor(&mut self, step: isize) {
        let rows = self.visible_rows();
        let current = rows
            .iter()
            .position(|&index| index == self.view().cursor)
            .unwrap_or(0);
        let next = (current as isize + step).clamp(0, rows.len() as isize - 1) as usize;
        self.view_mut().cursor = rows[next];
    }

    fn fold_or_parent(&mut self, index: usize) {
        if self.is_open_dir(index) && self.expanded[index] {
            self.expanded[index] = false;
        } else if index != 0 {
            self.view_mut().cursor = self.model.nodes[index].parent;
        }
    }

    fn unfold_or_enter(&mut self, index: usize) {
        if !self.is_open_dir(index) {
            return;
        }
        if self.expanded[index] {
            self.move_cursor(1);
        } else {
            self.expanded[index] = true;
        }
    }

    fn toggle_current(&mut self, index: usize) {
        match (self.tab, self.model.nodes[index].kind) {
            (Tab::Sources, NodeKind::Dir | NodeKind::Source) => self.model.toggle_source(index),
            (Tab::Includes, NodeKind::Dir) => self.model.toggle_include(index),
            _ => {}
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent) -> Option<bool> {
        if self.confirm_quit || self.adding.is_some() {
            return None;
        }
        let position = Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => return self.on_click(position),
            MouseEventKind::ScrollDown => self.scroll(3),
            MouseEventKind::ScrollUp => self.scroll(-3),
            _ => {}
        }
        None
    }

    fn scroll(&mut self, step: isize) {
        match self.tab {
            Tab::Sources | Tab::Includes => self.move_cursor(step),
            Tab::Review => self.review_scroll = (self.review_scroll as isize + step).max(0) as u16,
            Tab::Registry => self.registry.scroll(step),
            Tab::Project => {}
        }
    }

    fn on_click(&mut self, position: Position) -> Option<bool> {
        if let Some(index) = self
            .tab_rects
            .iter()
            .position(|rect| rect.contains(position))
        {
            self.set_tab(Tab::ALL[index]);
            return None;
        }
        match self.tab {
            Tab::Project => self.click_project(position),
            Tab::Sources | Tab::Includes => self.click_tree(position),
            Tab::Registry => {}
            Tab::Review => return self.write_button.contains(position).then_some(true),
        }
        None
    }

    fn click_project(&mut self, position: Position) {
        let Some(index) = self
            .field_rects
            .iter()
            .position(|(rect, _)| rect.contains(position))
        else {
            return;
        };
        let (rect, field) = self.field_rects[index];
        self.focus = index;
        self.reset_cursor();
        if field.is_text() {
            let offset = position.x.saturating_sub(rect.x + TEXT_PREFIX) as usize;
            self.cursor = self.cursor.min(offset);
        } else {
            self.change_field(field, 1);
        }
    }

    fn click_tree(&mut self, position: Position) {
        let area = self.view().area;
        if !area.contains(position) {
            return;
        }
        let row = self.view().state.offset() + (position.y - area.y) as usize;
        let Some(&index) = self.view().rows.get(row) else {
            return;
        };
        self.view_mut().cursor = index;
        let column = (position.x - area.x) as usize;
        let indent = 2 * self.model.nodes[index].depth;
        let in_checkbox = column >= indent + 2 && column < indent + 5;
        if in_checkbox {
            self.toggle_current(index);
        } else if self.is_open_dir(index) {
            self.expanded[index] = !self.expanded[index];
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        let computed = self.model.compute();
        let [tabs_area, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        self.draw_tabs(frame, tabs_area);
        match self.tab {
            Tab::Project => self.draw_project(frame, body),
            Tab::Sources | Tab::Includes => self.draw_tree(frame, body, &computed),
            Tab::Registry => self.registry.draw(frame, body, &self.model.deps),
            Tab::Review => self.draw_review(frame, body),
        }
        frame.render_widget(
            Paragraph::new(self.footer_text()).style(Style::new().fg(Color::Gray)),
            footer,
        );
        if self.confirm_quit {
            let area = centered(frame.area(), 44, 5);
            let prompt = Paragraph::new(
                "Quit without writing nomake.toml?\n\n(y) quit    (any other key) stay",
            )
            .alignment(Alignment::Center)
            .block(Block::bordered().title(" Quit "));
            frame.render_widget(Clear, area);
            frame.render_widget(prompt, area);
        }
        if let Some(text) = &self.adding {
            let area = centered(frame.area(), 70, 7);
            let lines = vec![
                Line::raw("Dependency folder (relative or absolute):"),
                Line::raw(format!("> {text}_")),
                Line::styled(self.add_error.clone(), Style::new().fg(Color::Red)),
                Line::raw(""),
                Line::styled("Enter add    Esc cancel", Style::new().fg(Color::Gray)),
            ];
            frame.render_widget(Clear, area);
            frame.render_widget(
                Paragraph::new(lines).block(Block::bordered().title(" Add dependency ")),
                area,
            );
        }
    }

    fn footer_text(&self) -> &'static str {
        match self.tab {
            Tab::Project => " Up/Down field | Space toggle | Enter configure dep | Ctrl+A add dep | Ctrl+S write | Esc quit",
            Tab::Sources => {
                " Up/Down move | Left/Right fold | Space include/exclude | * expand all | Ctrl+S write"
            }
            Tab::Includes => " Up/Down move | Left/Right fold | Space toggle -I | e export | * expand all | Ctrl+S write",
            Tab::Registry => " type to search | Up/Down move | Space add/remove | Right edit | Esc clear | Ctrl+S write",
            Tab::Review => " Up/Down scroll | Enter write | Tab next page | Esc quit",
        }
    }

    fn draw_tabs(&mut self, frame: &mut Frame, area: Rect) {
        self.tab_rects.clear();
        let title = format!(
            " nomake {}: {} ",
            self.model.verb(),
            self.model.root.file_name().unwrap().to_string_lossy()
        );
        let mut spans = vec![Span::styled(
            title.clone(),
            Style::new().add_modifier(Modifier::BOLD),
        )];
        let mut x = area.x + title.len() as u16;
        for (position, tab) in Tab::ALL.iter().enumerate() {
            let label = format!(" {} {} ", position + 1, tab.title());
            let width = label.len() as u16;
            self.tab_rects.push(Rect::new(x, area.y, width, 1));
            let style = if *tab == self.tab {
                Style::new()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::White)
            };
            spans.push(Span::styled(label, style));
            spans.push(Span::raw(" "));
            x += width + 1;
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_project(&mut self, frame: &mut Frame, area: Rect) {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(70), Constraint::Percentage(30)])
                .areas(area);
        let block = Block::bordered().title(" Package ");
        let inner = block.inner(left);
        frame.render_widget(block, left);
        self.field_rects.clear();
        let mut y = inner.y;
        for (position, field) in self.fields().into_iter().enumerate() {
            y += match field {
                Field::Export | Field::Register => 1,
                Field::Dep(0) => 2,
                _ => 0,
            };
            if y >= inner.bottom() {
                break;
            }
            if field == Field::Dep(0) {
                let heading = Rect::new(inner.x, y - 1, inner.width, 1);
                frame.render_widget(
                    Paragraph::new("Dependencies: Space toggle, Enter configure")
                        .style(Style::new().fg(Color::Gray)),
                    heading,
                );
            }
            let rect = Rect::new(inner.x, y, inner.width, 1);
            self.field_rects.push((rect, field));
            let focused = position == self.focus;
            frame.render_widget(Paragraph::new(self.field_line(field, focused)), rect);
            if focused && field.is_text() {
                frame.set_cursor_position(Position::new(
                    rect.x + TEXT_PREFIX + self.cursor as u16,
                    rect.y,
                ));
            }
            y += 1;
        }
        let nodes = &self.model.nodes;
        let summary = vec![
            Line::styled(
                self.model.root.display().to_string(),
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Line::raw(""),
            Line::raw(format!("{} source files", nodes[0].src_total)),
            Line::raw(format!("{} header files", nodes[0].hdr_total)),
        ];
        frame.render_widget(
            Paragraph::new(summary).block(Block::bordered().title(" Detected ")),
            right,
        );
    }

    fn field_line(&self, field: Field, focused: bool) -> Line<'static> {
        let marker = if focused { "> " } else { "  " };
        let label_style = if focused {
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::Gray)
        };
        let labelled =
            |label: &str| Span::styled(format!("{marker}{label:<LABEL_WIDTH$}: "), label_style);
        let model = &self.model;
        match field {
            Field::Name => Line::from(vec![
                labelled("Package name"),
                Span::raw(model.name.clone()),
            ]),
            Field::Std => Line::from(vec![labelled("C++ standard"), Span::raw(model.std.clone())]),
            Field::Defines => Line::from(vec![
                labelled("Defines"),
                self.text_value(&model.defines, "comma separated, e.g. FOO, BAR=1"),
            ]),
            Field::Libs => Line::from(vec![
                labelled("System libs"),
                self.text_value(&model.libs, "comma separated, e.g. pthread, user32"),
            ]),
            Field::Kind => {
                let mut spans = vec![labelled("Kind")];
                for kind in KINDS {
                    let selected = kind == model.kind;
                    let glyph = if selected { "(*) " } else { "( ) " };
                    let color = if selected {
                        Color::Green
                    } else {
                        Color::DarkGray
                    };
                    spans.push(Span::styled(
                        format!("{glyph}{}   ", kind_name(kind)),
                        Style::new().fg(color),
                    ));
                }
                Line::from(spans)
            }
            Field::Export => {
                self.toggle_line(marker, model.export, "Export include dirs to dependents")
            }
            Field::Dep(index) => {
                let dep = &model.deps[index];
                let mut line = self.toggle_line(marker, dep.on, &dep.label());
                if !dep.configured {
                    line.spans.push(Span::styled(
                        "  no nomake.toml",
                        Style::new().fg(Color::Yellow),
                    ));
                }
                line
            }
            Field::Register => {
                let parent = model.parent_toml.as_ref().unwrap().display();
                self.toggle_line(
                    marker,
                    model.register,
                    &format!("Add '{}' to {parent}", model.name),
                )
            }
        }
    }

    fn text_value(&self, value: &str, hint: &str) -> Span<'static> {
        if value.is_empty() {
            Span::styled(hint.to_string(), Style::new().fg(Color::DarkGray))
        } else {
            Span::raw(value.to_string())
        }
    }

    fn toggle_line(&self, marker: &str, on: bool, text: &str) -> Line<'static> {
        let color = toggle_color(on);
        Line::from(vec![
            Span::raw(marker.to_string()),
            Span::styled(checkbox(on), Style::new().fg(color)),
            Span::styled(format!(" {text}"), Style::new().fg(color)),
        ])
    }

    fn draw_tree(&mut self, frame: &mut Frame, area: Rect, computed: &Computed) {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
                .areas(area);
        let rows = self.visible_rows();
        let cursor_row = rows
            .iter()
            .position(|&index| index == self.view().cursor)
            .unwrap_or(0);
        let items: Vec<ListItem> = rows
            .iter()
            .map(|&index| self.tree_item(index, computed))
            .collect();
        let block = Block::bordered().title(self.tree_title());
        let inner = block.inner(left);
        let list = List::new(items)
            .block(block)
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
        let view = self.view_mut();
        view.rows = rows;
        view.area = inner;
        view.state.select(Some(cursor_row));
        frame.render_stateful_widget(list, left, &mut view.state);

        let mut lines = if self.tab == Tab::Sources {
            self.source_details(computed)
        } else {
            self.include_details()
        };
        lines.push(Line::raw(""));
        let preview = if self.tab == Tab::Sources {
            self.model.sources_section()
        } else {
            self.model.include_section()
        };
        lines.extend(toml_lines(&preview));
        let panel = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(" Result "));
        frame.render_widget(panel, right);
    }

    fn tree_title(&self) -> Line<'static> {
        let legend =
            |text: &str, color: Color| Span::styled(format!("{text}  "), Style::new().fg(color));
        let spans = if self.tab == Tab::Sources {
            vec![
                legend(" [x] compiled", Color::Green),
                legend("[ ] excluded", Color::Red),
                legend("[~] partial", Color::Yellow),
                legend("ignored", Color::DarkGray),
            ]
        } else {
            vec![
                legend(" [x] on include path", Color::Green),
                legend("[ ] off", Color::Gray),
                legend("header", Color::Cyan),
            ]
        };
        Line::from(spans)
    }

    fn tree_item(&self, index: usize, computed: &Computed) -> ListItem<'static> {
        let node = &self.model.nodes[index];
        let arrow = match (self.is_open_dir(index), self.expanded[index]) {
            (true, true) => "v ",
            (true, false) => "> ",
            _ => "  ",
        };
        let (glyph, color) = self.row_marker(index, computed);
        let label = if node.kind == NodeKind::Dir {
            format!("{}/", node.name)
        } else {
            node.name.clone()
        };
        let mut spans = vec![
            Span::raw("  ".repeat(node.depth)),
            Span::raw(arrow),
            Span::styled(glyph, Style::new().fg(color)),
            Span::raw(" "),
            Span::styled(label, Style::new().fg(color)),
        ];
        let dim = Style::new().fg(Color::DarkGray);
        match (self.tab, node.kind) {
            (Tab::Includes, NodeKind::Dir) => {
                spans.push(Span::styled(format!("  {} headers", node.hdr_total), dim))
            }
            (_, NodeKind::Dir) if node.locked => spans.push(Span::styled("  (ignored)", dim)),
            (_, NodeKind::Dir) => {
                let text = format!("  {}/{} files", computed.in_count[index], node.src_total);
                spans.push(Span::styled(text, dim));
            }
            (_, NodeKind::Source) if node.has_main => {
                spans.push(Span::styled(
                    "  main()",
                    Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ));
            }
            _ => {}
        }
        ListItem::new(Line::from(spans))
    }

    fn row_marker(&self, index: usize, computed: &Computed) -> (&'static str, Color) {
        let node = &self.model.nodes[index];
        if self.tab == Tab::Includes {
            return match node.kind {
                NodeKind::Header => ("   ", Color::Cyan),
                _ if self.model.include[index] => (checkbox(true), Color::Green),
                _ => (checkbox(false), Color::Gray),
            };
        }
        if node.locked {
            return ("   ", Color::DarkGray);
        }
        match self.model.state(computed, index) {
            State::In => ("[x]", Color::Green),
            State::Out => ("[ ]", Color::Red),
            State::Partial => ("[~]", Color::Yellow),
        }
    }

    fn source_details(&self, computed: &Computed) -> Vec<Line<'static>> {
        let index = self.sources.cursor;
        let node = &self.model.nodes[index];
        let mut lines = vec![Line::styled(
            node.rel.clone(),
            Style::new().add_modifier(Modifier::BOLD),
        )];
        if node.locked {
            lines.push(Line::styled(
                "ignored by the scanner",
                Style::new().fg(Color::DarkGray),
            ));
            return lines;
        }
        let state = self.model.state(computed, index);
        let text = match state {
            State::In => "compiled",
            State::Out => "excluded",
            State::Partial => "partly compiled",
        };
        lines.push(Line::styled(text, Style::new().fg(state_color(state))));
        if node.kind == NodeKind::Dir {
            lines.push(Line::raw(format!(
                "{} of {} sources compiled",
                computed.in_count[index], node.src_total
            )));
        }
        if node.has_main {
            lines.push(Line::styled(
                "contains main()",
                Style::new().fg(Color::Yellow),
            ));
        }
        lines
    }

    fn include_details(&self) -> Vec<Line<'static>> {
        let index = self.includes.cursor;
        let node = &self.model.nodes[index];
        let mut lines = vec![Line::styled(
            node.rel.clone(),
            Style::new().add_modifier(Modifier::BOLD),
        )];
        if node.kind == NodeKind::Dir {
            let on = self.model.include[index];
            let text = if on {
                "on the include path (-I)"
            } else {
                "not on the include path"
            };
            lines.push(Line::styled(text, Style::new().fg(toggle_color(on))));
            lines.push(Line::raw(format!("{} headers inside", node.hdr_total)));
        } else {
            lines.push(Line::styled("header file", Style::new().fg(Color::Cyan)));
        }
        let export = if self.model.export { "yes" } else { "no" };
        lines.push(Line::raw(format!(
            "exported to dependents: {export} (press e)"
        )));
        lines
    }

    fn draw_review(&mut self, frame: &mut Frame, area: Rect) {
        let [text_area, button_area] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(4)]).areas(area);
        let lines = toml_lines(&self.model.result_text());
        let visible = text_area.height.saturating_sub(2) as usize;
        self.review_scroll = self
            .review_scroll
            .min(lines.len().saturating_sub(visible) as u16);
        let manifest = Paragraph::new(lines)
            .scroll((self.review_scroll, 0))
            .block(Block::bordered().title(" nomake.toml "));
        frame.render_widget(manifest, text_area);

        let mut button = vec![Line::styled(
            "[ Write nomake.toml ]  (Enter / Ctrl+S)",
            Style::new()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )];
        if let (true, Some(parent_toml)) = (self.model.register, &self.model.parent_toml) {
            button.push(Line::raw(format!(
                "adds '{}' to {}",
                self.model.name,
                parent_toml.display()
            )));
        }
        frame.render_widget(
            Paragraph::new(button)
                .alignment(Alignment::Center)
                .block(Block::bordered()),
            button_area,
        );
        self.write_button = button_area;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn press_add(app: &mut App) {
        app.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
    }

    #[test]
    fn add_prompt_lists_an_unconfigured_folder_and_opens_it() {
        let root = std::env::temp_dir().join(format!("nomake_tui_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("deps/raw")).unwrap();
        std::fs::write(root.join("main.cpp"), "int main() {}").unwrap();
        let mut model = Model::scan(&root, None);
        let mut app = App::new(&mut model);
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();

        press_add(&mut app);
        for typed in "deps/raw".chars() {
            press(&mut app, KeyCode::Char(typed));
        }
        terminal.draw(|frame| app.draw(frame)).unwrap();
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.open_dep, Some(0));
        assert!(!app.model.deps[0].on);

        press_add(&mut app);
        for typed in "nowhere".chars() {
            press(&mut app, KeyCode::Char(typed));
        }
        press(&mut app, KeyCode::Enter);
        assert!(app.adding.is_some() && !app.add_error.is_empty());
        terminal.draw(|frame| app.draw(frame)).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn registry_tab_searches_adds_and_edits() {
        let _env = crate::registry::ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = std::env::temp_dir().join(format!("nomake_tui_registry_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let registry = root.join("registry");
        let rev = "a".repeat(40);
        for (name, version, extra) in [
            ("lua", "5.4.7", ""),
            ("lua", "5.3.6", ""),
            (
                "imgui",
                "1.90",
                "[features]\ndefault = [\"dx11\"]\n[feature.dx11]\n[feature.opengl]\n",
            ),
        ] {
            let dir = registry.join(name).join(version);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("nomake.toml"), format!("[package]\nname = \"{name}\"\n[source]\ngit = \"https://x/{name}\"\nrev = \"{rev}\"\n{extra}")).unwrap();
        }
        std::env::set_var("NOMAKE_REGISTRY", &registry);
        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::write(root.join("app/main.cpp"), "int main() {}").unwrap();
        let mut model = Model::scan(&root.join("app"), None);
        let mut app = App::new(&mut model);
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();

        for _ in 0..3 {
            press(&mut app, KeyCode::Tab);
        }
        assert!(app.tab == Tab::Registry && app.registry.needs_load());
        terminal.draw(|frame| app.draw(frame)).unwrap();
        app.registry.load();
        for typed in "imgu".chars() {
            press(&mut app, KeyCode::Char(typed));
        }
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.model.deps[0].version.as_deref(), Some("1.90"));
        press(&mut app, KeyCode::Right);
        for _ in 0..4 {
            press(&mut app, KeyCode::Down);
        }
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.model.deps[0].features, ["opengl"]);
        terminal.draw(|frame| app.draw(frame)).unwrap();
        press(&mut app, KeyCode::Esc);
        assert!(!app.confirm_quit);
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Esc);
        assert!(app.confirm_quit);
        std::fs::remove_dir_all(root).unwrap();
    }
}
