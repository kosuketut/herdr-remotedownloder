//! Read-only source snapshot with the same target hints as the download list.

use herdr_tiny_fingers::app::App;
use herdr_tiny_fingers::patterns::Position;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

/// Insert hints beside accepted paths, preserving every original source
/// character. Match positions count Unicode characters in the source snapshot;
/// only the final wrapping step converts them to terminal cell widths.
/// Captured terminal row breaks are preserved: without soft-wrap metadata,
/// joining them could change code or prose. A narrower panel may wrap them again.
pub(crate) fn render_rows(app: &App, width: u16) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let mut rows = Vec::new();
    for (row, text) in app.lines.iter().enumerate() {
        let chars = text.chars().collect::<Vec<_>>();
        let mut segments = app
            .targets
            .iter()
            .filter(|target| target.hint.starts_with(&app.input))
            .filter_map(|target| {
                row_segment(row, target.target.start, target.target.end, chars.len())
                    .map(|(start, end)| (start, end, target))
            })
            .collect::<Vec<_>>();
        segments.sort_by_key(|(start, end, _)| (*start, *end));

        let mut spans = Vec::new();
        let mut cursor = 0;
        for (start, end, target) in segments {
            // The matcher already removes overlaps. Keep the source intact if
            // a future caller nevertheless supplies overlapping target ranges.
            if start < cursor {
                continue;
            }
            if cursor < start {
                spans.push(Span::raw(chars[cursor..start].iter().collect::<String>()));
            }
            let selected = app.is_selected_hint(&target.hint);
            if row == target.target.start.row {
                spans.push(Span::styled(
                    format!("[{}]", target.hint),
                    app.theme.hint_style(selected),
                ));
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(
                chars[start..end].iter().collect::<String>(),
                app.theme.match_style(selected).add_modifier(Modifier::BOLD),
            ));
            cursor = end;
        }
        if cursor < chars.len() {
            spans.push(Span::raw(chars[cursor..].iter().collect::<String>()));
        }
        if spans.is_empty() {
            // wrap_line omits empty rows, but blank source lines provide useful
            // conversational structure and must survive in the snapshot.
            rows.push(Line::default());
        } else {
            rows.extend(crate::picker::wrap_line(Line::from(spans), width));
        }
    }
    rows
}

fn row_segment(
    row: usize,
    start: Position,
    end: Position,
    length: usize,
) -> Option<(usize, usize)> {
    if row < start.row || row > end.row {
        return None;
    }
    let first = if row == start.row { start.col } else { 0 }.min(length);
    let last = if row == end.row { end.col } else { length }.min(length);
    (first < last).then_some((first, last))
}

#[cfg(test)]
mod tests {
    use herdr_tiny_fingers::patterns::{Matcher, PatternSpec};
    use herdr_tiny_fingers::theme::Theme;
    use ratatui::backend::TestBackend;
    use ratatui::style::{Color, Style};
    use ratatui::widgets::Paragraph;
    use ratatui::Terminal;

    use super::*;

    fn fixture(text: &str) -> App {
        let matcher = Matcher::with_builtin_patterns(Some(&["path".to_string()]), vec![]).unwrap();
        // These are presentation fixtures. Production supplies only matches
        // already admitted by filter_existing_file_targets.
        App::from_text_with_theme_and_pane_width(text, &matcher, Theme::default(), 200)
    }

