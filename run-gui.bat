@echo off
REM Run the NewCormanLisp GUI REPL
powershell -ExecutionPolicy Bypass -File "%~dp0tools\Start-Gui.ps1" %*
