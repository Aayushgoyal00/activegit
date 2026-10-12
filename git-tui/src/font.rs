//! ActiveGit's own Windows Terminal profile, and the font it uses.
//!
//! A terminal program cannot set its font: the terminal owns it. Windows
//! Terminal lets an application add a profile of its own through a JSON
//! "fragment" (`%LOCALAPPDATA%\Microsoft\Windows Terminal\Fragments\
//! ActiveGit\activegit.json`), so the font picker changes that profile and
//! nothing else: other terminals and other profiles keep their font. The
//! shortcut (`activegit --shortcut`) opens ActiveGit in this profile.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Name of the Windows Terminal profile (`wt -p ActiveGit`).
pub const PROFILE: &str = "ActiveGit";
/// Identifies the profile to Windows Terminal; must never change.
const GUID: &str = "{6f0c2a4e-8d1b-5e37-9c4a-ac71ee617a01}";
const FRAGMENT: &str = "activegit.json";
const ICON: &str = "activegit.png";

/// Fonts to choose from: monospaced faces that ship with Windows or with
/// Windows Terminal. Consolas is what VS Code uses on Windows.
pub const FACES: [&str; 5] = [
    "Consolas",
    "Cascadia Mono",
    "Cascadia Code",
    "Courier New",
    "Lucida Console",
];
/// First row of the picker: leave the face to the terminal.
pub const TERMINAL_DEFAULT: &str = "Terminal default";
pub const DEFAULT_SIZE: u8 = 12;
pub const MIN_SIZE: u8 = 8;
pub const MAX_SIZE: u8 = 28;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Font {
    /// `None`: whatever face the terminal uses by default.
    pub face: Option<String>,
    /// Points.
    pub size: u8,
}

impl Default for Font {
    fn default() -> Self {
        Self {
            face: None,
            size: DEFAULT_SIZE,
        }
    }
}

impl Font {
    /// `Consolas 12`, `Terminal default 12`.
    pub fn describe(&self) -> String {
        format!(
            "{} {}",
            self.face.as_deref().unwrap_or(TERMINAL_DEFAULT),
            self.size
        )
    }
}

/// Where Windows Terminal looks for ActiveGit's profile; `None` off
/// Windows, where there is no such thing.
pub fn fragment_dir() -> Option<PathBuf> {
    if !cfg!(windows) {
        return None;
    }
    let local = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty())?;
    Some(
        PathBuf::from(local)
            .join("Microsoft")
            .join("Windows Terminal")
            .join("Fragments")
            .join(PROFILE),
    )
}

/// The font the profile in `dir` asks for; the default when there is no
/// profile yet or it cannot be read.
pub fn load(dir: &Path) -> Font {
    let read = || -> Option<Font> {
        let text = std::fs::read_to_string(dir.join(FRAGMENT)).ok()?;
        let json: serde_json::Value = serde_json::from_str(&text).ok()?;
        let font = json.get("profiles")?.get(0)?.get("font")?;
        Some(Font {
            face: font
                .get("face")
                .and_then(|face| face.as_str())
                .map(str::to_string),
            size: font
                .get("size")
                .and_then(|size| size.as_u64())
                .map(|size| size.clamp(MIN_SIZE as u64, MAX_SIZE as u64) as u8)
                .unwrap_or(DEFAULT_SIZE),
        })
    };
    read().unwrap_or_default()
}

/// Write the profile into `dir`: `exe` started on the start screen, with
/// the app's icon and `font`.
pub fn save(dir: &Path, exe: &Path, font: &Font) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let icon = dir.join(ICON);
    std::fs::write(&icon, include_bytes!("../../assets/activegit.png"))
        .with_context(|| format!("cannot write {}", icon.display()))?;
    let mut font_json = serde_json::json!({ "size": font.size });
    if let Some(face) = &font.face {
        font_json["face"] = serde_json::json!(face);
    }
    let json = serde_json::json!({
        "profiles": [{
            "guid": GUID,
            "name": PROFILE,
            "commandline": format!("\"{}\" --hold --pick", exe.display()),
            "icon": icon.display().to_string(),
            "startingDirectory": "%USERPROFILE%",
            "font": font_json,
        }]
    });
    let text = serde_json::to_string_pretty(&json).context("cannot write the profile")?;
    crate::config::write_atomic(&dir.join(FRAGMENT), &text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_broken_profile_means_the_default_font() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(load(dir.path()), Font::default());
        std::fs::write(dir.path().join(FRAGMENT), "{ not json").unwrap();
        assert_eq!(load(dir.path()), Font::default());
        assert_eq!(Font::default().describe(), "Terminal default 12");
    }

    #[test]
    fn saved_font_reads_back_and_the_profile_starts_activegit() {
        let dir = tempfile::TempDir::new().unwrap();
        let dir = dir.path().join("Fragments").join(PROFILE);
        let exe = Path::new(r"C:\Program Files\x\activegit.exe");
        let font = Font {
            face: Some("Consolas".into()),
            size: 14,
        };
        save(&dir, exe, &font).unwrap();
        assert_eq!(load(&dir), font);
        assert_eq!(font.describe(), "Consolas 14");
        let text = std::fs::read_to_string(dir.join(FRAGMENT)).unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        let profile = &json["profiles"][0];
        assert_eq!(profile["name"], PROFILE);
        assert_eq!(profile["guid"], GUID);
        // A path with a space survives: quoted in the command line.
        assert_eq!(
            profile["commandline"],
            r#""C:\Program Files\x\activegit.exe" --hold --pick"#
        );
        assert!(dir.join(ICON).exists());
        // No face: the key is left out so the terminal picks.
        save(&dir, exe, &Font::default()).unwrap();
        let text = std::fs::read_to_string(dir.join(FRAGMENT)).unwrap();
        assert!(!text.contains("\"face\""), "{text}");
        assert_eq!(load(&dir), Font::default());
    }

    #[test]
    fn an_out_of_range_size_is_clamped() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join(FRAGMENT),
            r#"{"profiles":[{"font":{"face":"Consolas","size":300}}]}"#,
        )
        .unwrap();
        assert_eq!(load(dir.path()).size, MAX_SIZE);
    }
}
