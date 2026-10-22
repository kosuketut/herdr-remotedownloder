use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, ExitCode, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use herdr_remote_download::transfer::{
    default_remote_socket_path, notify_herdr, read_token, request_ascii_input, sender_token_path,
    ReceiverEndpoint, TransferProgress,
};
use herdr_remote_download::{file_matcher, filter_existing_file_targets};
use herdr_tiny_fingers::app::{App, Outcome};
use herdr_tiny_fingers::herdr_client::SocketClient;
use herdr_tiny_fingers::theme::Theme;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;
use serde_json::{json, Value};

mod context_view;
mod picker;
mod source_cwd;

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
    switch_mac_to_ascii_input();
    let socket_path = std::env::var_os("HERDR_SOCKET_PATH")
        .context("HERDR_SOCKET_PATH is not set; open this through the Herdr plugin action")?;
    let (pane_id, _launch_cwd) = focused_pane_context()?;
    let mut client = SocketClient::connect(Path::new(&socket_path))?;
    let (mut app, mut picker, mut pane_cwd) =
        read_picker(&mut client, &pane_id, Path::new(&socket_path))?;

    let _restore = TerminalRestore;
    let mut terminal = ratatui::init();
    crossterm::execute!(std::io::stdout(), event::EnableMouseCapture)?;
    let outcome = loop {
        terminal.draw(|frame| picker.draw(frame, &app))?;
        match event::read()? {
            Event::Key(key) if is_refresh_key(key) => {
                let refreshed = read_picker(&mut client, &pane_id, Path::new(&socket_path));
                apply_refresh(&mut app, &mut picker, &mut pane_cwd, refreshed);
            }
            Event::Key(key) => match picker.handle_key(&mut app, key) {
                Outcome::Continue => {}
                other => break other,
            },
            Event::Resize(_, _) => {}
            Event::Mouse(mouse) if picker.refresh_clicked(mouse) => {
                let refreshed = read_picker(&mut client, &pane_id, Path::new(&socket_path));
                apply_refresh(&mut app, &mut picker, &mut pane_cwd, refreshed);
            }
            Event::Mouse(mouse) => {
                if picker.handle_mouse(mouse) {
                    app.message = None;
                }
            }
            _ => {}
        }
    };

    if let Outcome::Copy(selection) = outcome {
        let paths = selected_paths(&selection);
        let mut view = TransferView {
            paths: &paths,
            index: 0,
            progress: None,
        };
        let mut saved = Vec::new();
        let mut failure = None;
        for (index, path) in paths.iter().enumerate() {
            view.index = index;
            view.progress = None;
            terminal.draw(|frame| draw_transfer(frame, &app, &view, None))?;
            let result = send_selected_file(path, &pane_cwd, |progress| {
                if progress.is_some() {
                    view.progress = progress;
                    terminal.draw(|frame| draw_transfer(frame, &app, &view, None))?;
                }
                if event::poll(Duration::from_millis(50))? {
                    match event::read()? {
                        Event::Key(key) => return Ok(is_cancel_key(key)),
                        Event::Resize(_, _) => {
                            terminal.draw(|frame| draw_transfer(frame, &app, &view, None))?;
                        }
                        _ => {}
                    }
                }
                Ok(false)
            });
            match result {
                Ok(Some(saved_path)) => saved.push(saved_path),
                Ok(None) => break,
                Err(error) => {
                    failure = Some(format!("{error:#}"));
                    break;
                }
            }
        }
        notify_saved(&saved);
        if let Some(detail) = failure {
            loop {
                terminal.draw(|frame| draw_transfer(frame, &app, &view, Some(&detail)))?;
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

fn read_picker(
    client: &mut SocketClient,
    pane_id: &str,
    socket_path: &Path,
) -> Result<(App, picker::Picker, PathBuf)> {
    let pane_cwd = source_cwd::read(socket_path, pane_id)?;
    let text = client.read_visible_pane(pane_id)?;
    let width = client
        .visible_pane_width(pane_id)
        .ok()
        .map(visible_wrap_width);
    if source_cwd::read(socket_path, pane_id)? != pane_cwd {
        bail!("source directory changed while reading; reopen the picker");
    }
    let (app, picker) = build_picker(&text, &pane_cwd, width)?;
    Ok((app, picker, pane_cwd))
}

fn build_picker(
    text: &str,
    pane_cwd: &Path,
    width: Option<usize>,
) -> Result<(App, picker::Picker)> {
    let matcher = file_matcher()?;
    let mut app = match width {
        Some(width) => {
            App::from_text_with_theme_and_pane_width(text, &matcher, Theme::default(), width)
        }
        None => App::from_text_with_theme(text, &matcher, Theme::default()),
    };
    filter_existing_file_targets(&mut app, pane_cwd);
    let picker = picker::Picker::new(&app, pane_cwd)?;
    Ok((app, picker))
}

fn is_refresh_key(key: KeyEvent) -> bool {
    key.kind != event::KeyEventKind::Release
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('r' | 'R'))
}

fn apply_refresh(
    app: &mut App,
    picker: &mut picker::Picker,
    pane_cwd: &mut PathBuf,
    refreshed: Result<(App, picker::Picker, PathBuf)>,
) {
    match refreshed {
        Ok((mut updated, replacement, directory)) => {
            updated.message = Some(format!(
                "Refreshed: {} files; hints rebuilt, selections cleared.",
                updated.targets.len()
            ));
            *app = updated;
            *picker = replacement;
            *pane_cwd = directory;
        }
        Err(error) => {
            // A failed read must not discard the old snapshot or selection.
            app.message = Some(format!("Refresh failed; kept previous snapshot: {error:#}"));
        }
    }
}

/// Hints are typed as letters, which a Japanese IME on the Mac would capture.
/// Best effort in the background: an older receiver or a slow tunnel must not
/// delay the picker.
fn switch_mac_to_ascii_input() {
    thread::spawn(|| -> Result<()> {
        let socket = match std::env::var_os("HERDR_DOWNLOAD_SOCKET") {
            Some(path) => PathBuf::from(path),
            None => default_remote_socket_path()?,
        };
        let token = read_token(&sender_token_path()?)?;
        request_ascii_input(
            &ReceiverEndpoint::Unix(socket),
            &token,
            Duration::from_secs(5),
        )
    });
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

/// Split a picker selection into unique paths, keeping the selection order.
fn selected_paths(selection: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for path in selection
        .lines()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    {
        if !paths.iter().any(|seen| seen == path) {
            paths.push(path.to_string());
        }
    }
    paths
}

fn notify_saved(saved: &[String]) {
    let body = match saved {
        [] => return,
        [path] if !path.is_empty() => format!("Saved {path}"),
        _ => format!("Saved {} files", saved.len()),
    };
    notify_herdr("Herdr download complete", &body, "success");
}

/// Send one file and return its saved path on the Mac, or None when cancelled.
/// `on_poll` receives the latest progress and returns true to cancel.
fn send_selected_file(
    selected_path: &str,
    pane_cwd: &Path,
    mut on_poll: impl FnMut(Option<TransferProgress>) -> Result<bool>,
) -> Result<Option<String>> {
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
        .args(["send-context", "--progress", "--no-notify"])
        .env("HERDR_PLUGIN_CONTEXT_JSON", context.to_string())
        .env("TMPDIR", &temporary.0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(File::create(&stderr_path)?)
        .spawn()
        .context("could not start the remote download sender")?;
    let output = read_lines(child.stdout.take().context("sender output is not piped")?);
    let mut saved = None;
    let mut handle_line = |line: String| match TransferProgress::from_line(&line) {
        Some(progress) => Some(progress),
        None => {
            saved = saved_path(&line).or(saved.take());
            None
        }
    };
    let status = wait_for_sender(&mut child, || {
        let progress = output.try_iter().filter_map(&mut handle_line).last();
        on_poll(progress)
    })?;
    let Some(status) = status else {
        return Ok(None);
    };
    // The sender has exited, so this drains the rest and ends at EOF.
    output.iter().for_each(|line| {
        handle_line(line);
    });
    if !status.success() {
        let detail = fs::read_to_string(stderr_path)?;
        let detail = detail.trim();
        if detail.is_empty() {
            bail!("remote download sender exited with {status}");
        }
        bail!("{detail}");
    }
    Ok(Some(saved.unwrap_or_default()))
}

fn read_lines(stdout: ChildStdout) -> mpsc::Receiver<String> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    receiver
}

fn saved_path(line: &str) -> Option<String> {
    let response: Value = serde_json::from_str(line).ok()?;
    Some(response.get("path")?.as_str()?.to_string())
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

struct TransferView<'a> {
    paths: &'a [String],
    index: usize,
    progress: Option<TransferProgress>,
}

fn draw_transfer(frame: &mut Frame<'_>, app: &App, view: &TransferView<'_>, error: Option<&str>) {
    let area = frame.area();
    let count = view.paths.len();
    let lines = if let Some(detail) = error {
        let mut lines = vec![
            Line::from(Span::styled("Transfer failed.", app.theme.empty_style())),
            Line::from(""),
            Line::from(detail.to_string()),
        ];
        if count > 1 {
            lines.push(Line::from(""));
            lines.push(Line::from(format!(
                "Saved {} of {count} files before this failure; the rest were not sent.",
                view.index
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from("Press Esc or Enter to close."));
        lines
    } else {
        let heading = if count > 1 {
            format!(
                "Transferring file {} of {count} to the connected Mac...",
                view.index + 1
            )
        } else {
            "Transferring to the connected Mac...".to_string()
        };
        vec![
            Line::from(heading),
            Line::from(""),
            Line::from(view.paths[view.index].clone()),
            Line::from(progress_text(view.progress, usize::from(area.width))),
            Line::from(""),
            Line::from("Esc / Ctrl+C: cancel. This window closes when the transfer finishes."),
        ]
    };
    // Wrap long paths and errors so the whole message stays readable.
    let text_area = Rect {
        height: area.height.saturating_sub(1),
        ..area
    };
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), text_area);

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

fn progress_text(progress: Option<TransferProgress>, width: usize) -> String {
    let (sent, total) = match progress {
        None => return "Connecting...".to_string(),
        Some(TransferProgress::Archiving) => return "Archiving the directory...".to_string(),
        Some(TransferProgress::Hashing) => return "Computing the checksum...".to_string(),
        Some(TransferProgress::Sending { sent, total }) => (sent, total),
    };
    let ratio = if total == 0 {
        1.0
    } else {
        sent as f64 / total as f64
    };
    let mib = 1024.0 * 1024.0;
    let summary = format!(
        "{:3}% ({:.1}/{:.1} MiB)",
        (ratio * 100.0) as u64,
        sent as f64 / mib,
        total as f64 / mib
    );
    // Leave room for the brackets and summary; drop the bar on narrow panes.
    let bar_width = width.saturating_sub(summary.len() + 3).min(30);
    if bar_width < 5 {
        return summary;
    }
    let filled = ((ratio * bar_width as f64).round() as usize).min(bar_width);
    format!(
        "[{}{}] {summary}",
        "#".repeat(filled),
        "-".repeat(bar_width - filled)
    )
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
        KeyCode::Tab => Some('\t'),
        KeyCode::Backspace => Some('\u{7f}'),
        KeyCode::Char(character) => Some(character),
        _ => None,
    }
}

struct TerminalRestore;

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), event::DisableMouseCapture);
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
    fn refresh_is_explicit_and_clears_stale_selection_and_hint_input() {
        let temporary = SenderTemporaryDirectory::create().unwrap();
        for name in ["one.md", "two.md"] {
            fs::write(temporary.0.join(name), b"fixture").unwrap();
        }
        let (mut app, mut picker) = build_picker("one.md", &temporary.0, None).unwrap();
        app.multi_mode = true;
        app.selected_hints = vec![app.targets[0].hint.clone()];
        app.input = "a".to_string();
        let mut cwd = temporary.0.clone();
        let refreshed = build_picker("two.md one.md", &temporary.0, Some(80))
            .map(|(app, picker)| (app, picker, temporary.0.clone()));
        apply_refresh(&mut app, &mut picker, &mut cwd, refreshed);
        assert_eq!(app.targets.len(), 2);
        assert_eq!(app.targets[0].target.text, "two.md");
        assert!(!app.multi_mode);
        assert!(app.selected_hints.is_empty());
        assert!(app.input.is_empty());
        assert!(app
            .message
            .as_deref()
            .unwrap()
            .contains("hints rebuilt, selections cleared"));
        assert!(is_refresh_key(KeyEvent::new(
            KeyCode::Char('r'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_refresh_key(KeyEvent::new(
            KeyCode::Char('r'),
            KeyModifiers::NONE
        )));
        assert!(!is_refresh_key(KeyEvent::new_with_kind(
            KeyCode::Char('r'),
            KeyModifiers::CONTROL,
            event::KeyEventKind::Release,
        )));
    }

    #[test]
    fn failed_refresh_preserves_snapshot_and_selection() {
        let temporary = SenderTemporaryDirectory::create().unwrap();
        fs::write(temporary.0.join("one.md"), b"fixture").unwrap();
        let (mut app, mut picker) = build_picker("one.md", &temporary.0, None).unwrap();
        app.multi_mode = true;
        app.selected_hints = vec![app.targets[0].hint.clone()];
        let selected = app.selected_hints.clone();
        let mut cwd = temporary.0.clone();
        apply_refresh(
            &mut app,
            &mut picker,
            &mut cwd,
            Err(anyhow::anyhow!("source pane closed")),
        );
        assert_eq!(cwd, temporary.0);
        assert_eq!(app.lines, ["one.md"]);
        assert_eq!(app.targets.len(), 1);
        assert_eq!(app.selected_hints, selected);
        assert!(app.multi_mode);
        assert!(app
            .message
            .as_deref()
            .unwrap()
            .contains("kept previous snapshot"));
    }

    #[test]
    fn refresh_rebinds_same_named_relative_file_to_the_new_source_directory() {
        let temporary = SenderTemporaryDirectory::create().unwrap();
        let first = temporary.0.join("A");
        let second = temporary.0.join("B");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        fs::write(first.join("result.md"), b"old directory").unwrap();
        fs::write(second.join("result.md"), b"new directory").unwrap();
        let (mut app, mut picker) = build_picker("result.md", &first, None).unwrap();
        let mut cwd = first;
        let (updated, replacement) = build_picker("result.md", &second, None).unwrap();
        apply_refresh(
            &mut app,
            &mut picker,
            &mut cwd,
            Ok((updated, replacement, second.clone())),
        );
        let actual_send_path = herdr_remote_download::transfer::resolve_path_from_context(&json!({
            "selected_text": app.targets[0].target.text,
            "focused_pane_cwd": cwd,
        }))
        .unwrap();
        assert_eq!(
            actual_send_path,
            second.join("result.md").canonicalize().unwrap()
        );
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
    fn transfer_error_wraps_to_the_pane_width() {
        let app = App::from_text_with_theme("", &file_matcher().unwrap(), Theme::default());
        let backend = ratatui::backend::TestBackend::new(24, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let detail = "local receiver is unavailable through /tmp/test.sock";
        let paths = ["/data/file.pptx".to_string()];
        let view = TransferView {
            paths: &paths,
            index: 0,
            progress: None,
        };
        terminal
            .draw(|frame| draw_transfer(frame, &app, &view, Some(detail)))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("/tmp/test.sock"));
        assert!(screen.contains("Press Esc or Enter"));
    }

    #[test]
    fn tab_selects_multiple_unique_paths() {
        let mut app = App::from_text_with_theme(
            "/tmp/a.txt /tmp/b.txt /tmp/a.txt",
            &file_matcher().unwrap(),
            Theme::default(),
        );
        let hints = app
            .targets
            .iter()
            .map(|target| target.hint.clone())
            .collect::<Vec<_>>();
        assert_eq!(hints.len(), 3);
        let mut press = |code| {
            let character = key_to_char(KeyEvent::new(code, KeyModifiers::NONE)).unwrap();
            app.handle_char(character)
        };
        assert_eq!(press(KeyCode::Tab), Outcome::Continue);
        for hint in &hints {
            for character in hint.chars() {
                assert_eq!(press(KeyCode::Char(character)), Outcome::Continue);
            }
        }
        let Outcome::Copy(selection) = press(KeyCode::Tab) else {
            panic!("multi selection was not sent");
        };
        assert_eq!(selected_paths(&selection), ["/tmp/a.txt", "/tmp/b.txt"]);
    }

    #[test]
    fn progress_shows_percent_bytes_and_bar() {
        let sending = Some(TransferProgress::Sending {
            sent: 3 * 1024 * 1024,
            total: 4 * 1024 * 1024,
        });
        assert_eq!(
            progress_text(sending, 80),
            format!("[{}{}]  75% (3.0/4.0 MiB)", "#".repeat(23), "-".repeat(7))
        );
        assert_eq!(progress_text(sending, 20), " 75% (3.0/4.0 MiB)");
        assert_eq!(progress_text(None, 80), "Connecting...");
        assert_eq!(
            progress_text(Some(TransferProgress::Sending { sent: 0, total: 0 }), 10),
            "100% (0.0/0.0 MiB)"
        );
    }

    #[test]
    fn sender_output_reports_progress_and_saved_path() {
        let script = "printf 'progress hashing\\nprogress sending 2 4\\n{\"path\":\"/Users/me/Downloads/a.txt\"}\\n'";
        let mut child = Command::new("sh")
            .args(["-c", script])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let output = read_lines(child.stdout.take().unwrap());
        child.wait().unwrap();
        let lines = output.iter().collect::<Vec<_>>();
        assert_eq!(
            lines
                .iter()
                .filter_map(|line| TransferProgress::from_line(line))
                .next_back(),
            Some(TransferProgress::Sending { sent: 2, total: 4 })
        );
        assert_eq!(
            saved_path(&lines[2]).as_deref(),
            Some("/Users/me/Downloads/a.txt")
        );
    }
}
