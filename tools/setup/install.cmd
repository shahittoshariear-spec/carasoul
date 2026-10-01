@echo off
rem carasoul setup payload. IExpress unpacks this folder and runs `cmd /c
rem install.cmd` from inside it. Per-user on purpose: no UAC, and the app
rem registers its own startup entry the first time it runs.
setlocal
set "APPDIR=%LOCALAPPDATA%\Programs\carasoul"

rem A copy that is already running holds its exe open, so stop it before the
rem copy. It keeps no unsaved state, and the start below puts it back.
taskkill /im carasoul.exe /f >nul 2>nul
ping -n 2 127.0.0.1 >nul

if not exist "%APPDIR%" mkdir "%APPDIR%"
copy /y "%~dp0carasoul.exe" "%APPDIR%\carasoul.exe" >nul
if not exist "%APPDIR%\carasoul.exe" (
    echo Could not install carasoul into "%APPDIR%".
    pause
    exit /b 1
)
copy /y "%~dp0uninstall.cmd" "%APPDIR%\uninstall.cmd" >nul

rem Start Menu (app + uninstall) and desktop shortcuts. WScript.Shell is the
rem least fussy way to write a .lnk from a script.
powershell -NoProfile -ExecutionPolicy Bypass -Command ^
  "$ErrorActionPreference = 'SilentlyContinue';" ^
  "$app = Join-Path $env:LOCALAPPDATA 'Programs\carasoul';" ^
  "$exe = Join-Path $app 'carasoul.exe';" ^
  "$ws = New-Object -ComObject WScript.Shell;" ^
  "$l = $ws.CreateShortcut((Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\carasoul.lnk'));" ^
  "$l.TargetPath = $exe; $l.WorkingDirectory = $app; $l.IconLocation = $exe; $l.Description = 'carasoul wallpaper switcher'; $l.Save();" ^
  "$u = $ws.CreateShortcut((Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\Uninstall carasoul.lnk'));" ^
  "$u.TargetPath = (Join-Path $app 'uninstall.cmd'); $u.WorkingDirectory = $app; $u.IconLocation = $exe; $u.Save();" ^
  "$d = $ws.CreateShortcut((Join-Path ([Environment]::GetFolderPath('Desktop')) 'carasoul.lnk'));" ^
  "$d.TargetPath = $exe; $d.WorkingDirectory = $app; $d.IconLocation = $exe; $d.Save()"

rem Launch it once: it registers the startup entry, parks itself in the tray and
rem puts the how-to-drive-it balloon up.
start "" "%APPDIR%\carasoul.exe"
exit /b 0
