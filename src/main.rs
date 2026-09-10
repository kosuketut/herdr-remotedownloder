use std::fs::{self, File};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use herdr_remote_download::{file_matcher, filter_existing_file_targets};
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

    let _restore = TerminalRestore;
    let mut terminal = ratatui::init();
    let outcome = loop {
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
    };

    if let Outcome::Copy(selected_path) = outcome {
        terminal.draw(|frame| draw_transfer(frame, &app, &selected_path, None))?;
        if let Err(error) = send_selected_file(&selected_path, &pane_cwd, || {
            if event::poll(Duration::from_millis(50))? {
                match event::read()? {
                    Event::Key(key) => return Ok(is_cancel_key(key)),
                    Event::Resize(_, _) => {
                        terminal.draw(|frame| draw_transfer(frame, &app, &selected_path, None))?;
                    }
                    _ => {}
                }
            }
            Ok(false)
        }) {
            let detail = format!("{error:#}");
            loop {
                terminal.draw(|frame| draw_transfer(frame, &app, &selected_path, Some(&detail)))?;
                match event::read()? {
                    Event::Key(key) => match key.code {
                        KeyCode::Esc | KeyCode::Enter => break,
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            break;
                        }
                        _ => {}
                    },
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            }
            bail!("{detail}");
        }
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

fn send_selected_file(
    selected_path: &str,
    pane_cwd: &Path,
    cancel_requested: impl FnMut() -> Result<bool>,
) -> Result<()> {
    let plugin_root = std::env::var_os("HERDR_PLUGIN_ROOT")
        .map(PathBuf::from)
        .context("HERDR_PLUGIN_ROOT is not set")?;
    let sender = plugin_root
        .join("target")
        .join("release")
        .join("herdr-remote-download");
    let context = json!({
        "selected_text": selected_path,
        "focused_pane_cwd": pane_cwd,
    });
    // Keep archives and diagnostics together so cancellation also removes them.
    let temporary = SenderTemporaryDirectory::create()?;
    let stderr_path = temporary.0.join("stderr");
    let mut child = Command::new(sender)
        .arg("send-context")
        .env("HERDR_PLUGIN_CONTEXT_JSON", context.to_string())
        .env("TMPDIR", &temporary.0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(File::create(&stderr_path)?)
        .spawn()
        .context("could not start the remote download sender")?;
    if let Some(status) = wait_for_sender(&mut child, cancel_requested)? {
        if !status.success() {
            let detail = fs::read_to_string(stderr_path)?;
            let detail = detail.trim();
            if detail.is_empty() {
                bail!("remote download sender exited with {status}");
            }
            bail!("{detail}");
        }
    }
    Ok(())
}

fn is_cancel_key(key: KeyEvent) -> bool {
    key.kind != event::KeyEventKind::Release && matches!(key_to_char(key), Some('\u{1b}' | '\u{3}'))
}

fn wait_for_sender(
    child: &mut Child,
    mut cancel_requested: impl FnMut() -> Result<bool>,
) -> Result<Option<ExitStatus>> {
    let result = (|| loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if cancel_requested()? {
            return Ok(None);
        }
    })();
    if !matches!(&result, Ok(Some(_))) {
        // Reap the sender before removing its temporary files, even on UI errors.
        let killed = child.kill();
        let waited = child.wait();
        killed.context("could not stop the remote download sender")?;
        waited.context("could not reap the remote download sender")?;
    }
    result
}

struct SenderTemporaryDirectory(PathBuf);

impl SenderTemporaryDirectory {
    fn create() -> Result<Self> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path =
            std::env::temp_dir().join(format!("herdr-sender-{}-{nonce}", std::process::id()));
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
}

impl Drop for SenderTemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
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

fn draw_transfer(frame: &mut Frame<'_>, app: &App, selected_path: &str, error: Option<&str>) {
    let area = frame.area();
    let mut lines = if let Some(detail) = error {
        vec![
            Line::from(Span::styled("Transfer failed.", app.theme.empty_style())),
            Line::from(""),
            Line::from(detail.to_string()),
            Line::from(""),
            Line::from("Press Esc or Enter to close."),
        ]
    } else {
        vec![
            Line::from("Transferring to the connected Mac..."),
            Line::from(""),
            Line::from(selected_path.to_string()),
            Line::from(""),
            Line::from("Esc / Ctrl+C: cancel. This window closes when the transfer finishes."),
        ]
    };
    let line_count = usize::from(area.height.saturating_sub(1));
    lines.truncate(line_count);
    frame.render_widget(Paragraph::new(lines), area);

    if area.height == 0 {
        return;
    }
    let status_area = Rect {
        x: area.x,
        y: area.y + area.height - 1,
        width: area.width,
        height: 1,
    };
    let message = if error.is_some() {
        " download  failed  esc:close "
    } else {
        " download  transferring...  esc:cancel "
    };
    frame.render_widget(
        Paragraph::new(message).style(app.theme.status_style()),
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
    fn sender_cancellation_stops_and_reaps_the_process() {
        assert!(is_cancel_key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE
        )));
        assert!(is_cancel_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_cancel_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::NONE
        )));
        assert!(!is_cancel_key(KeyEvent::new_with_kind(
            KeyCode::Esc,
            KeyModifiers::NONE,
            event::KeyEventKind::Release,
        )));

        for ui_error in [false, true] {
            let temporary = SenderTemporaryDirectory::create().unwrap();
            let path = temporary.0.clone();
            fs::write(path.join("partial.tar.gz"), b"partial archive").unwrap();
            let mut child = Command::new("sleep").arg("30").spawn().unwrap();
            let result = wait_for_sender(&mut child, || {
                if ui_error {
                    bail!("input failed");
                }
                Ok(true)
            });
            if ui_error {
                assert!(result.unwrap_err().to_string().contains("input failed"));
            } else {
                assert!(result.unwrap().is_none());
            }
            assert!(!child.try_wait().unwrap().unwrap().success());
            drop(temporary);
            assert!(!path.exists());
        }

        let mut child = Command::new("sh").args(["-c", "exit 7"]).spawn().unwrap();
        let status = wait_for_sender(&mut child, || {
            std::thread::sleep(Duration::from_millis(10));
            Ok(false)
        })
        .unwrap()
        .unwrap();
        assert_eq!(status.code(), Some(7));
    }

    #[test]
    fn tab_is_not_used_for_multi_select() {
        assert_eq!(
            key_to_char(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            None
        );
    }
}
