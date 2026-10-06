@echo off
rem `cargo run` for the Android app; same switches as run.ps1 (see its header).
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0run.ps1" %*
