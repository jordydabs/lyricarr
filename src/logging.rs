//! A small rotating appender, same file and same 5 MB x 3 rotation as the
//! Python so the GUI's Activity tab and `--open-log` keep working unchanged.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const MAX_BYTES: u64 = 5 * 1024 * 1024;
const KEEP: usize = 3;

#[derive(Clone)]
pub struct Log {
    inner: Arc<Mutex<Inner>>,
    pub quiet: bool,
}

struct Inner {
    path: PathBuf,
    file: Option<std::fs::File>,
}

impl Log {
    pub fn open(path: &Path, quiet: bool) -> Self {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d).ok();
        }
        let file = std::fs::OpenOptions::new().create(true).append(true).open(path).ok();
        Self {
            inner: Arc::new(Mutex::new(Inner { path: path.to_path_buf(), file })),
            quiet,
        }
    }

    /// A logger that only prints, for commands with no log file of their own.
    pub fn console(quiet: bool) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner { path: PathBuf::new(), file: None })),
            quiet,
        }
    }

    pub fn info(&self, msg: &str) {
        let line = format!(
            "{} INFO    {}\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            msg
        );
        if !self.quiet {
            print!("{}", line);
            let _ = std::io::stdout().flush();
        }
        let mut g = self.inner.lock().unwrap();
        if g.file.is_none() {
            return;
        }
        // rotate when the live file passes the ceiling
        let too_big = g
            .file
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .map(|m| m.len() > MAX_BYTES)
            .unwrap_or(false);
        if too_big {
            let path = g.path.clone();
            g.file = None;
            for i in (1..KEEP).rev() {
                let from = with_suffix(&path, i);
                let to = with_suffix(&path, i + 1);
                let _ = std::fs::rename(&from, &to);
            }
            let _ = std::fs::rename(&path, with_suffix(&path, 1));
            g.file = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok();
        }
        if let Some(f) = g.file.as_mut() {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

fn with_suffix(p: &Path, n: usize) -> PathBuf {
    let mut s = p.as_os_str().to_string_lossy().to_string();
    s.push('.');
    s.push_str(&n.to_string());
    PathBuf::from(s)
}
