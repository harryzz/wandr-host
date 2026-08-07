@echo off
rem Shim so `wandr` runs the sibling wandr.ps1 in both cmd and PowerShell.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0wandr.ps1" %*
