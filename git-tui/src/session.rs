//! Session persistence: which projects were open.
//!
//! Stored at `$XDG_CONFIG_HOME/activegit/session.toml` (or
//! `~/.config/activegit/session.toml`; `%APPDATA%\activegit\session.toml` on
//! Windows). When the app starts with no explicit
//! paths, these projects are re-opened; explicit CLI paths override the
//! session and replace it.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Open projects to restore on next launch, plus which tab was selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub projects: Vec<PathBuf>,
    pub current: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionFile {
    #[serde(default)]
    projects: Vec<String>,
    #[serde(default)]
    current: usize,
}

impl Session {
    pub fn empty() -> Self {
        Self {
            projects: Vec::new(),
            current: 0,
        }
    }

    pub fn new(projects: Vec<PathBuf>, current: usize) -> Self {
        Self { projects, current }
    }

    /// Where the session file lives. `None` when no config directory can be
    /// found (see [`crate::config::config_dir`]); persistence is then disabled.
    pub fn default_path() -> Option<PathBuf> {
        crate::config::config_dir().map(|d| d.join("session.toml"))
    }

    /// Load from the default path. Missing file (or no known path) means an
    /// empty session; a corrupt file is also treated as empty so a bad write
    /// never bricks startup. Use [`Self::load_from_path`] when errors matter.
    pub fn load() -> Self {
        match Self::default_path() {
            Some(path) => Self::load_from_path(&path).unwrap_or_else(|_| Self::empty()),
            None => Self::empty(),
        }
    }

    pub fn load_from_path(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::empty());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read session {}", path.display()))?;
        // Empty file = empty session (e.g. freshly touched).
        if text.trim().is_empty() {
            return Ok(Self::empty());
        }
        let file: SessionFile =
            toml::from_str(&text).with_context(|| format!("bad session {}", path.display()))?;
        let projects = file.projects.iter().map(PathBuf::from).collect();
        Ok(Self {
            projects,
            current: file.current,
        })
    }

    /// Index clamped to the project list (0 when empty).
    pub fn current_clamped(&self) -> usize {
        if self.projects.is_empty() {
            0
        } else {
            self.current.min(self.projects.len() - 1)
        }
    }

    /// Only entries that still exist on disk (dead checkouts are dropped).
    /// Git-validity is checked later by `Workspace::open` / `Repo::discover`,
    /// so this stays a cheap filesystem filter.
    pub fn existing_projects(&self) -> Vec<PathBuf> {
        self.projects
            .iter()
            .filter(|p| p.as_path().exists())
            .cloned()
            .collect()
    }

    pub fn save_to_path(&self, path: &Path) -> Result<()> {
        let file = SessionFile {
            projects: self
                .projects
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
            current: self.current_clamped(),
        };
        let text = toml::to_string_pretty(&file).context("cannot serialize session")?;
        crate::config::write_atomic(path, &text)
    }

    /// Persist to the default path. No-op (Ok) when no path is known.
    #[allow(dead_code)]
    pub fn save(&self) -> Result<()> {
        match Self::default_path() {
            Some(path) => self.save_to_path(&path),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_means_empty_session() {
        let s =
            Session::load_from_path(Path::new("/nonexistent/session-git-tui-xyz.toml")).unwrap();
        assert!(s.projects.is_empty());
        assert_eq!(s.current_clamped(), 0);
    }

    #[test]
    fn roundtrip_preserves_projects_and_selection() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("session.toml");
        let s = Session::new(
            vec![PathBuf::from("/tmp/api"), PathBuf::from("/tmp/web")],
            1,
        );
        s.save_to_path(&path).unwrap();
        let back = Session::load_from_path(&path).unwrap();
        assert_eq!(back.projects, s.projects);
        assert_eq!(back.current, 1);
        assert_eq!(back.current_clamped(), 1);
    }

    #[test]
    fn out_of_range_selection_clamps() {
        let s = Session::new(vec![PathBuf::from("/tmp/a")], 99);
        assert_eq!(s.current_clamped(), 0);
        let empty = Session::empty();
        assert_eq!(empty.current_clamped(), 0);
    }

    #[test]
    fn corrupt_file_errors_on_explicit_load() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("session.toml");
        std::fs::write(&path, "projects = [unclosed\n").unwrap();
        assert!(Session::load_from_path(&path).is_err());
    }

    #[test]
    fn existing_projects_drops_missing_paths() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = Session::new(
            vec![
                dir.path().to_path_buf(),
                PathBuf::from("/definitely/not/here-git-tui-xyz"),
            ],
            1,
        );
        assert_eq!(s.existing_projects(), vec![dir.path().to_path_buf()]);
    }
}
