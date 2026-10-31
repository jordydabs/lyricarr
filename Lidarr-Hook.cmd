@echo off
REM Lidarr Custom Script hook -> Lyricarr.
REM
REM The filename is deliberately unchanged: Lidarr stores this path in its
REM Connect settings, so pointing it at the new executable here means nothing
REM has to be re-entered in Lidarr.
REM
REM Lidarr passes the affected paths in the environment (lidarr_addedtrackpaths,
REM lidarr_trackfile_path(s), lidarr_artist_path); lyricarr.exe reads them
REM itself, so no arguments are needed.
REM
REM Always exits 0 - a lyrics problem must never fail a Lidarr import.
"%~dp0lyricarr.exe" hook
exit /b 0
