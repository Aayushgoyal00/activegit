//! `activegit --shortcut`: put ActiveGit in the Start menu and on the
//! desktop (Windows), so it can be started with a click and pinned to the
//! taskbar instead of typed into a terminal.
//!
//! The shortcut starts `activegit.exe --hold --pick` in the user's home
//! folder; through Windows Terminal's `ActiveGit` profile when Windows
//! Terminal is installed, so the font chosen in the app applies (see
//! [`crate::font`]). `--pick` opens the start screen (recent projects, home folder,
//! drives) so the project is chosen first; `--hold` keeps the window open
//! on a startup error so the message can be read.

use anyhow::Result;

/// What the shortcut is called in the Start menu and on the desktop.
#[cfg(any(windows, test))]
const SHORTCUT_NAME: &str = "ActiveGit";

/// The PowerShell that writes the `.lnk` files. Inputs come in through the
/// environment (no quoting to get wrong): `ACTIVEGIT_EXE` is the program,
/// `ACTIVEGIT_SHORTCUT_NAME` the name, and `ACTIVEGIT_SHORTCUT_DIR`, when
/// set, the one folder to write to instead of Start menu + desktop. It
/// prints one created path per line.
#[cfg(any(windows, test))]
const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$exe = $env:ACTIVEGIT_EXE
$name = $env:ACTIVEGIT_SHORTCUT_NAME
# Through Windows Terminal's ActiveGit profile when there is one (its font
# is the one picked in the app); the command is passed too, so it still
# starts ActiveGit before Windows Terminal has noticed the profile.
$target = $exe
$arguments = '--hold --pick'
$wt = Join-Path $env:LOCALAPPDATA 'Microsoft\WindowsApps\wt.exe'
if ($env:ACTIVEGIT_PROFILE -and (Test-Path -LiteralPath $wt)) {
    $target = $wt
    $arguments = '-p "' + $env:ACTIVEGIT_PROFILE + '" "' + $exe + '" --hold --pick'
}
if ($env:ACTIVEGIT_SHORTCUT_DIR) {
    $dirs = @($env:ACTIVEGIT_SHORTCUT_DIR)
} else {
    $dirs = @([Environment]::GetFolderPath('Programs'), [Environment]::GetFolderPath('Desktop'))
    # A shortcut already pinned to the taskbar is a copy: bring it up to date
    # too, but never pin one that is not there.
    $pinned = Join-Path $env:APPDATA 'Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar'
    if (Test-Path -LiteralPath (Join-Path $pinned ($name + '.lnk'))) { $dirs += $pinned }
}
$shell = New-Object -ComObject WScript.Shell
foreach ($dir in $dirs) {
    if (-not $dir) { continue }
    if (-not (Test-Path -LiteralPath $dir)) { New-Item -ItemType Directory -Path $dir | Out-Null }
    $path = Join-Path $dir ($name + '.lnk')
    $link = $shell.CreateShortcut($path)
    $link.TargetPath = $target
    $link.Arguments = $arguments
    $link.WorkingDirectory = [Environment]::GetFolderPath('UserProfile')
    $link.IconLocation = $exe + ',0'
    $link.Description = 'ActiveGit: a keyboard and mouse git client for the terminal'
    $link.Save()
    Write-Output $path
}
"#;