    fn text_of(rows: &[Line<'_>]) -> String {
        rows.iter()
            .flat_map(|row| row.spans.iter().map(|span| span.content.as_ref()))
            .collect()
    }

    fn styled_text(rows: &[Line<'_>], style: Style) -> String {
        rows.iter()
            .flat_map(|row| row.spans.iter())
            .filter(|span| span.style == style)
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn separate_hints_preserve_both_paths_and_line_suffix_in_source_order() {
        let app = fixture("compare /tmp/first.md:93 with /tmp/second.md:42:7.");
        assert_eq!(app.targets.len(), 2);
        let rows = render_rows(&app, 100);
        assert_eq!(
            text_of(&rows),
            format!(
                "compare [{}] /tmp/first.md:93 with [{}] /tmp/second.md:42:7.",
                app.targets[0].hint, app.targets[1].hint
            )
        );
        assert_eq!(
            styled_text(&rows, app.theme.hint_style(false)),
            format!("[{}][{}]", app.targets[0].hint, app.targets[1].hint)
        );
        assert_eq!(
            styled_text(
                &rows,
                app.theme.match_style(false).add_modifier(Modifier::BOLD)
            ),
            "/tmp/first.md/tmp/second.md"
        );
    }

    #[test]
    fn cjk_prefix_uses_character_offsets_and_highlights_the_full_first_slash() {
        let app = fixture("说明： /tmp/报告.md 后文");
        assert_eq!(app.targets.len(), 1);
        assert_eq!(app.targets[0].target.start.col, 4);
        let mut terminal = Terminal::new(TestBackend::new(60, 3)).unwrap();
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new(render_rows(&app, 60)), frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        // Three CJK characters occupy six cells, followed by the source space.
        assert_eq!(buffer[(7, 0)].symbol(), "[");
        assert_eq!(buffer[(8, 0)].symbol(), app.targets[0].hint);
        assert_eq!(buffer[(7, 0)].bg, app.theme.hint_bg);
        assert_eq!(buffer[(11, 0)].symbol(), "/");
        assert_eq!(buffer[(11, 0)].fg, app.theme.match_fg);
        assert!(buffer[(11, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(0, 0)].fg, Color::Reset);
        assert_eq!(
            text_of(&render_rows(&app, 60)),
            format!("说明： [{}] /tmp/报告.md 后文", app.targets[0].hint)
        );
    }

    #[test]
    fn wrapped_source_path_has_one_hint_and_every_continuation_character() {
        let matcher = Matcher::with_builtin_patterns(Some(&["path".to_string()]), vec![]).unwrap();
        let first = "查看 /tmp/long/source/";
        let app = App::from_text_with_theme_and_pane_width(
            &format!("{first}\n  result.md tail"),
            &matcher,
            Theme::default(),
            Line::from(first).width(),
        );
        assert_eq!(app.targets.len(), 1);
        assert_eq!(app.targets[0].target.start.row, 0);
        assert_eq!(app.targets[0].target.end.row, 1);
        let rows = render_rows(&app, 15);
        let hint = format!("[{}]", app.targets[0].hint);
        assert_eq!(
            text_of(&rows),
            format!("查看 {hint} /tmp/long/source/  result.md tail")
        );
        assert_eq!(styled_text(&rows, app.theme.hint_style(false)), hint);
        assert_eq!(
            styled_text(
                &rows,
                app.theme.match_style(false).add_modifier(Modifier::BOLD)
            ),
            "/tmp/long/source/  result.md"
        );
        assert!(rows.iter().all(|row| row.width() <= 15));
    }

    #[test]
    fn accepted_unicode_and_space_paths_preserve_all_characters_when_rewrapped() {
        let mut pattern = PatternSpec::new("space-path", r"/tmp/研究  文档/résumé long.md");
        pattern.ignore_line_breaks = false;
        let app = App::from_text_with_theme(
            "read /tmp/研究  文档/résumé long.md now",
            &Matcher::new([pattern]).unwrap(),
            Theme::default(),
        );
        assert_eq!(app.targets.len(), 1);
        let rows = render_rows(&app, 12);
        assert_eq!(
            text_of(&rows),
            format!(
                "read [{}] /tmp/研究  文档/résumé long.md now",
                app.targets[0].hint
            )
        );
        assert!(rows.iter().all(|row| row.width() <= 12));
    }

    #[test]
    fn prefix_and_selection_use_the_same_hints_and_theme_as_the_list() {
        let mut app = fixture("/tmp/one.md /tmp/two.md /tmp/three.md");
        // Distinguish hint and path styles: the default selected colors are
        // identical after the source path receives its emphasis modifier.
        app.theme.selected_hint_fg = Color::Cyan;
        for (target, hint) in app.targets.iter_mut().zip(["aa", "as", "sa"]) {
            target.hint = hint.to_string();
        }
        app.input = "a".to_string();
        app.selected_hints = vec!["as".to_string()];
        let rows = render_rows(&app, 100);
        assert_eq!(
            text_of(&rows),
            "[aa] /tmp/one.md [as] /tmp/two.md /tmp/three.md"
        );
        assert_eq!(styled_text(&rows, app.theme.hint_style(false)), "[aa]");
        assert_eq!(styled_text(&rows, app.theme.hint_style(true)), "[as]");
        assert_eq!(
            styled_text(
                &rows,
                app.theme.match_style(true).add_modifier(Modifier::BOLD)
            ),
            "/tmp/two.md"
        );
        assert!(styled_text(&rows, Style::default()).contains("/tmp/three.md"));
    }

    #[test]
    fn filtered_target_text_stays_readable_and_unhighlighted() {
        let mut app = fixture("/tmp/existing.md then /tmp/missing.md");
        assert_eq!(app.targets.len(), 2);
        // Model the caller dropping a nonexistent target before presentation.
        app.targets.truncate(1);
        let rows = render_rows(&app, 100);
        assert_eq!(
            text_of(&rows),
            format!(
                "[{}] /tmp/existing.md then /tmp/missing.md",
                app.targets[0].hint
            )
        );
        assert!(styled_text(&rows, Style::default()).contains("/tmp/missing.md"));
        assert_eq!(
            styled_text(
                &rows,
                app.theme.match_style(false).add_modifier(Modifier::BOLD)
            ),
            "/tmp/existing.md"
        );
    }

    #[test]
    fn empty_target_set_preserves_source_blank_rows_and_zero_width_is_empty() {
        let mut app = fixture("first\n\nlast");
        app.targets.clear();
        let rows = render_rows(&app, 80);
        assert_eq!(rows.len(), 3);
        assert_eq!(text_of(&rows[..1]), "first");
        assert_eq!(text_of(&rows[1..2]), "");
        assert_eq!(text_of(&rows[2..]), "last");
        assert!(render_rows(&app, 0).is_empty());
    }
}
