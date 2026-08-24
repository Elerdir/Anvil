@echo off
REM ========================================================================
REM  Anvil - dev rezim na Windows s Vulkan akceleraci.
REM
REM  Proc Vulkan a ne CUDA: o rychlosti u modelu vetsiho nez VRAM nerozhoduje
REM  backend, ale rozlozeni modelu (crates/anvil-infrastructure/src/ai/
REM  offload_plan.rs). Vulkan navic pokryva NVIDII, AMD i Intel jednim SDK.
REM
REM  Prerekvizity:
REM    * Visual Studio Build Tools (workload "Desktop development with C++")
REM    * Vulkan SDK - https://vulkan.lunarg.com/sdk/home (nastavi VULKAN_SDK)
REM
REM  Na macOS pouzij scripts/dev-metal.sh - tam se nic doinstalovavat nemusi.
REM ========================================================================

setlocal enabledelayedexpansion
cd /d "%~dp0\.."

echo === Anvil dev (Vulkan) ===
echo.

REM ------ Visual Studio ------
set "VCVARS="
for %%V in (18 17 16) do (
    for %%E in (Community Professional Enterprise BuildTools) do (
        if exist "%ProgramFiles%\Microsoft Visual Studio\%%V\%%E\VC\Auxiliary\Build\vcvars64.bat" (
            if not defined VCVARS set "VCVARS=%ProgramFiles%\Microsoft Visual Studio\%%V\%%E\VC\Auxiliary\Build\vcvars64.bat"
        )
        if exist "%ProgramFiles(x86)%\Microsoft Visual Studio\%%V\%%E\VC\Auxiliary\Build\vcvars64.bat" (
            if not defined VCVARS set "VCVARS=%ProgramFiles(x86)%\Microsoft Visual Studio\%%V\%%E\VC\Auxiliary\Build\vcvars64.bat"
        )
    )
)

if not defined VCVARS (
    echo [CHYBA] Visual Studio Build Tools nenalezeno.
    echo Nainstaluj s workloadem "Desktop development with C++":
    echo   https://visualstudio.microsoft.com/downloads/?q=build+tools
    pause
    exit /b 1
)
echo [OK] %VCVARS%
call "%VCVARS%" >nul

REM ------ CMake ------
REM Byva soucasti VS a casto neni v globalnim PATH - dohledame ho tam.
where cmake >nul 2>&1
if errorlevel 1 (
    for %%V in (18 17 16) do (
        for %%E in (Community Professional Enterprise BuildTools) do (
            set "CM=%ProgramFiles%\Microsoft Visual Studio\%%V\%%E\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin"
            if exist "!CM!\cmake.exe" (
                set "PATH=!CM!;!PATH!"
            )
        )
    )
)
where cmake >nul 2>&1
if errorlevel 1 (
    echo [CHYBA] cmake neni v PATH - llama.cpp se bez nej neprelozi.
    echo Je soucasti workloadu "Desktop development with C++".
    pause
    exit /b 1
)
echo [OK] CMake nalezen

REM ------ Vulkan SDK ------
if not defined VULKAN_SDK (
    echo [CHYBA] VULKAN_SDK neni nastaven - nainstaluj Vulkan SDK:
    echo   https://vulkan.lunarg.com/sdk/home
    echo Po instalaci otevri nove okno terminalu.
    pause
    exit /b 1
)
echo [OK] Vulkan SDK: %VULKAN_SDK%

REM ------ Ninja ------
REM MSBuild pada pri kompilaci Vulkan shaderu ("cannot find the batch label
REM VCEnd") - llama-cpp-sys-2 si pro Vulkan vypina TrackFileAccess a to rozbije
REM paralelni custom build kroky. Ninja tuhle davkovou masinerii nepouziva.
for %%V in (18 17 16) do (
    for %%E in (Community Professional Enterprise BuildTools) do (
        set "NJ=%ProgramFiles%\Microsoft Visual Studio\%%V\%%E\Common7\IDE\CommonExtensions\Microsoft\CMake\Ninja"
        if exist "!NJ!\ninja.exe" (
            if not defined CMAKE_GENERATOR (
                set "PATH=!NJ!;!PATH!"
                set "CMAKE_GENERATOR=Ninja"
            )
        )
    )
)
if defined CMAKE_GENERATOR (
    echo [OK] Ninja generator
) else (
    echo [VAROVANI] Ninja nenalezen - build Vulkan shaderu muze selhat na MSBuildu.
)

echo.
echo === Spoustim Tauri dev s --features engine-vulkan ===
echo Prvni build llama.cpp a Vulkan shaderu trva ~5 min, dalsi spousteni do 1 min.
echo.

call pnpm tauri dev -- --features engine-vulkan
endlocal
