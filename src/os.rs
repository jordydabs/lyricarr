//! What Windows and Linux do differently, in one place.
//!
//! Everything else in Lyricarr is ordinary portable Rust. These are the jobs the
//! two systems do their own way: an exclusive lock, starting at sign-in,
//! scheduling a scan, running in the background, and showing a file in the
//! file manager.
//!
//! Every command line written here is built from a path that ends up inside
//! someone else's syntax - PowerShell, a systemd unit, a desktop entry - so
//! each has its own escaping, and anything that cannot be represented safely
//! (a newline in a path) is refused rather than written half-right.

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

/// Windows has the tray. Linux has no tray: a window that hides cannot be
/// relied on there (Wayland has no such thing), so background running is a
/// window-less service instead.
pub const HAS_TRAY: bool = cfg!(windows);

/// What "start at sign-in" is called on this system, for the window.
pub const START_AT_LOGIN_LABEL: &str = if cfg!(windows) {
    "Start with Windows, hidden in the tray"
} else {
    "Start when you log in, in the background (no window)"
};

/// The scheduler the "scheduled task" buttons talk to.
pub const SCHEDULER_NAME: &str = if cfg!(windows) { "Windows" } else { "systemd" };

pub fn exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("lyricarr"))
}

/// A command that runs this same program, with the same `--config`. Every
/// scan Lyricarr starts goes through here: they used to drop `--config`, so
/// with a config.toml anywhere but the usual place the window and its scans
/// were quietly working from two different files.
pub fn lyricarr() -> std::process::Command {
    let mut c = std::process::Command::new(exe());
    if let Some(p) = crate::config::explicit_path() {
        c.arg("--config").arg(p);
    }
    c
}

// ---------------------------------------------------------------------------
// locks
// ---------------------------------------------------------------------------

/// Take an exclusive lock on a path, or return None because someone else holds
/// it. Used for "only one window", "only one full scan" and "only one monitor".
///
/// Both kinds release themselves if the process is killed, which a pid file
/// cannot promise. The returned handle must be kept alive for as long as the
/// lock should hold - drop it and the lock is gone.
#[cfg(windows)]
pub fn exclusive_lock(lock_path: &Path) -> Option<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    if let Some(d) = lock_path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    // Holding a file open with no sharing is the cheapest exclusive lock
    // Windows offers.
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .share_mode(0) // no FILE_SHARE_READ/WRITE: a second open fails
        .open(lock_path)
        .ok()
}

#[cfg(unix)]
pub fn exclusive_lock(lock_path: &Path) -> Option<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    if let Some(d) = lock_path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .ok()?;
    // flock locks belong to the open file, so a second open of the same path
    // - in this process or another - is refused, the same as on Windows.
    let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    (r == 0).then_some(f)
}

// ---------------------------------------------------------------------------
// the window (Windows only - see HAS_TRAY)
// ---------------------------------------------------------------------------

/// Take the window off the screen, into the tray.
pub fn hide_window() {
    #[cfg(windows)]
    crate::winui::hide();
}

/// Whether the window is hidden in the tray right now.
pub fn window_hidden() -> bool {
    #[cfg(windows)]
    {
        crate::winui::is_hidden()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Show a track in the file manager - highlighted where the system can do
/// that, otherwise by opening its folder.
pub fn reveal(audio: &Path) {
    #[cfg(windows)]
    {
        // `explorer /select,` opens the folder with the track already
        // highlighted. (opener::reveal does the same, but only behind a
        // feature that drags in url and dbus.)
        if std::process::Command::new("explorer")
            .arg(format!("/select,{}", audio.display()))
            .spawn()
            .is_ok()
        {
            return;
        }
    }
    if let Some(d) = audio.parent() {
        let _ = opener::open(d);
    }
}

/// Where a folder picker should start when there is nothing better.
pub fn default_browse_dir() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from("C:/")
    } else {
        std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
    }
}

