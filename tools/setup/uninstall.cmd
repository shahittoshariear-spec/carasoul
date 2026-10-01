@echo off
rem carasoul uninstaller. It copies itself to %TEMP% before doing anything,
rem because a batch file cannot reliably delete the folder it is running from.
if not "%~1"=="--go" (
    copy /y "%~f0" "%TEMP%\carasoul-uninstall.cmd" >nul
    start "" "%TEMP%\carasoul-uninstall.cmd" --go
    exit /b 0
)

setlocal
set "APPDIR=%LOCALAPPDATA%\Programs\carasoul"

rem Let the app take its own startup entry out before the exe is deleted.
if exist "%APPDIR%\carasoul.exe" "%APPDIR%\carasoul.exe" --uninstall
taskkill /im carasoul.exe /f >nul 2>nul

del "%APPDATA%\Microsoft\Windows\Start Menu\Programs\carasoul.lnk" >nul 2>nul
del "%APPDATA%\Microsoft\Windows\Start Menu\Programs\Uninstall carasoul.lnk" >nul 2>nul
del "%USERPROFILE%\Desktop\carasoul.lnk" >nul 2>nul

rmdir /s /q "%LOCALAPPDATA%\carasoul" >nul 2>nul
rem The app looks for a wallpapers/ folder next to its exe, so people do drop
rem their collection in there. Take only our own files out; rmdir without /s
rem removes the folder itself just if nothing of the user's is left in it.
del "%APPDIR%\carasoul.exe" >nul 2>nul
del "%APPDIR%\uninstall.cmd" >nul 2>nul
rmdir "%APPDIR%" >nul 2>nul
del "%~f0" >nul 2>nul
