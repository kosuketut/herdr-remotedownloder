//! One download window owns both the read-only source snapshot and file list.

use std::path::Path;

use anyhow::{Context, Result};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use herdr_remote_download::resolve_existing_file;
use herdr_tiny_fingers::app::{App, Outcome};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Panel {
    Context,
    Files,
}

#[derive(Clone, Copy, Debug)]
struct FileRows {
    target_index: usize,
    start: usize,
    end: usize,
}

pub struct Picker {
    // Both displays retain the original App target/hint order. The sender
    // continues to receive original target text, including any line suffix.
    paths: Vec<String>,
    scroll: usize,
    page_height: usize,
    row_count: usize,
    file_width: u16,
    visible_files: Vec<FileRows>,
    focused_target: Option<usize>,
    detail_scroll: usize,
    context_scroll: usize,
    context_page_height: usize,
    context_row_count: usize,
    active_panel: Panel,
    context_area: Rect,
    files_area: Rect,
    refresh_area: Rect,
}

impl Picker {
    pub fn new(app: &App, cwd: &Path) -> Result<Self> {
        let paths = app
            .targets
            .iter()
            .map(|target| {
                resolve_existing_file(&target.target.text, cwd)
                    .with_context(|| {
                        format!(
                            "file disappeared while opening picker: {}",
                            target.target.text
                        )
                    })
                    .map(|path| path.to_string_lossy().into_owned())
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            paths,
            scroll: 0,
            page_height: 0,
            row_count: 0,
            file_width: 80,
            visible_files: Vec::new(),
            focused_target: (!app.targets.is_empty()).then_some(0),
            detail_scroll: 0,
            context_scroll: 0,
            context_page_height: 0,
            context_row_count: 0,
            active_panel: Panel::Files,
            context_area: Rect::default(),
            files_area: Rect::default(),
            refresh_area: Rect::default(),
        })
    }

