//! The drives a path can start from, with the name and free space that
//! Explorer's "This PC" shows next to each. Elsewhere there is one root.

use std::path::PathBuf;

/// One drive (`C:\`), or `/` off Windows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drive {
    pub root: PathBuf,
    /// Volume name ("Windows", "New Volume"); empty when it has none.
    pub label: String,
    /// Free and total bytes; `None` when the drive would not say.
    pub space: Option<(u64, u64)>,
}

impl Drive {
    /// What goes next to the drive: `Windows · 34.3 GB free of 275 GB`.
    pub fn note(&self) -> String {
        let space = self
            .space
            .map(|(free, total)| format!("{} free of {}", size(free), size(total)));
        match (self.label.is_empty(), space) {
            (false, Some(space)) => format!("{} \u{b7} {space}", self.label),
            (false, None) => self.label.clone(),
            (true, Some(space)) => space,
            (true, None) => String::new(),
        }
    }
}

/// Bytes the way Explorer prints them: `34.3 GB`, `275 GB`, `512 MB`.
fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 || value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(not(windows))]
pub fn list() -> Vec<Drive> {
    vec![Drive {
        root: PathBuf::from("/"),
        label: String::new(),
        space: None,
    }]
}

#[cfg(windows)]
pub fn list() -> Vec<Drive> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLogicalDrives() -> u32;
        fn GetVolumeInformationW(
            root: *const u16,
            name: *mut u16,
            name_len: u32,
            serial: *mut u32,
            max_component: *mut u32,
            flags: *mut u32,
            file_system: *mut u16,
            file_system_len: u32,
        ) -> i32;
        fn GetDiskFreeSpaceExW(
            dir: *const u16,
            free_to_caller: *mut u64,
            total: *mut u64,
            total_free: *mut u64,
        ) -> i32;
    }

    // SAFETY: no arguments, returns a bit per drive letter.
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    // A: and B: are floppy letters; asking an empty one can stall.
    for (bit, letter) in ('A'..='Z').enumerate().skip(2) {
        if mask & (1 << bit) == 0 {
            continue;
        }
        let root = PathBuf::from(format!("{letter}:\\"));
        let wide: Vec<u16> = root.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut name = [0u16; 261];
        // SAFETY: `wide` is NUL-terminated, `name` is writable for the
        // length passed, and the outputs not asked for are null.
        let named = unsafe {
            GetVolumeInformationW(
                wide.as_ptr(),
                name.as_mut_ptr(),
                name.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            )
        } != 0;
        let label = if named {
            let len = name.iter().position(|c| *c == 0).unwrap_or(name.len());
            String::from_utf16_lossy(&name[..len])
        } else {
            String::new()
        };
        let (mut free, mut total) = (0u64, 0u64);
        // SAFETY: `wide` is NUL-terminated and both outputs are valid.
        let sized = unsafe {
            GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, std::ptr::null_mut())
        } != 0;
        out.push(Drive {
            root,
            label,
            space: sized.then_some((free, total)),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_like_explorer() {
        const GB: u64 = 1024 * 1024 * 1024;
        assert_eq!(size(0), "0 bytes");
        assert_eq!(size(512 * 1024 * 1024), "512 MB");
        assert_eq!(size(GB * 343 / 10), "34.3 GB");
        assert_eq!(size(275 * GB), "275 GB");
        assert_eq!(size(2 * 1024 * GB), "2.0 TB");
    }

    #[test]
    fn note_joins_whatever_is_known() {
        const GB: u64 = 1024 * 1024 * 1024;
        let mut drive = Drive {
            root: PathBuf::from("C:\\"),
            label: "Windows".into(),
            space: Some((10 * GB, 200 * GB)),
        };
        assert_eq!(drive.note(), "Windows \u{b7} 10.0 GB free of 200 GB");
        drive.space = None;
        assert_eq!(drive.note(), "Windows");
        drive.label.clear();
        assert_eq!(drive.note(), "");
    }

    #[test]
    fn lists_the_drive_this_runs_from() {
        let drives = list();
        assert!(!drives.is_empty());
        let here = std::env::current_dir().unwrap();
        let here = dunce::canonicalize(&here).unwrap_or(here);
        assert!(
            drives.iter().any(|d| here.starts_with(&d.root)),
            "{here:?} is on none of {drives:?}"
        );
        // Every listed root is a top: nothing above it.
        assert!(drives.iter().all(|d| d.root.parent().is_none()));
    }
}