/// Create the shortcuts and say where they are and how to pin one.
#[cfg(windows)]
pub fn install() -> Result<()> {
    use anyhow::Context;
    let exe = std::env::current_exe().context("cannot find the activegit program file")?;
    // A shortcut to a build inside a `target` folder breaks on the next
    // `cargo clean`; say so rather than leave a dead link behind.
    let from_build_dir = exe
        .components()
        .any(|part| part.as_os_str().eq_ignore_ascii_case("target"));
    // ActiveGit's own Windows Terminal profile, keeping a font already
    // chosen. Without it the shortcut starts the program directly.
    let profile = crate::font::fragment_dir().is_some_and(|dir| {
        let font = crate::font::load(&dir);
        match crate::font::save(&dir, &exe, &font) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("note: no Windows Terminal profile, so no font choice: {e:#}");
                false
            }
        }
    });
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            SCRIPT,
        ])
        .env("ACTIVEGIT_EXE", &exe)
        .env("ACTIVEGIT_SHORTCUT_NAME", SHORTCUT_NAME)
        .env(
            "ACTIVEGIT_PROFILE",
            if profile { crate::font::PROFILE } else { "" },
        )
        .output()
        .context("cannot run PowerShell to create the shortcut")?;
    if !out.status.success() {
        anyhow::bail!(
            "could not create the shortcut: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let created = String::from_utf8_lossy(&out.stdout);
    println!("Created shortcuts to {}:", exe.display());
    for path in created.lines().map(str::trim).filter(|l| !l.is_empty()) {
        println!("  {path}");
    }
    println!();
    println!("ActiveGit is now in the Start menu and on the desktop.");
    println!("To keep it on the taskbar: right-click the desktop shortcut (or the");
    println!("Start menu entry) and choose \"Pin to taskbar\" (on Windows 11 it is");
    println!("under \"Show more options\").");
    if from_build_dir {
        println!();
        println!("Note: this is a build inside a `target` folder. Install it first");
        println!("(cargo install --path git-tui) and run --shortcut again, or the");
        println!("shortcut stops working when that folder is cleaned.");
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn install() -> Result<()> {
    anyhow::bail!(
        "--shortcut creates Start-menu and desktop shortcuts on Windows only; \
         on this system add a launcher that runs `activegit` in your terminal"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_starts_the_program_held_open_with_its_own_icon() {
        // The pieces a working, pinnable shortcut needs.
        for needle in [
            "$link.TargetPath = $target",
            "$arguments = '--hold --pick'",
            "'-p \"' + $env:ACTIVEGIT_PROFILE",
            "$link.IconLocation = $exe + ',0'",
            "GetFolderPath('Programs')",
            "GetFolderPath('Desktop')",
            "GetFolderPath('UserProfile')",
        ] {
            assert!(SCRIPT.contains(needle), "{needle} missing from the script");
        }
        assert_eq!(SHORTCUT_NAME, "ActiveGit");
    }

    /// Runs the real script against a temp folder and reads the shortcut
    /// back through the same COM object Explorer uses.
    #[cfg(windows)]
    #[test]
    fn writes_a_shortcut_that_points_at_the_program() {
        let dir = tempfile::TempDir::new().unwrap();
        let exe = dir.path().join("activegit.exe");
        std::fs::write(&exe, b"not really a program").unwrap();
        let run = |script: &str| {
            std::process::Command::new("powershell")
                .args(["-NoProfile", "-NonInteractive", "-Command", script])
                .env("ACTIVEGIT_EXE", &exe)
                .env("ACTIVEGIT_SHORTCUT_NAME", SHORTCUT_NAME)
                .env("ACTIVEGIT_SHORTCUT_DIR", dir.path().join("links"))
                .output()
                .unwrap()
        };
        let out = run(SCRIPT);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let link = dir.path().join("links").join("ActiveGit.lnk");
        assert!(link.exists(), "no shortcut written");
        let read_back = run("$l = (New-Object -ComObject WScript.Shell).CreateShortcut(\
             (Join-Path $env:ACTIVEGIT_SHORTCUT_DIR 'ActiveGit.lnk')); \
             Write-Output $l.TargetPath; Write-Output $l.Arguments; Write-Output $l.IconLocation");
        let text = String::from_utf8_lossy(&read_back.stdout).to_lowercase();
        let lines: Vec<&str> = text.lines().map(str::trim).collect();
        assert!(lines[0].ends_with("activegit.exe"), "target: {lines:?}");
        assert_eq!(lines[1], "--hold --pick");
        assert!(lines[2].ends_with("activegit.exe,0"), "icon: {lines:?}");
    }
}