    pub fn handle_key(&mut self, app: &mut App, key: KeyEvent) -> Outcome {
        if key.kind == KeyEventKind::Release {
            return Outcome::Continue;
        }
        self.refresh_file_layout(app);
        if (self.page_height == 0 || self.file_width == 0)
            && (key.code == KeyCode::Enter
                || (self.active_panel == Panel::Files
                    && app.multi_mode
                    && key.code == KeyCode::Char(' ')))
        {
            app.message =
                Some("Enlarge the window to display the file before selecting it".to_string());
            return Outcome::Continue;
        }
        match key.code {
            KeyCode::Left => {
                self.active_panel = Panel::Context;
                app.message = None;
            }
            KeyCode::Right => {
                self.active_panel = Panel::Files;
                app.message = None;
            }
            KeyCode::Up | KeyCode::Down
                if self.active_panel == Panel::Files
                    && key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.detail_scroll = if key.code == KeyCode::Up {
                    self.detail_scroll.saturating_sub(1)
                } else {
                    self.detail_scroll.saturating_add(1)
                };
                self.ensure_focus_visible();
            }
            KeyCode::Up
            | KeyCode::Down
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Home
            | KeyCode::End => self.scroll_panel(key.code),
            KeyCode::Enter if !app.multi_mode => {
                if self.active_panel == Panel::Files {
                    if let Some(index) = self.focused_target {
                        return Outcome::Copy(app.targets[index].target.text.clone());
                    }
                } else {
                    app.message =
                        Some("Press Right to choose a file, then Enter to download".to_string());
                }
            }
            KeyCode::Char(' ') if app.multi_mode && self.active_panel == Panel::Files => {
                if let Some(index) = self.focused_target {
                    let hint = &app.targets[index].hint;
                    if let Some(selected) =
                        app.selected_hints.iter().position(|value| value == hint)
                    {
                        app.selected_hints.remove(selected);
                    } else {
                        app.selected_hints.push(hint.clone());
                    }
                    app.message = None;
                }
            }
            _ => {
                let character = if app.multi_mode && key.code == KeyCode::Enter {
                    Some('\t')
                } else {
                    super::key_to_char(key)
                };
                if let Some(character) = character {
                    let old_input = app.input.clone();
                    let outcome = app.handle_char(character);
                    if old_input != app.input {
                        self.scroll = 0;
                        self.detail_scroll = 0;
                    }
                    self.refresh_file_layout(app);
                    return outcome;
                }
            }
        }
        Outcome::Continue
    }

    fn scroll_panel(&mut self, key: KeyCode) {
        if self.active_panel == Panel::Files {
            self.move_file_focus(key);
            return;
        }
        let (scroll, height, count) = (
            &mut self.context_scroll,
            self.context_page_height,
            self.context_row_count,
        );
        let maximum = count.saturating_sub(height);
        *scroll = match key {
            KeyCode::Up => scroll.saturating_sub(1),
            KeyCode::Down => scroll.saturating_add(1).min(maximum),
            KeyCode::PageUp => scroll.saturating_sub(height.max(1)),
            KeyCode::PageDown => scroll.saturating_add(height.max(1)).min(maximum),
            KeyCode::Home => 0,
            KeyCode::End => maximum,
            _ => *scroll,
        };
    }

    fn move_file_focus(&mut self, key: KeyCode) {
        if self.visible_files.is_empty() {
            self.focused_target = None;
            return;
        }
        let current = self
            .visible_files
            .iter()
            .position(|file| Some(file.target_index) == self.focused_target)
            .unwrap_or(0);
        let last = self.visible_files.len() - 1;
        let row = self.visible_files[current].start;
        let next = match key {
            KeyCode::Up => current.saturating_sub(1),
            KeyCode::Down => (current + 1).min(last),
            KeyCode::Home => 0,
            KeyCode::End => last,
            KeyCode::PageUp => self.visible_files[..current]
                .iter()
                .rposition(|file| file.start <= row.saturating_sub(self.page_height.max(1)))
                .unwrap_or(0),
            KeyCode::PageDown => self
                .visible_files
                .iter()
                .enumerate()
                .skip(current + 1)
                .find(|(_, file)| file.start >= row.saturating_add(self.page_height.max(1)))
                .map_or(last, |(index, _)| index),
            _ => current,
        };
        self.focused_target = Some(self.visible_files[next].target_index);
        self.detail_scroll = 0;
        self.ensure_focus_visible();
    }

    fn refresh_file_layout(&mut self, app: &App) -> Vec<Line<'static>> {
        // Filtering changes the visible subset, never target indices or hints.
        let visible = |index: usize| {
            app.targets
                .get(index)
                .is_some_and(|target| target.hint.starts_with(&app.input))
        };
        if !self.focused_target.is_some_and(visible) {
            self.focused_target = (0..app.targets.len()).find(|&index| visible(index));
            self.detail_scroll = 0;
        }
        let (rows, files) = self.file_layout(app, self.file_width);
        self.visible_files = files;
        self.row_count = rows.len();
        self.ensure_focus_visible();
        rows
    }

    fn ensure_focus_visible(&mut self) {
        if let Some(file) = self
            .visible_files
            .iter()
            .find(|file| Some(file.target_index) == self.focused_target)
        {
            let extra_rows = (file.end - file.start).saturating_sub(self.page_height);
            self.detail_scroll = self.detail_scroll.min(extra_rows);
            // The border pins the focused hint while Shift+arrows reveal the
            // remainder of a path taller than the viewport.
            if extra_rows > 0 {
                self.scroll = file.start + self.detail_scroll;
            } else if file.start < self.scroll {
                self.scroll = file.start;
            } else if file.end > self.scroll.saturating_add(self.page_height) {
                self.scroll = file.end.saturating_sub(self.page_height);
            }
        }
        self.scroll = self
            .scroll
            .min(self.row_count.saturating_sub(self.page_height));
    }

    pub fn refresh_clicked(&self, event: MouseEvent) -> bool {
        event.kind == MouseEventKind::Down(MouseButton::Left)
            && event.column >= self.refresh_area.x
            && event.column < self.refresh_area.right()
            && event.row >= self.refresh_area.y
            && event.row < self.refresh_area.bottom()
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> bool {
        if self.refresh_clicked(event) {
            return false;
        }
        let previous_panel = self.active_panel;
        let inside = |area: Rect| {
            event.column >= area.x
                && event.column < area.right()
                && event.row >= area.y
                && event.row < area.bottom()
        };
        let panel = if inside(self.context_area) {
            Some(Panel::Context)
        } else if inside(self.files_area) {
            Some(Panel::Files)
        } else {
            None
        };
        if let Some(panel) = panel {
            match event.kind {
                MouseEventKind::Down(MouseButton::Left) => self.active_panel = panel,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    self.active_panel = panel;
                    self.scroll_panel(if event.kind == MouseEventKind::ScrollUp {
                        KeyCode::Up
                    } else {
                        KeyCode::Down
                    });
                }
                _ => {}
            }
        }
        self.active_panel != previous_panel
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>, app: &App) {
        let area = frame.area();
        self.refresh_area = Rect::default();
        if area.width == 0 || area.height == 0 {
            self.page_height = 0;
            self.file_width = 0;
            self.context_area = Rect::default();
            self.files_area = Rect::default();
            return;
        }
        let body = Rect {
            height: area.height.saturating_sub(1),
            ..area
        };
        let (context_area, files_area) = if body.width >= 64 {
            let files_width = (body.width * 3 / 10).max(28);
            let context_width = body.width - files_width;
            (
                Rect {
                    width: context_width,
                    ..body
                },
                Rect {
                    x: body.x + context_width,
                    width: files_width,
                    ..body
                },
            )
        } else {
            let context_height = body.height / 2;
            (
                Rect {
                    height: context_height,
                    ..body
                },
                Rect {
                    y: body.y + context_height,
                    height: body.height - context_height,
                    ..body
                },
            )
        };
        self.context_area = context_area;
        self.files_area = files_area;
        let context_block = Block::bordered()
            .title(" Conversation snapshot ")
            .border_style(self.border_style(Panel::Context));
        let files_inner = Block::bordered().inner(files_area);
        self.file_width = files_inner.width;
        self.page_height = usize::from(files_inner.height);
        let rows = self.refresh_file_layout(app);
        let mut files_block = Block::bordered()
            .title(" Files -> Mac Downloads ")
            .border_style(self.border_style(Panel::Files));
        if let Some(index) = self.focused_target {
            let title = format!(
                " [{}] path row {} ",
                app.targets[index].hint,
                self.detail_scroll + 1
            );
            files_block = files_block.title_bottom(title);
        }
        const REFRESH_LABEL: &str = "[Refresh ^R]";
        let refresh_width = REFRESH_LABEL.len() as u16;
        if files_area.height > 0
            && files_area.width >= " Files -> Mac Downloads ".len() as u16 + refresh_width + 3
        {
            self.refresh_area = Rect {
                x: files_area.right() - 1 - refresh_width,
                y: files_area.y,
                width: refresh_width,
                height: 1,
            };
            files_block = files_block.title_top(Line::from(REFRESH_LABEL).right_aligned());
        }
        let context_inner = context_block.inner(context_area);
        frame.render_widget(context_block, context_area);
        frame.render_widget(files_block, files_area);

        let context_rows = crate::context_view::render_rows(app, context_inner.width);
        self.context_page_height = usize::from(context_inner.height);
        self.context_row_count = context_rows.len();
        self.context_scroll = self.context_scroll.min(
            self.context_row_count
                .saturating_sub(self.context_page_height),
        );
        frame.render_widget(
            Paragraph::new(
                context_rows
                    .into_iter()
                    .skip(self.context_scroll)
                    .take(self.context_page_height)
                    .collect::<Vec<_>>(),
            ),
            context_inner,
        );

        frame.render_widget(
            Paragraph::new(
                rows.into_iter()
                    .skip(self.scroll)
                    .take(self.page_height)
                    .collect::<Vec<_>>(),
            ),
            files_inner,
        );
        self.draw_status(frame, app, area);
    }

    fn border_style(&self, panel: Panel) -> Style {
        Style::default().fg(if self.active_panel == panel {
            Color::Cyan
        } else {
            Color::DarkGray
        })
    }

    #[cfg(test)]
    fn rows(&self, app: &App, width: u16) -> Vec<Line<'static>> {
        self.file_layout(app, width).0
    }

    fn file_layout(&self, app: &App, width: u16) -> (Vec<Line<'static>>, Vec<FileRows>) {
        if app.targets.is_empty() {
            return (
                wrap_line(
                    Line::from(Span::styled(
                        "No existing paths. Show a full path, then reopen.",
                        app.theme.empty_style(),
                    )),
                    width,
                ),
                Vec::new(),
            );
        }
        let mut rows = Vec::new();
        let mut files = Vec::new();
        for (index, (target, path)) in app.targets.iter().zip(&self.paths).enumerate() {
            if !target.hint.starts_with(&app.input) {
                continue;
            }
            let selected = app.is_selected_hint(&target.hint);
            let mut spans = vec![
                Span::styled(
                    if self.focused_target == Some(index) {
                        "> "
                    } else {
                        "  "
                    },
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("[{}]", target.hint), app.theme.hint_style(selected)),
                Span::raw(" "),
            ];
            if app.multi_mode {
                spans.push(Span::raw(if selected { "[x] " } else { "[ ] " }));
            }
            spans.push(Span::styled(
                path.clone(),
                app.theme.match_style(selected).add_modifier(Modifier::BOLD),
            ));
            let start = rows.len();
            rows.extend(wrap_line(Line::from(spans), width));
            files.push(FileRows {
                target_index: index,
                start,
                end: rows.len(),
            });
            rows.push(Line::default());
        }
        rows.pop();
        (rows, files)
    }

    fn draw_status(&self, frame: &mut Frame<'_>, app: &App, area: Rect) {
        let mode = if app.multi_mode {
            format!("multi:{}", app.selected_target_count())
        } else {
            "single".to_string()
        };
        let input = if app.input.is_empty() {
            "-"
        } else {
            &app.input
        };
        let action = match (app.multi_mode, self.active_panel) {
            (true, Panel::Files) => "Space:toggle Tab/Enter:download",
            (true, Panel::Context) => "Tab/Enter:download Right:files",
            (false, Panel::Files) => "hint/Enter:download Tab:multi",
            (false, Panel::Context) => "hint:download Right:files Tab:multi",
        };
        let short_action = if app.multi_mode || self.active_panel == Panel::Files {
            "Enter:download"
        } else {
            "Right:files"
        };
        let panel = match self.active_panel {
            Panel::Context => "context",
            Panel::Files => "files",
        };
        let navigation = match self.active_panel {
            Panel::Context => "Up/Down:scroll",
            Panel::Files => "Up/Down:select",
        };
        let path_help = if self.active_panel == Panel::Files {
            " Shift+Up/Down:path"
        } else {
            ""
        };
        let variants = [
            format!(" {mode} {action} Left/Right:panel {navigation} PgUp/PgDn/Home/End:move{path_help} Ctrl+R:refresh focus:{panel} in:{input} Esc:close downloader"),
            format!(" {action} Left/Right:panel {navigation}{path_help} Ctrl+R:refresh Esc:close"),
            format!(" {short_action} arrows:move{path_help} Ctrl+R:refresh Esc:close"),
            format!(" {short_action} arrows:move ^R:refresh Esc:close"),
        ];
        let status = app.message.clone().unwrap_or_else(|| {
            variants
                .into_iter()
                .find(|text| Line::from(text.as_str()).width() <= usize::from(area.width))
                .unwrap_or_else(|| " Esc:close".to_string())
        });
        frame.render_widget(
            Paragraph::new(status).style(app.theme.status_style()),
            Rect {
                y: area.y + area.height - 1,
                height: 1,
                ..area
            },
        );
    }
}

