//! Moves an item to the Trash. This is the only place the app changes the disk, and it
//! runs only after the user confirms the list in the Collector. Items go to the Trash
//! (never a permanent delete), so Finder's "Put Back" can restore them.

use std::path::Path;

/// Folders the app refuses to touch whatever is selected.
const PROTECTED: &[&str] = &[
    "/",
    "/System",
    "/usr",
    "/bin",
    "/sbin",
    "/etc",
    "/var",
    "/private",
    "/Library",
    "/Applications",
    "/Users",
    "/Volumes",
    "/opt",
    "/cores",
    "/dev",
];

pub fn refuse_reason(p: &Path) -> Option<&'static str> {
    let s = p.to_string_lossy();
    if PROTECTED.iter().any(|q| s == *q) {
        return Some("system folder");
    }
    if s.starts_with("/System/")
        || s.starts_with("/usr/")
        || s.starts_with("/bin/")
        || s.starts_with("/sbin/")
        || s.starts_with("/private/var/db/")
    {
        return Some("inside a system folder");
    }
    if let Some(home) = std::env::var_os("HOME") {
        if p == Path::new(&home) {
            return Some("your home folder");
        }
    }
    None
}

#[cfg(target_os = "macos")]
pub fn to_trash(p: &Path) -> Result<(), String> {
    use objc2_foundation::{NSFileManager, NSString, NSURL};
    if let Some(r) = refuse_reason(p) {
        return Err(format!("refused: {r}"));
    }
    let s = NSString::from_str(&p.to_string_lossy());
    #[allow(unused_unsafe)]
    unsafe {
        let url = NSURL::fileURLWithPath(&s);
        let fm = NSFileManager::defaultManager();
        fm.trashItemAtURL_resultingItemURL_error(&url, None)
            .map_err(|e| e.localizedDescription().to_string())
    }
}

#[cfg(not(target_os = "macos"))]
pub fn to_trash(_p: &Path) -> Result<(), String> {
    Err("moving to the Trash is only built for macOS".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_system_folders() {
        assert!(refuse_reason(Path::new("/")).is_some());
        assert!(refuse_reason(Path::new("/System/Library")).is_some());
        assert!(refuse_reason(Path::new("/usr/local/bin/x")).is_some());
        assert!(refuse_reason(Path::new(&std::env::var("HOME").unwrap())).is_some());
        assert!(refuse_reason(Path::new("/Users/someone/Downloads/big.iso")).is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn moves_a_file_to_the_trash() {
        let dir = std::env::temp_dir().join("spatium-trash-test");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("spatium-test-file.txt");
        std::fs::write(&f, b"spatium test").unwrap();
        to_trash(&f).unwrap();
        assert!(!f.exists());
    }
}
