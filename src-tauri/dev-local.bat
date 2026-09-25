@echo off
setlocal
set "PATH=%PATH:C:\Program Files\coreutils\bin;=%"
set "PATH=%PATH:C:\Program Files\coreutils\bin=%"
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvarsall.bat" x64 >nul
if errorlevel 1 (echo VCVARSALL FAILED & exit /b 1)
cd /d "C:\Users\Amit Brahmbhatt\muse-desk\muse-desk"
if errorlevel 1 (echo CD FAILED & exit /b 1)
if not exist "src-tauri\.build-tmp" mkdir "src-tauri\.build-tmp"
set "TMP=C:\Users\Amit Brahmbhatt\muse-desk\muse-desk\src-tauri\.build-tmp"
set "TEMP=C:\Users\Amit Brahmbhatt\muse-desk\muse-desk\src-tauri\.build-tmp"
REM Local-CLI demo backend: must boot with NO Meta key.
set "META_API_KEY="
set "META_BASE_URL="
set "MUSED_BACKEND=local-cli"
REM tauri.conf devUrl is :1420 with no beforeDevCommand, so serve src/ here.
start /b python -m http.server 1420 --directory src
call npx tauri dev
