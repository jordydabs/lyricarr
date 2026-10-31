#!/bin/sh
# Lidarr Custom Script hook -> Lyricarr, for Linux. (Lidarr-Hook.cmd is the
# Windows one.)
#
# Point Lidarr's Custom Script connector at this file, next to the lyricarr
# binary, and make it executable: chmod +x Lidarr-Hook.sh lyricarr
#
# Lidarr passes the affected paths in the environment (lidarr_addedtrackpaths,
# lidarr_trackfile_path(s), lidarr_artist_path); lyricarr reads them itself,
# so no arguments are needed.
#
# The lookup runs detached, in the background. Lidarr waits for a custom
# script to finish, and an album's worth of lookups can take a minute - that
# minute should not hold up the import. (On Windows the same happens by
# itself: cmd.exe does not wait for lyricarr.exe.)
#
# Always exits 0 - a lyrics problem must never fail a Lidarr import.
here=$(dirname "$0")
if command -v setsid >/dev/null 2>&1; then
    setsid "$here/lyricarr" hook </dev/null >/dev/null 2>&1 &
else
    nohup "$here/lyricarr" hook </dev/null >/dev/null 2>&1 &
fi
exit 0
