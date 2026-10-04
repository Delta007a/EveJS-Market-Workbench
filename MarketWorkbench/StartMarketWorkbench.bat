@echo off
setlocal
cd /d "%~dp0"
if errorlevel 1 goto missing
if not exist "market-workbench.exe" goto missing
set "WB_INSTALL_ROOT=%~dp0..\.."
if not "%~1"=="" set "WB_INSTALL_ROOT=%~1"
if not exist "%WB_INSTALL_ROOT%\StartMarketServer.bat" (
  echo Extract the MarketWorkbench folder into the tools folder of your EveJS installation.
  echo Expected: EveJS\tools\MarketWorkbench\StartMarketWorkbench.bat
  echo See README.md in this folder. You can also supply the EveJS root as an argument.
  pause
  exit /b 1
)
set "WB_PORT=8765"
if not "%MARKET_WORKBENCH_PORT%"=="" set "WB_PORT=%MARKET_WORKBENCH_PORT%"
echo Starting Market Workbench. Please wait.
echo Wait for the backend line: Market Workbench is ready: http://127.0.0.1:%WB_PORT%/
echo Open that address after the ready line appears.
echo Your presets and market databases: %CD%\user-data
echo Press Ctrl+C here to stop Workbench. EveJS services are not controlled.
"market-workbench.exe" workbench --port "%WB_PORT%" --storage-dir "%CD%\user-data" --install-root "%WB_INSTALL_ROOT%"
set "WB_EXIT=%ERRORLEVEL%"
if not "%WB_EXIT%"=="0" (
  echo.
  echo Workbench exited with code %WB_EXIT%. See the error above.
  echo If the port is in use, close the previous Workbench or set MARKET_WORKBENCH_PORT.
  pause
)
exit /b %WB_EXIT%
:missing
echo Missing market-workbench.exe. Extract the entire portable ZIP, or follow BUILD.md.
pause
exit /b 1
