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
"%BRIDGE%" check
if errorlevel 1 goto fail
echo Yonetim token'i:
"%BRIDGE%" admin-token
echo Varsayilan yonetim adresi: http://127.0.0.1:4000/ui
echo Bu pencereyi acik tutun. Sonra Antigravity'yi baslatin.
"%BRIDGE%" serve
if errorlevel 1 goto fail
exit /b 0
:fail
echo Islem basarisiz. Yukaridaki hata mesajini inceleyin.
pause
exit /b 1
