@echo off
setlocal
cd /d "%~dp0"
set "BRIDGE=%~dp0target\release\antigravity-bridge.exe"
if not exist "%BRIDGE%" (
  echo Release dosyasi bulunamadi. Once cargo build --release calistirin.
  pause
  exit /b 1
)
"%BRIDGE%" init
if errorlevel 1 goto fail
"%BRIDGE%" patch --check %*
if errorlevel 1 goto fail
"%BRIDGE%" patch %*
if errorlevel 1 goto fail
echo Kurulum tamamlandi. BASLAT.cmd ile kopruyu, sonra Antigravity'yi acin.
pause
exit /b 0
:fail
echo Kurulum tamamlanamadi. Antigravity ve language_server.exe kapali olmali.
echo Uygulama zorla kapatilmaz. Hata mesajini inceleyin.
pause
exit /b 1
