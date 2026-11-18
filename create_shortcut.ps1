# Creates a Desktop shortcut for Antigravity Bridge with the custom icon.
$ErrorActionPreference = "Stop"

$desktop = [System.Environment]::GetFolderPath([System.Environment+SpecialFolder]::Desktop)
$root = $PSScriptRoot
$cmd = Join-Path $root "BASLAT.cmd"
$ico = Join-Path $root "icon.ico"
$shortcutPath = Join-Path $desktop "Antigravity Bridge.lnk"

$WshShell = New-Object -ComObject WScript.Shell
$Shortcut = $WshShell.CreateShortcut($shortcutPath)
$Shortcut.TargetPath = "cmd.exe"
$Shortcut.Arguments = "/c `"`"$cmd`"`""
$Shortcut.WorkingDirectory = $root
$Shortcut.IconLocation = "$ico,0"
$Shortcut.Description = "Antigravity Model Bridge - Local AI Gateway"
$Shortcut.Save()

Write-Host "Masaustu kisayolu basariyla olusturuldu: $shortcutPath" -ForegroundColor Green
