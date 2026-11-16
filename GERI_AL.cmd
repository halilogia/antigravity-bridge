@echo off
setlocal
cd /d "%~dp0"
"%~dp0target\release\antigravity-bridge.exe" restore %*
if errorlevel 1 (
  echo Geri yukleme reddedildi. Yukaridaki hatayi inceleyin.
  pause
  exit /b 1
)
pause
