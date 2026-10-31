# Changelog

## 2.2.0

Lyricarr now runs on Linux as well as Windows, and this release is the
security and reliability pass over everything 2.1.0 added.

### Linux

- One binary for any 64-bit distribution with glibc 2.31 or newer. Nothing
  else to install: the windowing and OpenGL libraries are loaded when the
  window opens, so a headless server never needs them.
- The same window, and the same folder monitor - inotify instead of
  ReadDirectoryChangesW, with a watch on every folder, new folders picked up
  as they appear, and the kernel's watch limit reported (with the command
  that raises it) instead of silently watching less.
- No tray on Linux. Instead, **Start when you log in** runs Lyricarr in the
  background with no window, and `lyricarr install-service` sets the same
  thing up as a systemd user service for a machine with no desktop at all.
  `lyricarr watch` is the window-less monitor itself, on either system.
- `install-task` writes a systemd user timer (`Persistent=true`, so a missed
  run is caught up) where Windows gets a scheduled task.
- Config in `~/.config/lyricarr`, state in `~/.local/share/lyricarr` - or
  beside the binary, Windows-style, if a config.toml or data folder is
  already there.
- `Lidarr-Hook.sh`, which runs the hook detached so Lidarr's import does not
  wait for the lookups.
- The folder picker goes through the desktop portal on a thread of its own:
  run inline, it froze the window while open and GNOME offered to Force Quit
  it. Where there is no portal, the window says so; a new box takes a typed
  or pasted path on either system.

### Windows

- Ejecting a USB drive works while it is being watched. The open folder
  handle made Windows refuse ("this device is currently in use"); Lyricarr
  now lets go when Windows asks, and picks the drive up again when it is back.
- The scheduled-task installer quotes everything it hands PowerShell, and
  checks `--schedule` and `--time` before using them. An install folder with
  an apostrophe in its name broke the command before.

### Everywhere

- Only one monitor is active at a time: the window, or the background one.
  The other waits, and takes over if the active one stops. Two would each
  have looked up every new album.
- Every scan Lyricarr starts now carries the `--config` it was started with.
  With a config.toml anywhere but the usual place, the window's scans were
  quietly using a different file.
- `lyricarr add-folder DIR` adds a music folder from the command line.
- With no config.toml yet, adding a music folder creates one from the
  commented example, instead of failing with "config.toml does not exist".
- Folder names that TOML cannot take as-is - an apostrophe, a line break, a
  control character - are written escaped, so config.toml always parses.
- prefs.json is written to a temporary file and renamed into place, so the
  background monitor never reads half of it.
- Paths that cannot go into the monitor's batch file as one line of UTF-8
  are handed over as their nearest clean folder rather than lost.
- The watcher only says it is watching once it really is.
- Built and tested on real Windows and Linux machines by GitHub Actions on
  every push.

## 2.1.0

Lyricarr now keeps up with the library by itself. New music gets its lyrics
as it lands, full scans run on a schedule, and the window no longer stutters.

### Automation

- **Live folder monitoring.** Every music folder is watched, and anything new -
  copied in by hand, written by a download client, synced from elsewhere - is
  looked up a few seconds after it finishes copying. Works alongside the Lidarr
  hook. On by default; Settings -> Automation turns it off.
- New files are only read once they have stopped changing: quiet for the
  settle time (10 s by default), the same size on two looks, and openable. A
  folder moved in is walked once everything inside it has settled.
- A music folder that goes offline is retried, and checked over when it comes
  back. More changes than Windows can report at once falls back to walking
  that folder.
- **Automatic full scans** every day by default (or 6 h, 12 h, 3 days, a week,
  or off), counted from the last *full* scan.
- **Full rescan** button: walks everything and re-asks LRCLIB about every track
  without lyrics, with a confirmation that says how many and roughly how long.
- Adding a music folder in Settings starts looking through it straight away.
- The title bar shows what the monitor is doing, and the tray tooltip says how
  many folders are being watched.

### A smoother window

- The window is always dark. egui follows the Windows light/dark setting by
  default, and on a PC in light mode the window came up half light-themed:
  white buttons, dim grey text, and the wrong font sizes - and flipped if the
  Windows theme changed while it was open.