/// Wrap at terminal cell boundaries without word wrapping: a space is part of a
/// filename and must not be discarded at a line boundary. Ratatui's grapheme
/// iterator and width calculation also keep CJK and combining characters intact.
pub(crate) fn wrap_line(line: Line<'_>, width: u16) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let mut rows = Vec::new();
    let mut spans = Vec::new();
    let mut used = 0;
    for grapheme in line.styled_graphemes(Style::default()) {
        let cells = Span::raw(grapheme.symbol).width();
        if used + cells > usize::from(width) && !spans.is_empty() {
            rows.push(Line::from(std::mem::take(&mut spans)));
            used = 0;
        }
        spans.push(Span::styled(grapheme.symbol.to_string(), grapheme.style));
        used += cells;
    }
    if !spans.is_empty() {
        rows.push(Line::from(spans));
    }
    rows
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crossterm::event::KeyModifiers;
    use herdr_remote_download::{file_matcher, filter_existing_file_targets};
    use herdr_tiny_fingers::theme::Theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use super::*;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "herdr-picker-list-{}-{}",
                std::process::id(),
                NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn area_rows(terminal: &Terminal<TestBackend>, area: Rect) -> Vec<String> {
        let buffer = terminal.backend().buffer();
        (area.y..area.bottom())
            .map(|y| {
                let mut line = String::new();
                let mut x = area.x;
                while x < area.right() {
                    let symbol = buffer[(x, y)].symbol();
                    line.push_str(symbol);
                    x += Span::raw(symbol).width().max(1) as u16;
                }
                line
            })
            .collect()
    }

    fn screen_rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
        area_rows(terminal, terminal.backend().buffer().area)
    }

    fn file_rows(terminal: &Terminal<TestBackend>, picker: &Picker) -> Vec<String> {
        area_rows(terminal, Block::bordered().inner(picker.files_area))
    }

    fn assert_rendered_file(
        terminal: &Terminal<TestBackend>,
        picker: &Picker,
        app: &App,
        index: usize,
        expected: &str,
    ) {
        let (rows, files) = picker.file_layout(app, picker.file_width);
        let file = files
            .iter()
            .find(|file| file.target_index == index)
            .expect("target belongs to the filtered catalog");
        // A real temp path can span multiple terminal rows (especially under
        // macOS TMPDIR). Reconstruct all of it without removing filename spaces.
        let rendered = rows[file.start..file.end]
            .iter()
            .flat_map(|row| row.spans.iter().map(|span| span.content.as_ref()))
            .collect::<String>();
        assert_eq!(rendered, expected);

        // Also inspect TestBackend, including the separately rendered hint.
        // Paths taller than the viewport need not be visible in their entirety.
        assert!(file.start >= picker.scroll);
        assert!(file.start < picker.scroll + picker.page_height);
        let screen = file_rows(terminal, picker);
        for row in file.start..file.end.min(picker.scroll + picker.page_height) {
            let expected_row = rows[row]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            let actual_row = &screen[row - picker.scroll];
            assert!(actual_row.starts_with(&expected_row));
            assert!(actual_row[expected_row.len()..].chars().all(|c| c == ' '));
        }
    }

    fn assert_focused_file_visible(picker: &Picker) {
        let file = picker
            .visible_files
            .iter()
            .find(|file| Some(file.target_index) == picker.focused_target)
            .expect("focused target belongs to the filtered catalog");
        assert!(
            file.start >= picker.scroll,
            "focus cursor scrolled out above viewport"
        );
        assert!(
            file.start < picker.scroll + picker.page_height,
            "focus cursor scrolled out below viewport"
        );
    }

    fn key(picker: &mut Picker, app: &mut App, code: KeyCode) -> Outcome {
        picker.handle_key(app, KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn fixture(count: usize) -> (TestDirectory, App, Picker) {
        let directory = TestDirectory::new();
        let filenames = (0..count)
            .map(|index| format!("result-{index:02}.md"))
            .collect::<Vec<_>>();
        for filename in &filenames {
            fs::write(directory.0.join(filename), b"fixture").unwrap();
        }
        let mut app = App::from_text_with_theme(
            &filenames.join(" "),
            &file_matcher().unwrap(),
            Theme::default(),
        );
        filter_existing_file_targets(&mut app, &directory.0);
        let picker = Picker::new(&app, &directory.0).unwrap();
        (directory, app, picker)
    }

    #[test]
    fn hint_is_separate_from_complete_resolved_path_and_selects_the_same_target() {
        let directory = TestDirectory::new();
        fs::write(directory.0.join("RESULTS.md"), b"fixture").unwrap();
        let mut app =
            App::from_text_with_theme("RESULTS.md:93", &file_matcher().unwrap(), Theme::default());
        filter_existing_file_targets(&mut app, &directory.0);
        let mut picker = Picker::new(&app, &directory.0).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(260, 12)).unwrap();
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        let screen = screen_rows(&terminal).join("\n");
        assert_rendered_file(
            &terminal,
            &picker,
            &app,
            0,
            &format!("> [a] {}/RESULTS.md", directory.0.display()),
        );
        assert!(screen.contains("Files -> Mac Downloads"));
        assert!(screen.contains("Conversation snapshot"));
        assert!(!file_rows(&terminal, &picker)
            .join("\n")
            .contains("RESULTS.md:93"));
        assert!(screen.contains("[a] RESULTS.md:93"));
        assert_eq!(
            key(&mut picker, &mut app, KeyCode::Char('a')),
            Outcome::Copy("RESULTS.md:93".to_string())
        );
    }

    #[test]
    fn long_unicode_paths_and_filename_spaces_wrap_without_losing_characters() {
        let (_directory, app, mut picker) = fixture(1);
        // Feed presentation paths directly: matching quoted/space paths is a
        // separate parser concern; the list must preserve any accepted target.
        picker.paths[0] = "/home/me/研究  文档/résumé long name/RESULTS.md".to_string();
        let mut terminal = Terminal::new(TestBackend::new(26, 12)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new(picker.rows(&app, frame.area().width)),
                    frame.area(),
                )
            })
            .unwrap();
        let rows = screen_rows(&terminal);
        assert!(rows.iter().any(|row| row == "> [a] /home/me/研究  文档/"));
        assert!(rows
            .iter()
            .any(|row| row.trim_end() == "résumé long name/RESULTS.m"));
        assert!(rows.iter().any(|row| row.trim_end() == "d"));

        picker.paths[0] = "/tmp/long  name/file.md".to_string();
        let mut terminal = Terminal::new(TestBackend::new(16, 12)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new(picker.rows(&app, frame.area().width)),
                    frame.area(),
                )
            })
            .unwrap();
        let rows = screen_rows(&terminal);
        assert!(rows.iter().any(|row| row == "> [a] /tmp/long "));
        assert!(rows.iter().any(|row| row.trim_end() == " name/file.md"));
        let reconstructed = picker
            .rows(&app, 16)
            .iter()
            .flat_map(|row| row.spans.iter().map(|span| span.content.as_ref()))
            .collect::<String>();
        assert_eq!(reconstructed, "> [a] /tmp/long  name/file.md");
    }

    #[test]
    fn scrolling_exposes_later_files_without_reassigning_their_hints() {
        let (directory, mut app, mut picker) = fixture(40);
        let mut terminal = Terminal::new(TestBackend::new(260, 8)).unwrap();
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        assert!(!file_rows(&terminal, &picker)
            .join("\n")
            .contains("result-39.md"));
        assert_eq!(key(&mut picker, &mut app, KeyCode::Down), Outcome::Continue);
        assert_eq!(picker.focused_target, Some(1));
        assert_eq!(key(&mut picker, &mut app, KeyCode::Up), Outcome::Continue);
        assert_eq!(picker.focused_target, Some(0));
        assert_eq!(picker.scroll, 0);
        key(&mut picker, &mut app, KeyCode::Down);
        assert_eq!(picker.focused_target, Some(1));
        assert_eq!(
            key(&mut picker, &mut app, KeyCode::PageUp),
            Outcome::Continue
        );
        assert_eq!(picker.focused_target, Some(0));
        assert_eq!(picker.scroll, 0);
        while picker.focused_target != Some(39) {
            key(&mut picker, &mut app, KeyCode::PageDown);
        }
        key(&mut picker, &mut app, KeyCode::Home);
        assert_eq!(picker.focused_target, Some(0));
        assert_eq!(picker.scroll, 0);
        key(&mut picker, &mut app, KeyCode::End);
        assert_eq!(picker.focused_target, Some(39));
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        let last = &app.targets[39];
        assert_rendered_file(
            &terminal,
            &picker,
            &app,
            39,
            &format!(
                "> [{}] {}",
                last.hint,
                directory.0.join("result-39.md").display()
            ),
        );
        let hint = last.hint.clone();
        let mut outcome = Outcome::Continue;
        for character in hint.chars() {
            outcome = key(&mut picker, &mut app, KeyCode::Char(character));
        }
        assert_eq!(outcome, Outcome::Copy("result-39.md".to_string()));
    }

    #[test]
    fn resizing_clamps_scroll_and_preserves_the_last_target_hint() {
        let (directory, mut app, mut picker) = fixture(40);
        let hint = app.targets[39].hint.clone();
        let displayed_target = format!("> [{hint}] {}", directory.0.join("result-39.md").display());
        let mut terminal = Terminal::new(TestBackend::new(36, 8)).unwrap();
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        key(&mut picker, &mut app, KeyCode::End);
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        let narrow_scroll = picker.scroll;

        terminal.backend_mut().resize(260, 20);
        terminal.autoresize().unwrap();
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        assert!(picker.scroll < narrow_scroll);
        let last = picker.visible_files.last().unwrap();
        if last.end - last.start <= picker.page_height {
            assert_eq!(picker.scroll + picker.page_height, picker.row_count);
        } else {
            assert_eq!(picker.scroll, last.start);
        }
        assert_rendered_file(&terminal, &picker, &app, 39, &displayed_target);

        terminal.backend_mut().resize(36, 8);
        terminal.autoresize().unwrap();
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        key(&mut picker, &mut app, KeyCode::End);
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        assert_rendered_file(&terminal, &picker, &app, 39, &displayed_target);
        assert_eq!(picker.focused_target, Some(39));
        assert_eq!(
            key(&mut picker, &mut app, KeyCode::Enter),
            Outcome::Copy("result-39.md".to_string())
        );
    }

    #[test]
    fn multi_selection_marks_paths_and_enter_sends_original_selection_order() {
        let (directory, mut app, mut picker) = fixture(3);
        let first = app.targets[0].hint.clone();
        let last = app.targets[2].hint.clone();
        assert_eq!(key(&mut picker, &mut app, KeyCode::Tab), Outcome::Continue);
        for hint in [&last, &first] {
            for character in hint.chars() {
                assert_eq!(
                    key(&mut picker, &mut app, KeyCode::Char(character)),
                    Outcome::Continue
                );
            }
        }
        let mut terminal = Terminal::new(TestBackend::new(260, 12)).unwrap();
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        key(&mut picker, &mut app, KeyCode::Home);
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        assert_rendered_file(
            &terminal,
            &picker,
            &app,
            0,
            &format!(
                "> [{first}] [x] {}",
                directory.0.join("result-00.md").display()
            ),
        );
        key(&mut picker, &mut app, KeyCode::End);
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        assert_rendered_file(
            &terminal,
            &picker,
            &app,
            2,
            &format!(
                "> [{last}] [x] {}",
                directory.0.join("result-02.md").display()
            ),
        );
        assert!(screen_rows(&terminal)
            .join("\n")
            .contains("Tab/Enter:download"));
        assert_eq!(
            key(&mut picker, &mut app, KeyCode::Enter),
            Outcome::Copy("result-02.md\nresult-00.md".to_string())
        );
    }

    #[test]
    fn hint_prefix_filters_the_list_and_release_events_do_not_select_twice() {
        let (directory, mut app, mut picker) = fixture(3);
        for (target, hint) in app.targets.iter_mut().zip(["aa", "as", "sa"]) {
            target.hint = hint.to_string();
        }
        assert_eq!(
            key(&mut picker, &mut app, KeyCode::Char('s')),
            Outcome::Continue
        );
        let mut terminal = Terminal::new(TestBackend::new(260, 12)).unwrap();
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        let screen = file_rows(&terminal, &picker).join("\n");
        assert_rendered_file(
            &terminal,
            &picker,
            &app,
            2,
            &format!("> [sa] {}", directory.0.join("result-02.md").display()),
        );
        assert_eq!(picker.visible_files.len(), 1);
        assert_eq!(picker.visible_files[0].target_index, 2);
        assert!(!screen.contains("result-00.md"));
        assert_eq!(
            picker.handle_key(
                &mut app,
                KeyEvent::new_with_kind(
                    KeyCode::Char('a'),
                    KeyModifiers::NONE,
                    KeyEventKind::Release,
                )
            ),
            Outcome::Continue
        );
        assert_eq!(app.input, "s");
        assert_eq!(
            key(&mut picker, &mut app, KeyCode::Char('a')),
            Outcome::Copy("result-02.md".to_string())
        );
    }

    #[test]
    fn conversation_is_wider_and_its_scrolling_does_not_move_the_file_list() {
        let (_directory, mut app, mut picker) = fixture(40);
        app.lines.extend(std::iter::repeat_n(
            "More conversation context".to_string(),
            20,
        ));
        let mut terminal = Terminal::new(TestBackend::new(160, 12)).unwrap();
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        assert_eq!(picker.context_area.width, 112);
        assert_eq!(picker.files_area.width, 48);
        let files_scroll = picker.scroll;
        key(&mut picker, &mut app, KeyCode::Left);
        key(&mut picker, &mut app, KeyCode::End);
        assert!(picker.context_scroll > 0);
        assert_eq!(picker.scroll, files_scroll);
        key(&mut picker, &mut app, KeyCode::Right);
        key(&mut picker, &mut app, KeyCode::Down);
        assert_eq!(picker.focused_target, Some(1));
    }

    #[test]
    fn clicking_snapshot_keeps_escape_inside_the_downloader() {
        let (_directory, mut app, mut picker) = fixture(1);
        let mut terminal = Terminal::new(TestBackend::new(160, 12)).unwrap();
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        picker.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: picker.context_area.x + 2,
            row: picker.context_area.y + 2,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(picker.active_panel, Panel::Context);
        assert_eq!(key(&mut picker, &mut app, KeyCode::Esc), Outcome::Cancel);
        assert!(app.selected_hints.is_empty());
        // Both visual regions belong to this one picker; clicking source text
        // performs no Herdr operation and returns no text to the source app.
    }

    #[test]
    fn snapshot_enter_gives_guidance_and_status_matches_the_active_panel() {
        let (_directory, mut app, mut picker) = fixture(1);
        let mut terminal = Terminal::new(TestBackend::new(160, 12)).unwrap();
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        key(&mut picker, &mut app, KeyCode::Left);
        for width in [160, 70, 50] {
            terminal.backend_mut().resize(width, 12);
            terminal.autoresize().unwrap();
            terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
            let status = screen_rows(&terminal).pop().unwrap();
            assert!(status.contains("Right:files"));
            assert!(!status.contains("Enter:download"));
            assert!(!status.contains("Shift+Up/Down:path"));
        }
        assert_eq!(
            key(&mut picker, &mut app, KeyCode::Enter),
            Outcome::Continue
        );
        assert_eq!(
            app.message.as_deref(),
            Some("Press Right to choose a file, then Enter to download")
        );
        key(&mut picker, &mut app, KeyCode::Right);
        assert!(app.message.is_none());
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        assert!(screen_rows(&terminal)
            .pop()
            .unwrap()
            .contains("Enter:download"));
        assert_eq!(
            key(&mut picker, &mut app, KeyCode::Enter),
            Outcome::Copy("result-00.md".to_string())
        );
    }

    #[test]
    fn arrows_choose_files_and_enter_sends_only_the_focused_file() {
        let (_directory, mut app, mut picker) = fixture(4);
        let mut terminal = Terminal::new(TestBackend::new(160, 12)).unwrap();
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        key(&mut picker, &mut app, KeyCode::Down);
        key(&mut picker, &mut app, KeyCode::Down);
        key(&mut picker, &mut app, KeyCode::Up);
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        assert_eq!(picker.focused_target, Some(1));
        assert_focused_file_visible(&picker);
        assert!(file_rows(&terminal, &picker)
            .join("\n")
            .contains(&format!("> [{}]", app.targets[1].hint)));
        assert_eq!(
            key(&mut picker, &mut app, KeyCode::Enter),
            Outcome::Copy("result-01.md".to_string())
        );
        key(&mut picker, &mut app, KeyCode::Left);
        assert_eq!(
            key(&mut picker, &mut app, KeyCode::Enter),
            Outcome::Continue
        );
        key(&mut picker, &mut app, KeyCode::Right);
        assert_eq!(
            key(&mut picker, &mut app, KeyCode::Enter),
            Outcome::Copy("result-01.md".to_string())
        );
    }

    #[test]
    fn filtering_reconciles_focus_to_original_indices_before_enter() {
        let (_directory, mut app, mut picker) = fixture(3);
        for (target, hint) in app.targets.iter_mut().zip(["aa", "as", "sa"]) {
            target.hint = hint.to_string();
        }
        let mut terminal = Terminal::new(TestBackend::new(160, 12)).unwrap();
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        key(&mut picker, &mut app, KeyCode::End);
        assert_eq!(picker.focused_target, Some(2));
        key(&mut picker, &mut app, KeyCode::Char('a'));
        assert_eq!(picker.focused_target, Some(0));
        key(&mut picker, &mut app, KeyCode::Down);
        assert_eq!(picker.focused_target, Some(1));
        assert_eq!(
            key(&mut picker, &mut app, KeyCode::Enter),
            Outcome::Copy("result-01.md".to_string())
        );
        key(&mut picker, &mut app, KeyCode::Backspace);
        assert_eq!(picker.focused_target, Some(1));
        key(&mut picker, &mut app, KeyCode::Char('s'));
        assert_eq!(picker.focused_target, Some(2));
        assert_eq!(
            key(&mut picker, &mut app, KeyCode::Enter),
            Outcome::Copy("result-02.md".to_string())
        );
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        assert_focused_file_visible(&picker);
        assert_eq!(
            app.targets
                .iter()
                .map(|target| target.hint.as_str())
                .collect::<Vec<_>>(),
            ["aa", "as", "sa"]
        );
    }

    #[test]
    fn pages_resizes_and_mouse_wheel_keep_enter_target_visible() {
        let (_directory, mut app, mut picker) = fixture(40);
        let mut terminal = Terminal::new(TestBackend::new(160, 12)).unwrap();
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        for code in [
            KeyCode::PageDown,
            KeyCode::PageDown,
            KeyCode::PageUp,
            KeyCode::End,
            KeyCode::Home,
        ] {
            key(&mut picker, &mut app, code);
            terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
            assert_focused_file_visible(&picker);
            let index = picker.focused_target.unwrap();
            assert_eq!(
                key(&mut picker, &mut app, KeyCode::Enter),
                Outcome::Copy(app.targets[index].target.text.clone())
            );
        }
        for size in [(36, 8), (260, 20), (160, 5)] {
            key(&mut picker, &mut app, KeyCode::End);
            terminal.backend_mut().resize(size.0, size.1);
            terminal.autoresize().unwrap();
            terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
            assert_focused_file_visible(&picker);
            assert_eq!(picker.focused_target, Some(39));
        }
        picker.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: picker.files_area.x + 2,
            row: picker.files_area.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        assert_eq!(picker.focused_target, Some(38));
        assert_focused_file_visible(&picker);
        assert_eq!(
            key(&mut picker, &mut app, KeyCode::Enter),
            Outcome::Copy("result-38.md".to_string())
        );
    }

    #[test]
    fn space_toggles_focused_multi_selection_and_empty_batch_does_not_send() {
        let (_directory, mut app, mut picker) = fixture(3);
        let mut terminal = Terminal::new(TestBackend::new(160, 12)).unwrap();
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        key(&mut picker, &mut app, KeyCode::Tab);
        assert_eq!(
            key(&mut picker, &mut app, KeyCode::Enter),
            Outcome::Continue
        );
        key(&mut picker, &mut app, KeyCode::Down);
        key(&mut picker, &mut app, KeyCode::Char(' '));
        key(&mut picker, &mut app, KeyCode::Up);
        key(&mut picker, &mut app, KeyCode::Char(' '));
        assert_eq!(
            key(&mut picker, &mut app, KeyCode::Enter),
            Outcome::Copy("result-01.md\nresult-00.md".to_string())
        );
        key(&mut picker, &mut app, KeyCode::Char(' '));
        key(&mut picker, &mut app, KeyCode::Down);
        key(&mut picker, &mut app, KeyCode::Char(' '));
        assert!(app.selected_hints.is_empty());
        assert_eq!(
            key(&mut picker, &mut app, KeyCode::Enter),
            Outcome::Continue
        );
        assert_eq!(key(&mut picker, &mut app, KeyCode::Tab), Outcome::Continue);
    }

    #[test]
    fn empty_catalog_and_empty_filter_have_no_enter_target() {
        let (_directory, mut app, mut picker) = fixture(0);
        let mut terminal = Terminal::new(TestBackend::new(160, 12)).unwrap();
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        assert!(file_rows(&terminal, &picker)
            .join("\n")
            .contains("No existing paths."));
        for code in [
            KeyCode::Down,
            KeyCode::Up,
            KeyCode::PageDown,
            KeyCode::PageUp,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::Enter,
        ] {
            assert_eq!(key(&mut picker, &mut app, code), Outcome::Continue);
            assert_eq!(picker.focused_target, None);
        }
        key(&mut picker, &mut app, KeyCode::Tab);
        key(&mut picker, &mut app, KeyCode::Char(' '));
        assert_eq!(
            key(&mut picker, &mut app, KeyCode::Enter),
            Outcome::Continue
        );

        let (_directory, mut app, mut picker) = fixture(1);
        app.input = "zz".to_string();
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        assert_eq!(picker.focused_target, None);
        assert_eq!(
            key(&mut picker, &mut app, KeyCode::Enter),
            Outcome::Continue
        );
    }

    #[test]
    fn shifted_arrows_reveal_long_path_tail_with_focused_hint_pinned() {
        let (_directory, mut app, mut picker) = fixture(2);
        picker.paths[0] = format!("/tmp/{}wanted-final-name.md", "long-directory/".repeat(12));
        let mut terminal = Terminal::new(TestBackend::new(160, 5)).unwrap();
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        assert_eq!(picker.page_height, 2);
        assert!(!file_rows(&terminal, &picker)
            .join("\n")
            .contains("wanted-final-name.md"));
        let hint = app.targets[0].hint.clone();
        for _ in 0..20 {
            picker.handle_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT));
            terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
            assert_eq!(picker.focused_target, Some(0));
            assert!(screen_rows(&terminal)
                .join("\n")
                .contains(&format!("[{hint}] path row")));
        }
        assert!(picker.detail_scroll > 0);
        let tail = file_rows(&terminal, &picker)
            .iter()
            .map(|row| row.trim_end())
            .collect::<String>();
        assert!(tail.contains("wanted-final-name.md"));
        assert_eq!(
            key(&mut picker, &mut app, KeyCode::Enter),
            Outcome::Copy("result-00.md".to_string())
        );
        let detail_scroll = picker.detail_scroll;
        picker.handle_key(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT));
        assert_eq!(picker.detail_scroll, detail_scroll - 1);
        terminal.backend_mut().resize(260, 20);
        terminal.autoresize().unwrap();
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        assert_eq!(picker.detail_scroll, 0);
        assert_focused_file_visible(&picker);
        key(&mut picker, &mut app, KeyCode::Down);
        assert_eq!(picker.focused_target, Some(1));
        assert_eq!(picker.detail_scroll, 0);
    }

    #[test]
    fn undisplayable_file_content_blocks_enter_and_space() {
        let (_directory, mut app, mut picker) = fixture(1);
        let mut terminal = Terminal::new(TestBackend::new(160, 3)).unwrap();
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        assert_eq!(picker.page_height, 0);
        assert_eq!(
            key(&mut picker, &mut app, KeyCode::Enter),
            Outcome::Continue
        );
        assert!(app
            .message
            .as_deref()
            .unwrap()
            .contains("Enlarge the window"));
        key(&mut picker, &mut app, KeyCode::Tab);
        key(&mut picker, &mut app, KeyCode::Char(' '));
        assert!(app.selected_hints.is_empty());
        app.selected_hints.push(app.targets[0].hint.clone());
        assert_eq!(
            key(&mut picker, &mut app, KeyCode::Enter),
            Outcome::Continue
        );
        key(&mut picker, &mut app, KeyCode::Left);
        assert_eq!(
            key(&mut picker, &mut app, KeyCode::Enter),
            Outcome::Continue
        );
        key(&mut picker, &mut app, KeyCode::Right);
        terminal.backend_mut().resize(2, 10);
        terminal.autoresize().unwrap();
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        assert_eq!(picker.file_width, 0);
        assert_eq!(
            key(&mut picker, &mut app, KeyCode::Enter),
            Outcome::Continue
        );
    }

    #[test]
    fn refresh_button_hitbox_matches_only_its_visible_left_clicks() {
        let (_directory, mut app, mut picker) = fixture(1);
        let mut terminal = Terminal::new(TestBackend::new(160, 12)).unwrap();
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        let button = picker.refresh_area;
        assert_eq!(area_rows(&terminal, button), ["[Refresh ^R]"]);
        assert!(screen_rows(&terminal).join("\n").contains("Ctrl+R:refresh"));
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: button.x,
            row: button.y,
            modifiers: KeyModifiers::NONE,
        };
        assert!(picker.refresh_clicked(click));
        assert!(picker.refresh_clicked(MouseEvent {
            column: button.right() - 1,
            ..click
        }));
        assert!(!picker.refresh_clicked(MouseEvent {
            column: button.x - 1,
            ..click
        }));
        assert!(!picker.refresh_clicked(MouseEvent {
            column: button.right(),
            ..click
        }));
        assert!(!picker.refresh_clicked(MouseEvent {
            row: button.bottom(),
            ..click
        }));
        assert!(!picker.refresh_clicked(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            ..click
        }));
        assert!(!picker.refresh_clicked(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..click
        }));
        key(&mut picker, &mut app, KeyCode::Left);
        picker.handle_mouse(click);
        assert_eq!(picker.active_panel, Panel::Context);
        terminal.backend_mut().resize(64, 12);
        terminal.autoresize().unwrap();
        terminal.draw(|frame| picker.draw(frame, &app)).unwrap();
        assert_eq!(picker.refresh_area, Rect::default());
        assert!(!picker.refresh_clicked(click));
        assert!(!screen_rows(&terminal).join("\n").contains("[Refresh ^R]"));
    }
}
