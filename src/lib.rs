use std::path::{Path, PathBuf};

use anyhow::Result;
use herdr_tiny_fingers::app::App;
use herdr_tiny_fingers::hints::assign_hints;
use herdr_tiny_fingers::patterns::{Matcher, PatternSpec};

pub fn file_matcher() -> Result<Matcher> {
    let enabled_builtin_patterns = vec!["path".to_string()];
    let mut filename = PatternSpec::new(
        "filename",
        r"(?P<match>[.\w@-]+\.[\w@-]+(?::[0-9]+(?::[0-9]+)?)?)",
    );
    filename.ignore_line_breaks = false;
    Matcher::with_builtin_patterns(Some(&enabled_builtin_patterns), vec![filename])
}

pub fn resolve_existing_file(raw: &str, cwd: &Path) -> Option<PathBuf> {
    let candidates = std::iter::once(raw).chain(
        raw.rsplit_once(':')
            .and_then(|(without_last, last)| last.parse::<u64>().ok().map(|_| without_last))
            .into_iter()
            .chain(raw.rsplit_once(':').and_then(|(without_column, column)| {
                column.parse::<u64>().ok().and_then(|_| {
                    without_column
                        .rsplit_once(':')
                        .and_then(|(without_line, line)| {
                            line.parse::<u64>().ok().map(|_| without_line)
                        })
                })
            })),
    );

    for candidate in candidates {
        let expanded = expand_home(candidate);
        let path = if expanded.is_absolute() {
            expanded
        } else {
            cwd.join(expanded)
        };
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

pub fn filter_existing_file_targets(app: &mut App, cwd: &Path) {
    let matches = std::mem::take(&mut app.targets)
        .into_iter()
        .filter(|target| resolve_existing_file(&target.target.text, cwd).is_some())
        .map(|target| target.target)
        .collect();
    app.targets = assign_hints(matches);
}

fn expand_home(value: &str) -> PathBuf {
    if value == "~" {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(value));
    }
    if let Some(relative) = value.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(relative);
        }
    }
    PathBuf::from(value)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use herdr_tiny_fingers::app::App;
    use herdr_tiny_fingers::theme::Theme;

    use super::{file_matcher, filter_existing_file_targets, resolve_existing_file};

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("herdr-download-picker-{unique}"));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn keeps_only_regular_files_and_reassigns_hints() {
        let root = TestDirectory::new();
        fs::create_dir(root.path.join("docs")).unwrap();
        fs::write(root.path.join("docs/result.md"), "result").unwrap();
        fs::create_dir(root.path.join("docs/folder")).unwrap();

        let matcher = file_matcher().unwrap();
        let mut app = App::from_text_with_theme(
            "docs/result.md docs/missing.md docs/folder",
            &matcher,
            Theme::default(),
        );
        filter_existing_file_targets(&mut app, &root.path);

        assert_eq!(app.targets.len(), 1);
        assert_eq!(app.targets[0].target.text, "docs/result.md");
        assert_eq!(app.targets[0].hint, "a");
    }

    #[test]
    fn recognizes_a_bare_filename_with_a_line_suffix() {
        let root = TestDirectory::new();
        let readme = root.path.join("README.md");
        fs::write(&readme, "readme").unwrap();

        assert_eq!(
            resolve_existing_file("README.md:42:7", &root.path),
            Some(readme)
        );
    }

    #[test]
    fn does_not_join_bare_filenames_across_visible_lines() {
        let root = TestDirectory::new();
        let long_name = "Approximate_Inverse_Model_Explanations_AIME_Unveiling_Local_and_Global_Insights_in_Machine_Learning_Models.pdf";
        fs::write(root.path.join(long_name), "long").unwrap();
        fs::write(root.path.join("Jasse.pdf"), "short").unwrap();

        let matcher = file_matcher().unwrap();
        let mut app = App::from_text_with_theme_and_pane_width(
            &format!("{long_name}\nJasse.pdf"),
            &matcher,
            Theme::default(),
            84,
        );
        filter_existing_file_targets(&mut app, &root.path);

        let targets = app
            .targets
            .iter()
            .map(|target| target.target.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(targets, [long_name, "Jasse.pdf"]);
    }
}
