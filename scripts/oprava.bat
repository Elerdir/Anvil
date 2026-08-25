@echo off
REM Zmeri, jestli model dokaze opravit zname chyby pres edit_file.
REM   scripts\oprava.bat D:\models\model.gguf fixtures\vadny-projekt
setlocal enabledelayedexpansion
cd /d "%~dp0\.."
set "VCVARS="
for %%V in (18 17 16) do (
    for %%E in (Community Professional Enterprise BuildTools) do (
        if exist "%ProgramFiles%\Microsoft Visual Studio\%%V\%%E\VC\Auxiliary\Build\vcvars64.bat" (
            if not defined VCVARS set "VCVARS=%ProgramFiles%\Microsoft Visual Studio\%%V\%%E\VC\Auxiliary\Build\vcvars64.bat"
        )
    )
)
if not defined VCVARS ( echo [CHYBA] VS Build Tools nenalezeno & exit /b 1 )
call "%VCVARS%" >nul
where cmake >nul 2>&1
if errorlevel 1 (
    for %%V in (18 17 16) do (
        for %%E in (Community Professional Enterprise BuildTools) do (
            set "CM=%ProgramFiles%\Microsoft Visual Studio\%%V\%%E\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin"
            if exist "!CM!\cmake.exe" set "PATH=!CM!;!PATH!"
        )
    )
)
for %%V in (18 17 16) do (
    for %%E in (Community Professional Enterprise BuildTools) do (
        set "NJ=%ProgramFiles%\Microsoft Visual Studio\%%V\%%E\Common7\IDE\CommonExtensions\Microsoft\CMake\Ninja"
        if exist "!NJ!\ninja.exe" if not defined CMAKE_GENERATOR (
            set "PATH=!NJ!;!PATH!"
            set "CMAKE_GENERATOR=Ninja"
        )
    )
)
if not defined VULKAN_SDK ( echo [CHYBA] VULKAN_SDK neni nastaven & exit /b 1 )
cargo run --release --example oprava -p anvil-infrastructure --features engine-vulkan -- %*
set RC=%ERRORLEVEL%
endlocal & exit /b %RC%
