@echo off
REM ========================================================================
REM  Anvil - spusteni aplikace bez instalatoru.
REM
REM  Postavi release binarku s vestavenym frontendem a spusti ji. Dev server
REM  neni potreba, takze se to chova jako hotova aplikace.
REM
REM  Prvni spusteni trva ~10 minut (llama.cpp a Vulkan shadery), dalsi
REM  jednotky sekund - cargo i vite stavi jen to, co se zmenilo.
REM
REM  Pouziti:
REM    run.bat            spusti (a v pripade potreby postavi)
REM    run.bat --rebuild  vynuti cisty build
REM
REM  Na macOS pouzij scripts/run-mac.sh.
REM ========================================================================

setlocal enabledelayedexpansion
cd /d "%~dp0"

set "EXE=target\release\anvil.exe"

echo === Anvil ===
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
call "%VCVARS%" >nul

REM ------ CMake ------
REM Byva soucasti VS a casto neni v globalnim PATH.
where cmake >nul 2>&1
if errorlevel 1 (
    for %%V in (18 17 16) do (
        for %%E in (Community Professional Enterprise BuildTools) do (
            set "CM=%ProgramFiles%\Microsoft Visual Studio\%%V\%%E\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin"
            if exist "!CM!\cmake.exe" set "PATH=!CM!;!PATH!"
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

REM ------ Vulkan SDK ------
if not defined VULKAN_SDK (
    echo [CHYBA] VULKAN_SDK neni nastaven. Nainstaluj Vulkan SDK:
    echo   https://vulkan.lunarg.com/sdk/home
    echo Po instalaci otevri nove okno terminalu.
    pause
    exit /b 1
)

REM ------ Ninja ------
REM MSBuild pada pri kompilaci Vulkan shaderu ("cannot find the batch label
REM VCEnd"); Ninja tuhle davkovou masinerii nepouziva.
for %%V in (18 17 16) do (
    for %%E in (Community Professional Enterprise BuildTools) do (
        set "NJ=%ProgramFiles%\Microsoft Visual Studio\%%V\%%E\Common7\IDE\CommonExtensions\Microsoft\CMake\Ninja"
        if exist "!NJ!\ninja.exe" if not defined CMAKE_GENERATOR (
            set "PATH=!NJ!;!PATH!"
            set "CMAKE_GENERATOR=Ninja"
        )
    )
)

REM ------ Node a pnpm ------
where pnpm >nul 2>&1
if errorlevel 1 (
    echo [CHYBA] pnpm neni v PATH. Nainstaluj ho:
    echo   npm install -g pnpm
    pause
    exit /b 1
)

REM ------ Bezici instance ------
REM Linker neprepise zamcenou binarku a cargo hlasi jen "Pristup byl odepren
REM (os error 5)", z ceho se pricina nepozna. Radsi to zjistime tady.
tasklist /FI "IMAGENAME eq anvil.exe" 2>nul | find /I "anvil.exe" >nul
if not errorlevel 1 (
    echo Anvil uz bezi a drzi zamek na binarce - build by selhal.
    choice /C AN /N /M "Ukoncit bezici instanci? [A]no / [N]e: "
    if errorlevel 2 (
        echo Preruseno. Zavri Anvil a spust run.bat znovu.
        pause
        exit /b 1
    )
    taskkill /F /IM anvil.exe >nul 2>&1
    REM Windows uvolnuje popisovace souboru se zpozdenim; bez cekani by
    REM linker porad narazil na zamek.
    ping -n 3 127.0.0.1 >nul
)

REM ------ Cisty build na vyzadani ------
if /I "%~1"=="--rebuild" (
    echo Vynuceny cisty build...
    if exist "%EXE%" del /q "%EXE%"
    cargo clean -p anvil >nul 2>&1
)

if not exist "node_modules" (
    echo Instaluji zavislosti frontendu...
    call pnpm install
    if errorlevel 1 (
        echo [CHYBA] pnpm install selhal.
        pause
        exit /b 1
    )
)

REM ------ Build ------
REM Stavi se vzdycky. Kdyz se nic nezmenilo, je to otazka sekund, a je to
REM levnejsi nez riskovat, ze pojede stara binarka.
if not exist "%EXE%" (
    echo Prvni build - llama.cpp a Vulkan shadery, pocitej s ~10 minutami.
) else (
    echo Kontroluji zmeny...
)
echo.

call pnpm tauri build --no-bundle -- --features engine-vulkan
if errorlevel 1 (
    echo.
    echo [CHYBA] Build selhal.
    pause
    exit /b 1
)

if not exist "%EXE%" (
    echo [CHYBA] Binarka %EXE% nevznikla.
    pause
    exit /b 1
)

echo.
echo Spoustim %EXE%
start "" "%EXE%"

endlocal
