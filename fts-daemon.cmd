@echo off
rem fts-daemon.cmd — cmd-runnable wrapper for fts-daemon.ps1.
rem Bypasses PowerShell execution policy per-invocation (no admin needed).
rem Usage: fts-daemon.cmd start | stop | restart | status
rem Tip: fts.exe has native commands too — "fts start", "fts stop", "fts status".
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0fts-daemon.ps1" %*
