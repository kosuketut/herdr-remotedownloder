use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{bail, Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use herdr_remote_download_picker::{file_matcher, filter_existing_file_targets};
use herdr_tiny_fingers::app::{App, Outcome};
use herdr_tiny_fingers::herdr_client::SocketClient;
use herdr_tiny_fingers::theme::Theme;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde_json::{json, Value};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("herdr-remote-download-picker: {error:#}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<()> {
    let socket_path = std::env::var_os("HERDR_SOCKET_PATH")
        .context("HERDR_SOCKET_PATH is not set; open this through the Herdr plugin action")?;
    let (pane_id, pane_cwd) = focused_pane_context()?;
    let mut client = SocketClient::connect(Path::new(&socket_path))?;
    let text = client.read_visible_pane(&pane_id)?;
    let pane_width = client
        .visible_pane_width(&pane_id)
        .ok()
        .map(visible_wrap_width);
    let matcher = file_matcher()?;
    let mut app = match pane_width {
        Some(width) => {
            App::from_text_with_theme_and_pane_width(&text, &matcher, Theme::default(), width)
        }
        None => App::from_text_with_theme(&text, &matcher, Theme::default()),
    };
    filter_existing_file_targets(&mut app, &pane_cwd);

    let outcome = {
        let _restore = TerminalRestore;
        let mut terminal = ratatui::init();
        loop {
            terminal.draw(|frame| draw(frame, &app))?;
            match event::read()? {
                Event::Key(key) => {
                    if let Some(character) = key_to_char(key) {
                        match app.handle_char(character) {
                            Outcome::Continue => {}
                            other => break other,
                        }
                    }
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    };

    if let Outcome::Copy(selected_path) = outcome {
        send_selected_file(&selected_path, &pane_cwd)?;
    }
    Ok(())
}

fn focused_pane_context() -> Result<(String, PathBuf)> {
    let raw = std::env::var("HERDR_PLUGIN_CONTEXT_JSON")
        .context("HERDR_PLUGIN_CONTEXT_JSON is not set")?;
    let context: Value =
        serde_json::from_str(&raw).context("HERDR_PLUGIN_CONTEXT_JSON is invalid")?;
    let pane_id = context
        .get("focused_pane_id")
        .and_then(Value::as_str)
        .context("plugin context did not include focused_pane_id")?;
    let cwd = context
        .get("focused_pane_cwd")
        .or_else(|| context.get("workspace_cwd"))
        .and_then(Value::as_str)
        .context("plugin context did not include focused_pane_cwd")?;
    Ok((pane_id.to_string(), PathBuf::from(cwd)))
}

fn send_selected_file(selected_path: &str, pane_cwd: &Path) -> Result<()> {
    let plugin_root = std::env::var_os("HERDR_PLUGIN_ROOT")
        .map(PathBuf::from)
        .context("HERDR_PLUGIN_ROOT is not set")?;
    let script = plugin_root.join("herdr_remote_download.py");
    let context = json!({
        "selected_text": selected_path,
        "focused_pane_cwd": pane_cwd,
    });
    let output = Command::new("python3")
        .arg(script)
        .arg("send-context")
        .env("HERDR_PLUGIN_CONTEXT_JSON", context.to_string())
        .output()
        .context("could not start the remote download sender")?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if detail.is_empty() {
            bail!("remote download sender exited with {}", output.status);
        }
        bail!("{detail}");
    }
    Ok(())
}

fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    let line_count = usize::from(area.height.saturating_sub(1));
    let lines = if app.targets.is_empty() {
        vec![Line::from(Span::styled(
            "No existing file paths on this visible screen.",
            app.theme.empty_style(),
        ))]
    } else {
        herdr_tiny_fingers::ui::render_lines(app, line_count)
    };
    frame.render_widget(Paragraph::new(lines), area);
    draw_status(frame, app, area);
}

fn draw_status(frame: &mut Frame<'_>, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    let status_area = Rect {
        x: area.x,
        y: area.y + area.height - 1,
        width: area.width,
        height: 1,
    };
    let input = if app.input.is_empty() {
        "-"
    } else {
        &app.input
    };
    let full = format!(
        " download  files:{}  input:{}  esc:close ",
        app.visible_target_count(),
        input
    );
    let compact = format!(
        " download files:{} input:{} ",
        app.visible_target_count(),
        input
    );
    let width = usize::from(status_area.width);
    let status = if full.chars().count() <= width {
        full
    } else {
        compact.chars().take(width).collect()
    };
    frame.render_widget(
        Paragraph::new(status).style(app.theme.status_style()),
        status_area,
    );
}

fn visible_wrap_width(layout_width: usize) -> usize {
    if layout_width > 1 {
        layout_width - 1
    } else {
        layout_width
    }
}

fn key_to_char(key: KeyEvent) -> Option<char> {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') | KeyCode::Char('C') => Some('\u{3}'),
            _ => None,
        };
    }
    match key.code {
        KeyCode::Esc => Some('\u{1b}'),
        KeyCode::Backspace => Some('\u{7f}'),
        KeyCode::Char(character) => Some(character),
        _ => None,
    }
}

struct TerminalRestore;

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_width_excludes_the_terminal_right_edge() {
        assert_eq!(visible_wrap_width(118), 117);
        assert_eq!(visible_wrap_width(1), 1);
    }

    #[test]
    fn tab_is_not_used_for_multi_select() {
        assert_eq!(
            key_to_char(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            None
        );
    }
}
