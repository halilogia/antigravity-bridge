# Creates a root shortcut for Antigravity Bridge with the custom icon.
$ErrorActionPreference = "Stop"

$root = $PSScriptRoot
$cmd = Join-Path $root "BASLAT.cmd"
$ico = Join-Path $root "icon.ico"
$shortcutPath = Join-Path $root "Antigravity Bridge.lnk"

# Remove desktop shortcut if exists
$desktop = [System.Environment]::GetFolderPath([System.Environment+SpecialFolder]::Desktop)
$oldDesktopShortcut = Join-Path $desktop "Antigravity Bridge.lnk"
if (Test-Path $oldDesktopShortcut) {
    Remove-Item $oldDesktopShortcut -Force -ErrorAction SilentlyContinue
}

$WshShell = New-Object -ComObject WScript.Shell
$Shortcut = $WshShell.CreateShortcut($shortcutPath)
$Shortcut.TargetPath = "cmd.exe"
$Shortcut.Arguments = "/c `"`"$cmd`"`""
$Shortcut.WorkingDirectory = $root
$Shortcut.IconLocation = "$ico,0"
$Shortcut.Description = "Antigravity Model Bridge - Local AI Gateway"
$Shortcut.Save()

Write-Host "Proje koku kisayolu basariyla olusturuldu: $shortcutPath" -ForegroundColor Green

