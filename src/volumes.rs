//! Mounted volumes and their capacity (statfs), the overview on the left.

use std::path::PathBuf;

#[derive(Clone)]
pub struct Volume {
    pub name: String,
    pub mount: PathBuf,
    pub total: u64,
    pub free: u64,
}

impl Volume {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }
}

#[cfg(target_os = "macos")]
pub fn list() -> Vec<Volume> {
    use std::ffi::CStr;
    let mut out = Vec::new();
    unsafe {
        let mut buf: *mut libc::statfs = std::ptr::null_mut();
        let n = libc::getmntinfo(&mut buf, libc::MNT_NOWAIT);
        for i in 0..n.max(0) as usize {
            let s = &*buf.add(i);
            let mount = CStr::from_ptr(s.f_mntonname.as_ptr())
                .to_string_lossy()
                .into_owned();
            let user_visible = mount == "/"
                || (mount.starts_with("/Volumes/") && s.f_flags & libc::MNT_DONTBROWSE as u32 == 0);
            if !user_visible || s.f_blocks == 0 {
                continue;
            }
            let bs = s.f_bsize as u64;
            let name = if mount == "/" {
                startup_name()
            } else {
                mount.trim_start_matches("/Volumes/").to_string()
            };
            out.push(Volume {
                name,
                mount: PathBuf::from(&mount),
                total: s.f_blocks * bs,
                free: s.f_bavail * bs,
            });
        }
    }
    out
}

#[cfg(not(target_os = "macos"))]
pub fn list() -> Vec<Volume> {
    let mut out = Vec::new();
    unsafe {
        let mut s: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c"/".as_ptr(), &mut s) == 0 {
            let bs = s.f_frsize as u64;
            out.push(Volume {
                name: "/".into(),
                mount: "/".into(),
                total: s.f_blocks as u64 * bs,
                free: s.f_bavail as u64 * bs,
            });
        }
    }
    out
}

/// "Macintosh HD": the link in /Volumes that points at "/".
#[cfg(target_os = "macos")]
fn startup_name() -> String {
    if let Ok(rd) = std::fs::read_dir("/Volumes") {
        for e in rd.flatten() {
            if std::fs::read_link(e.path())
                .map(|t| t == std::path::Path::new("/"))
                .unwrap_or(false)
            {
                return e.file_name().to_string_lossy().into_owned();
            }
        }
    }
    "Startup disk".into()
}
