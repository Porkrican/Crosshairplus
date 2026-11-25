@echo off
setlocal EnableExtensions EnableDelayedExpansion
chcp 65001 >nul
pushd "%~dp0" || exit /b 1

set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
if not exist "%VSWHERE%" set "VSWHERE=%ProgramFiles%\Microsoft Visual Studio\Installer\vswhere.exe"

set "VCVARS="
set "VSPATH="
if exist "%VSWHERE%" call :find_vs
if defined VSPATH if exist "!VSPATH!\VC\Auxiliary\Build\vcvars64.bat" set "VCVARS=!VSPATH!\VC\Auxiliary\Build\vcvars64.bat"

set "MODE=%~1"
if "%MODE%"=="" set "MODE=build"
set "SCENE_FLAG="
if not "%~2"=="" (
  if /i "%~2"=="--overwrite-scenes" (
    set "SCENE_FLAG=--overwrite-scenes"
  ) else (
    echo Unknown option "%~2". Use --overwrite-scenes.
    popd
    exit /b 2
  )
)

if /i "%MODE%"=="preflight" goto :preflight
if /i "%MODE%"=="check" goto :check
if /i "%MODE%"=="build" goto :build
if /i "%MODE%"=="test" goto :test
if /i "%MODE%"=="run" goto :run
if /i "%MODE%"=="clean" goto :clean
if /i "%MODE%"=="assets" goto :assets
echo Unknown mode "%MODE%". Use: preflight ^| assets [--overwrite-scenes] ^| check ^| build [--overwrite-scenes] ^| test ^| run ^| clean
popd
exit /b 2

:ensure_vc
if not defined VCVARS (
  echo [BUILD] Visual Studio C++ build tools were not found.
  echo         Install "Visual Studio Build Tools" with the VCTools workload, then rerun.
  exit /b 1
)
set "PATH=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer;!PATH!"
call "%VCVARS%" >nul
if errorlevel 1 (
  echo [BUILD] vcvars64.bat failed.
  exit /b 1
)
exit /b 0

:find_vs
for /f "usebackq delims=" %%I in (`"%VSWHERE%" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do set "VSPATH=%%I"
exit /b 0

:preflight
echo === Crosshair+ toolchain preflight ===
set "FAIL=0"
if defined VCVARS (
  echo [ok] vcvars64: !VCVARS!
  call :ensure_vc
  if errorlevel 1 set "FAIL=1"
) else (
  echo [FAIL] vcvars64 not found
  set "FAIL=1"
)
where cl >nul 2>nul
if errorlevel 1 (echo [FAIL] cl.exe not on PATH after vcvars64 & set "FAIL=1") else (echo [ok] cl.exe)
where link >nul 2>nul
if errorlevel 1 (echo [FAIL] link.exe not on PATH after vcvars64 & set "FAIL=1") else (echo [ok] link.exe)
where dumpbin >nul 2>nul
if errorlevel 1 (echo [FAIL] dumpbin.exe not on PATH after vcvars64 & set "FAIL=1") else (echo [ok] dumpbin.exe)
where cargo >nul 2>nul
if errorlevel 1 (echo [FAIL] cargo not on PATH & set "FAIL=1") else (for /f "delims=" %%V in ('cargo -V') do echo [ok] %%V)
where rustc >nul 2>nul
if errorlevel 1 (echo [FAIL] rustc not on PATH & set "FAIL=1") else (for /f "delims=" %%V in ('rustc -Vv ^| findstr /b "host:"') do echo [ok] %%V)
where rustup >nul 2>nul
if errorlevel 1 (
  echo [warn] rustup unavailable; cannot verify the installed target list
) else (
  for /f "delims=" %%V in ('rustup -V') do echo [ok] %%V
  rustup target list --installed | findstr /c:"x86_64-pc-windows-msvc" >nul
  if errorlevel 1 (echo [FAIL] x86_64-pc-windows-msvc target is not installed & set "FAIL=1") else (echo [ok] x86_64-pc-windows-msvc target)
)
where winget >nul 2>nul
if errorlevel 1 (echo [warn] winget unavailable) else (for /f "delims=" %%V in ('winget -v') do echo [ok] winget %%V)
reg query "HKLM\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}" /v pv >nul 2>nul
if errorlevel 1 (echo [warn] WebView2 runtime not found in HKLM) else (for /f "tokens=3" %%V in ('reg query "HKLM\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}" /v pv ^| findstr pv') do echo [ok] WebView2 %%V)
popd
if "!FAIL!"=="0" (exit /b 0) else (exit /b 1)

:check
call :ensure_vc || exit /b 1
cargo check --locked
set "RC=!ERRORLEVEL!"
popd
exit /b !RC!

:assets
call :ensure_vc || exit /b 1
cargo run --bin gen_assets --release --locked -- %SCENE_FLAG%
set "RC=!ERRORLEVEL!"
popd
exit /b !RC!

:build
call :ensure_vc || exit /b 1
cargo run --bin gen_assets --release --locked -- %SCENE_FLAG% || (echo [BUILD] asset generation failed & popd & exit /b 1)
cargo build --release --locked
set "RC=!ERRORLEVEL!"
popd
exit /b !RC!

:test
call :ensure_vc || exit /b 1
cargo test --release --locked
set "RC=!ERRORLEVEL!"
popd
exit /b !RC!

:run
call :ensure_vc || exit /b 1
cargo run --release --bin CrosshairPlus
set "RC=!ERRORLEVEL!"
popd
exit /b !RC!

:clean
call :ensure_vc || exit /b 1
cargo clean
set "RC=!ERRORLEVEL!"
popd
exit /b !RC!