- No more database queries on the drawing thread. The track list, counts,
  artist coverage and log tail are read on a thread of their own, and skipped
  entirely when nothing in the database has changed. On a large library these
  queries were freezing the window for a noticeable moment every three
  seconds, and on every key typed in the search box.
- Search waits for a pause in typing instead of running on every key.
- Clicking or right-clicking on a track's text now works. Table text was
  selectable, so a click on the words went to the text rather than the row.
- The selected track and right-click actions follow the track itself, not its
  position in the list, so a refresh during a scan can no longer move the
  selection - or a "Forget it" - onto a different track.
- The status line under the coverage bar is always one line, so a scan starting
  or stopping no longer pushes the whole window down and back. The six count
  cards are a fixed width, so a number gaining a digit does not nudge the rest.
- The size slider applies when it is let go, instead of rescaling the window
  under the pointer while it is dragged.
- Installing or removing the scheduled task no longer freezes the window while
  PowerShell runs.
- Faint outlines on checkboxes, text boxes and buttons, which were dark grey on
  dark grey.
- Messages in the bottom bar clear themselves after a few seconds.
- Starting in the tray (`--background`) no longer takes keyboard focus.

### Scanning

- `scan --path` can be given more than once, and takes files as well as
  folders. `scan --paths-from FILE` reads the list from a file.
- A hook or monitor run no longer overwrites a running full scan's progress
  line with its own numbers, or makes the window show the full scan's Stop
  button.
- Stop now stops whichever scans were running when it was pressed. Before,
  a hook run going alongside the full scan could take the stop request for
  itself and leave the full scan running.
- A scan with no music folders configured no longer leaves the window saying
  SCANNING for a minute.
- Targeted runs log one line per track looked up, so the Activity tab shows
  what happened to new music.

### Build

- The icon and version information are embedded when cross-compiling too:
  `build.rs` checked the build machine's OS rather than the target's.

## 2.0.0

First public release. One `lyricarr.exe` — window, command line, Lidarr hook
and scheduled-task installer — replacing an earlier Python version that needed
a Python install, `mutagen`, Tk, and four separate `.cmd` / `.ps1` files.

### The window

- Five tabs: Tracks, Find by hand, Coverage, Activity, Settings.
- Resizable, sortable columns; widths, sort, filter, window size and current
  tab are remembered.
- A live progress line under the coverage bar: files walked, lookups and rate,
  what came back, how long it has been running.
- Right-click row actions: find by hand, view the lyrics file, show in
  Explorer, copy path, mark instrumental, forget.
- Adjustable overall size (0.8×–1.6×) and table row height.

### Finding lyrics that a scan can't

- **Find by hand** — search LRCLIB yourself, preview candidates with the
  length difference against your file called out, and pick one. The result is
  recorded as `chosen by hand` so later scans leave it alone.
- **Coverage** — worst-covered artists first, so a targeted run can be aimed
  where it pays.
- **Stray lyrics finder** — lyrics files with no audio file beside them, which
  nothing else would ever show you.

### Tray and background

- Tray icon with a full menu that works with no window open.
- Left-click opens the program; right-click opens the menu.
- Close-to-tray (default on), minimise-to-tray, start-with-Windows.
- `--background` starts with no window at all.
- Only one window at a time: a second copy raises the first.

### Scanning

- Only one full scan at a time, enforced with a real lock —
  "start a scan when Lyricarr opens", the tray menu and the weekly task can no
  longer pile on top of each other. `--path` runs (the Lidarr hook) are exempt.
- Music folders can be added and removed from Settings with a folder picker;
  the change rewrites one line of `config.toml` and keeps its comments.
- A scan with no folders configured says so instead of finishing instantly.
- Unreachable folders are called out rather than silently scanning nothing.

### Correctness

- `src/text.rs` is a faithful port of CPython's
  `difflib.SequenceMatcher.ratio()`, because the matching thresholds are
  calibrated against it. Checked by `cargo test` against 40 vectors generated
  from the real `difflib`, plus 150 assertions over the normalisers.
- A transport failure is recorded as `error` and retried on the next run —
  never as "this track has no lyrics", which would park it for a week.
- The scheduled task sets `StartWhenAvailable`, so a run missed while the
  machine was off is caught up rather than skipped.