/// Whether two music folder paths name the same place, as far as the list of
/// folders is concerned. Windows paths are case-insensitive; Linux paths are
/// not, and `Music` and `music` can be two different folders.
pub fn same_path(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

// ---------------------------------------------------------------------------
// escaping
// ---------------------------------------------------------------------------

#[cfg(any(windows, test))]
/// A PowerShell single-quoted string. Inside one, the only special character
/// is the quote itself, written twice.
pub fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[cfg(any(not(windows), test))]
/// One argument on a systemd `ExecStart=` line: double-quoted, with C-style
/// backslash escapes, `%` doubled (it introduces specifiers) and `$` doubled
/// (it introduces environment variables).
pub fn systemd_quote(s: &str) -> Result<String> {
    if s.chars().any(|c| c.is_control()) {
        bail!("{s:?} cannot be written into a systemd unit (it contains a control character)");
    }
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out.push('"');
    Ok(out)
}

#[cfg(any(not(windows), test))]
/// One argument on a desktop entry's `Exec=` line. The spec layers two
/// escapes: quoting escapes `"`, `` ` ``, `$` and `\` with a backslash, and the
/// file format then escapes every backslash again - hence four for a literal
/// one. `%` introduces field codes and is doubled.
pub fn desktop_exec_quote(s: &str) -> Result<String> {
    if s.chars().any(|c| c.is_control()) {
        bail!("{s:?} cannot be written into a desktop entry (it contains a control character)");
    }
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' | '`' | '$' => {
                out.push_str("\\\\");
                out.push(c);
            }
            '\\' => out.push_str("\\\\\\\\"),
            '%' => out.push_str("%%"),
            c => out.push(c),
        }
    }
    out.push('"');
    Ok(out)
}

/// `HH:MM`, 00:00 to 23:59 - the only thing a schedule time may be.
pub fn valid_time(t: &str) -> Option<(u32, u32)> {
    let (h, m) = t.split_once(':')?;
    if h.is_empty() || h.len() > 2 || m.len() != 2 {
        return None;
    }
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some((h, m))
}

/// "Daily" or "Weekly", whatever the case, or nothing.
fn valid_schedule(s: &str) -> Option<bool> {
    if s.eq_ignore_ascii_case("daily") {
        Some(true)
    } else if s.eq_ignore_ascii_case("weekly") {
        Some(false)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// start at sign-in
// ---------------------------------------------------------------------------

/// Starting with Windows: an `HKCU\...\Run` value pointing at this
/// executable with `--background`.
///
/// Written through the registry API rather than by launching PowerShell. The
/// first version shelled out to `New-ItemProperty`, and Windows Defender
/// flagged the program as `Behavior:Win32/Persistence.A!ml` the moment the box
/// was ticked - an unsigned executable spawning PowerShell to add itself to
/// `Run` is the exact shape of the thing that heuristic is looking for. Doing
/// the write ourselves is also faster, needs no PowerShell, and flashes no
/// console window.
#[cfg(windows)]
mod run_key {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
        HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SZ,
    };

    const SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const NAME: &str = "Lyricarr";

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }

    fn open(write: bool) -> Option<HKEY> {
        let mut h: HKEY = std::ptr::null_mut();
        let rights = if write { KEY_WRITE } else { KEY_READ };
        let r = unsafe {
            RegOpenKeyExW(HKEY_CURRENT_USER, wide(SUBKEY).as_ptr(), 0, rights, &mut h)
        };
        if r == ERROR_SUCCESS { Some(h) } else { None }
    }

    pub fn set(on: bool) -> std::io::Result<()> {
        let exe = std::env::current_exe()?;
        let Some(h) = open(true) else {
            return Err(std::io::Error::other("cannot open the Run key for writing"));
        };
        let name = wide(NAME);
        let r = if on {
            let value = wide(&format!("\"{}\" --background", exe.display()));
            unsafe {
                RegSetValueExW(
                    h,
                    name.as_ptr(),
                    0,
                    REG_SZ,
                    value.as_ptr() as *const u8,
                    (value.len() * 2) as u32,
                )
            }
        } else {
            let r = unsafe { RegDeleteValueW(h, name.as_ptr()) };
            // Removing something that was never there is a success, not a failure.
            if r == 2 { ERROR_SUCCESS } else { r }
        };
        unsafe { RegCloseKey(h) };
        if r == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(std::io::Error::from_raw_os_error(r as i32))
        }
    }

    pub fn is_set() -> bool {
        let Some(h) = open(false) else { return false };
        let mut len: u32 = 0;
        let r = unsafe {
            RegQueryValueExW(
                h,
                wide(NAME).as_ptr(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut len,
            )
        };
        unsafe { RegCloseKey(h) };
        r == ERROR_SUCCESS
    }
}

/// Starting at log-in on Linux: an XDG autostart entry, which GNOME, KDE,
/// Xfce and the rest all read. It starts Lyricarr in the background - the
/// folder monitor and the automatic scans, with no window.
#[cfg(not(windows))]
mod autostart {
    use super::desktop_exec_quote;
    use std::path::PathBuf;

    fn path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("autostart").join("lyricarr.desktop"))
    }

    pub fn set(on: bool) -> std::io::Result<()> {
        let p = path().ok_or_else(|| std::io::Error::other("HOME is not set"))?;
        if !on {
            return match std::fs::remove_file(&p) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        let exe = std::env::current_exe()?;
        let exec = desktop_exec_quote(&exe.to_string_lossy())
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let body = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Lyricarr\n\
             Comment=Watch the music folders and fetch lyrics for new songs\n\
             Exec={exec} --background\n\
             Terminal=false\n\
             NoDisplay=true\n\
             X-GNOME-Autostart-enabled=true\n"
        );
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(&p, body)
    }

    pub fn is_set() -> bool {
        path().map(|p| p.is_file()).unwrap_or(false)
    }
}

pub fn set_start_at_login(on: bool) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        run_key::set(on)
    }
    #[cfg(not(windows))]
    {
        autostart::set(on)
    }
}

pub fn is_start_at_login() -> bool {
    #[cfg(windows)]
    {
        run_key::is_set()
    }
    #[cfg(not(windows))]
    {
        autostart::is_set()
    }
}

// ---------------------------------------------------------------------------
// the scheduled scan
// ---------------------------------------------------------------------------

/// Register (or remove) the scheduled scan: a Windows scheduled task, or a
/// systemd user timer.
pub fn install_task(schedule: &str, time: &str, remove: bool) -> Result<i32> {
    // Checked before anything is built from them: both end up inside another
    // program's syntax.
    let Some(daily) = valid_schedule(schedule) else {
        bail!("--schedule must be Daily or Weekly, not {schedule:?}");
    };
    let Some((h, m)) = valid_time(time) else {
        bail!("--time must be HH:MM (00:00 to 23:59), not {time:?}");
    };
    #[cfg(windows)]
    {
        windows_task(daily, h, m, remove)
    }
    #[cfg(not(windows))]
    {
        systemd::timer(daily, h, m, remove)
    }
}

/// Registered through PowerShell rather than schtasks, for one reason:
/// schtasks cannot set StartWhenAvailable, so a task made that way leaves it
/// False - and a weekly run missed while the machine was off is then skipped
/// rather than caught up. Set-ScheduledTask on an existing schtasks-made task
/// fails with "The parameter is incorrect (UserId)", so the whole thing is
/// registered here in one call. The principal is left to default to the
/// registering user with Interactive logon, so mapped drives and their
/// permissions match an interactive run.
#[cfg(windows)]
fn windows_task(daily: bool, h: u32, m: u32, remove: bool) -> Result<i32> {
    const NAME: &str = "Lyricarr - fetch lyrics";
    if remove {
        let out = std::process::Command::new("schtasks")
            .args(["/Delete", "/TN", NAME, "/F"])
            .output()?;
        println!("{}", String::from_utf8_lossy(&out.stdout).trim());
        if !out.status.success() {
            eprintln!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        return Ok(if out.status.success() { 0 } else { 1 });
    }

    let exe = std::env::current_exe()?;
    let dir = exe.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let at = ps_quote(&format!("{h:02}:{m:02}"));
    let trigger = if daily {
        format!("New-ScheduledTaskTrigger -Daily -At {at}")
    } else {
        format!("New-ScheduledTaskTrigger -Weekly -DaysOfWeek Sunday -At {at}")
    };
    // Every value is a quoted PowerShell literal. The executable's own path
    // used to be pasted in between bare quotes, so a folder with an
    // apostrophe in its name - "Bob's Apps" - broke the command, and a
    // stranger name could have run something.
    let ps = format!(
        "$ErrorActionPreference='Stop'; \
         $n={name}; \
         try {{ Unregister-ScheduledTask -TaskName $n -Confirm:$false }} catch {{}} \
         $a = New-ScheduledTaskAction -Execute {exe} -Argument 'scan --quiet' -WorkingDirectory {dir}; \
         $t = {trigger}; \
         $s = New-ScheduledTaskSettingsSet -StartWhenAvailable -MultipleInstances IgnoreNew \
              -ExecutionTimeLimit (New-TimeSpan -Hours 72) -DontStopIfGoingOnBatteries -AllowStartIfOnBatteries; \
         Register-ScheduledTask -TaskName $n -Action $a -Trigger $t -Settings $s \
              -Description 'Fetch synced lyrics from LRCLIB into .lrc sidecars' | Out-Null; \
         $v = Get-ScheduledTask -TaskName $n; \
         'registered: ' + $v.Actions[0].Execute + ' ' + $v.Actions[0].Arguments; \
         'StartWhenAvailable: ' + $v.Settings.StartWhenAvailable; \
         'next run: ' + ($v | Get-ScheduledTaskInfo).NextRunTime",
        name = ps_quote(NAME),
        exe = ps_quote(&exe.to_string_lossy()),
        dir = ps_quote(&dir.to_string_lossy()),
        trigger = trigger,
    );
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &ps])
        .output()?;
    print!("{}", String::from_utf8_lossy(&out.stdout));
    if !out.status.success() {
        eprintln!("{}", String::from_utf8_lossy(&out.stderr).trim());
        return Ok(1);
    }
    Ok(0)
}

/// The background service (Linux): `lyricarr watch` as a systemd user
/// service, so new music is watched for without a window or a desktop.
#[cfg(not(windows))]
pub fn install_service(remove: bool) -> Result<i32> {
    systemd::service(remove)
}

#[cfg(not(windows))]
mod systemd {
    use super::systemd_quote;
    use anyhow::{bail, Context, Result};
    use std::path::PathBuf;

    const TIMER: &str = "lyricarr-scan.timer";
    const SCAN: &str = "lyricarr-scan.service";
    const WATCH: &str = "lyricarr-watch.service";

    fn unit_dir() -> Result<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .context("HOME is not set, so there is nowhere to put a user unit")?;
        Ok(base.join("systemd").join("user"))
    }

    fn systemctl(args: &[&str]) -> Result<bool> {
        let out = std::process::Command::new("systemctl")
            .arg("--user")
            .args(args)
            .output()
            .context("could not run systemctl - is this a systemd system?")?;
        let err = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() && !err.trim().is_empty() {
            eprintln!("systemctl --user {}: {}", args.join(" "), err.trim());
        }
        Ok(out.status.success())
    }

    fn exec_start(args: &str) -> Result<String> {
        let exe = std::env::current_exe()?;
        Ok(format!("{} {args}", systemd_quote(&exe.to_string_lossy())?))
    }

    fn write(name: &str, body: &str) -> Result<()> {
        let dir = unit_dir()?;
        std::fs::create_dir_all(&dir)?;
        let p = dir.join(name);
        std::fs::write(&p, body).with_context(|| format!("writing {}", p.display()))?;
        println!("wrote {}", p.display());
        Ok(())
    }

    fn unwrite(names: &[&str]) -> Result<()> {
        let dir = unit_dir()?;
        for n in names {
            match std::fs::remove_file(dir.join(n)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
        }
        Ok(())
    }

    pub(super) fn scan_unit(exec: &str) -> String {
        format!(
            "[Unit]\n\
             Description=Lyricarr - fetch lyrics for new or due tracks\n\n\
             [Service]\n\
             Type=oneshot\n\
             ExecStart={exec}\n\
             Nice=10\n"
        )
    }

    /// `Persistent=true` is systemd's StartWhenAvailable: a run missed while
    /// the machine was off happens when it is next on.
    pub(super) fn timer_unit(daily: bool, h: u32, m: u32) -> String {
        let when = if daily {
            format!("*-*-* {h:02}:{m:02}:00")
        } else {
            format!("Sun *-*-* {h:02}:{m:02}:00")
        };
        format!(
            "[Unit]\n\
             Description=Lyricarr - scheduled scan\n\n\
             [Timer]\n\
             OnCalendar={when}\n\
             Persistent=true\n\n\
             [Install]\n\
             WantedBy=timers.target\n"
        )
    }

    /// Restarted if it ever dies; stopped with SIGTERM, which it handles by
    /// finishing what it has in flight.
    pub(super) fn watch_unit(exec: &str) -> String {
        format!(
            "[Unit]\n\
             Description=Lyricarr - watch the music folders for new music\n\
             After=network-online.target\n\n\
             [Service]\n\
             ExecStart={exec}\n\
             Restart=on-failure\n\
             RestartSec=30\n\n\
             [Install]\n\
             WantedBy=default.target\n"
        )
    }

    /// The scheduled scan.
    pub fn timer(daily: bool, h: u32, m: u32, remove: bool) -> Result<i32> {
        if remove {
            let _ = systemctl(&["disable", "--now", TIMER]);
            unwrite(&[TIMER, SCAN])?;
            let _ = systemctl(&["daemon-reload"]);
            println!("removed the scheduled scan");
            return Ok(0);
        }
        write(SCAN, &scan_unit(&exec_start("scan --quiet")?))?;
        write(TIMER, &timer_unit(daily, h, m))?;
        if !systemctl(&["daemon-reload"])? || !systemctl(&["enable", "--now", TIMER])? {
            bail!("the unit files are written, but systemd would not enable {TIMER}");
        }
        // The window shows the last line, so the confirmation goes last.
        println!(
            "to have it run while you are logged out too: loginctl enable-linger {}",
            std::env::var("USER").unwrap_or_else(|_| "$USER".into())
        );
        println!(
            "registered: {TIMER}, {} at {h:02}:{m:02}",
            if daily { "daily" } else { "Sundays" }
        );
        Ok(0)
    }

    /// The monitor as a service.
    pub fn service(remove: bool) -> Result<i32> {
        if remove {
            let _ = systemctl(&["disable", "--now", WATCH]);
            unwrite(&[WATCH])?;
            let _ = systemctl(&["daemon-reload"]);
            println!("removed the background service");
            return Ok(0);
        }
        write(WATCH, &watch_unit(&exec_start("watch")?))?;
        if !systemctl(&["daemon-reload"])? || !systemctl(&["enable", "--now", WATCH])? {
            bail!("the unit file is written, but systemd would not start {WATCH}");
        }
        println!(
            "to keep it running while you are logged out: loginctl enable-linger {}",
            std::env::var("USER").unwrap_or_else(|_| "$USER".into())
        );
        println!("started {WATCH}; `systemctl --user status {WATCH}` shows how it is doing");
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn powershell_quotes_cannot_be_escaped() {
        assert_eq!(ps_quote(r"C:\Apps\Lyricarr\lyricarr.exe"), r"'C:\Apps\Lyricarr\lyricarr.exe'");
        assert_eq!(ps_quote(r"C:\Bob's Apps\x.exe"), r"'C:\Bob''s Apps\x.exe'");
        // An attempt to close the string and run something stays inside it.
        assert_eq!(ps_quote("x'; Remove-Item C:\\ -Recurse; '"), "'x''; Remove-Item C:\\ -Recurse; '''");
    }

    #[test]
    fn systemd_arguments_are_quoted_and_escaped() {
        assert_eq!(systemd_quote("/opt/lyricarr/lyricarr").unwrap(), "\"/opt/lyricarr/lyricarr\"");
        assert_eq!(
            systemd_quote(r#"/home/a b/100% "x" $HOME\y"#).unwrap(),
            r#""/home/a b/100%% \"x\" $$HOME\\y""#
        );
        assert!(systemd_quote("/tmp/evil\nExecStartPost=/bin/sh").is_err());
    }

    #[test]
    fn desktop_entries_double_their_escapes() {
        assert_eq!(desktop_exec_quote("/usr/bin/lyricarr").unwrap(), "\"/usr/bin/lyricarr\"");
        assert_eq!(
            desktop_exec_quote(r#"/a b/$x`y"z\w 50%"#).unwrap(),
            r#""/a b/\\$x\\`y\\"z\\\\w 50%%""#
        );
        assert!(desktop_exec_quote("/x\nExec=/bin/sh").is_err());
    }

    #[test]
    fn times_and_schedules_are_checked() {
        assert_eq!(valid_time("04:00"), Some((4, 0)));
        assert_eq!(valid_time("4:05"), Some((4, 5)));
        assert_eq!(valid_time("23:59"), Some((23, 59)));
        for bad in ["24:00", "12:60", "12", "12:5", "x:00", "", "04:00'; evil", "123:00"] {
            assert_eq!(valid_time(bad), None, "{bad:?}");
        }
        assert_eq!(valid_schedule("DAILY"), Some(true));
        assert_eq!(valid_schedule("weekly"), Some(false));
        assert_eq!(valid_schedule("Weekly; evil"), None);
    }

    /// Nothing is written or registered by these: the inputs are refused
    /// before anything is built from them.
    #[test]
    fn a_bad_schedule_is_refused_before_anything_happens() {
        assert!(install_task("Hourly", "04:00", false).is_err());
        assert!(install_task("Weekly", "04:00'; evil", false).is_err());
        assert!(install_task("Daily", "25:00", false).is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn systemd_units_say_what_they_should() {
        let exec = format!("{} scan --quiet", systemd_quote("/opt/my apps/lyricarr").unwrap());
        let svc = systemd::scan_unit(&exec);
        assert!(svc.contains("ExecStart=\"/opt/my apps/lyricarr\" scan --quiet\n"), "{svc}");
        assert!(svc.contains("Type=oneshot"));
        let weekly = systemd::timer_unit(false, 4, 0);
        assert!(weekly.contains("OnCalendar=Sun *-*-* 04:00:00\n"), "{weekly}");
        assert!(weekly.contains("Persistent=true"));
        assert!(systemd::timer_unit(true, 3, 30).contains("OnCalendar=*-*-* 03:30:00\n"));
        let watch = systemd::watch_unit("\"/usr/bin/lyricarr\" watch");
        assert!(watch.contains("ExecStart=\"/usr/bin/lyricarr\" watch\nRestart=on-failure"));
        // Every line is `key=value`, a [section], or blank - nothing a path
        // could have smuggled in.
        for unit in [svc, weekly, watch] {
            for line in unit.lines() {
                assert!(
                    line.is_empty() || line.starts_with('[') || line.contains('='),
                    "odd line {line:?}"
                );
            }
        }
    }

    #[test]
    fn the_lock_is_exclusive_and_lets_go() {
        let p = std::env::temp_dir().join(format!("lyr-lock-{}", std::process::id())).join("x.lock");
        let a = exclusive_lock(&p).expect("first lock");
        assert!(exclusive_lock(&p).is_none(), "a second holder got the lock");
        drop(a);
        assert!(exclusive_lock(&p).is_some(), "the lock was not released");
    }
}
