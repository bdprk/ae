@echo off
REM Auto-push the ae repository to GitHub (bdprk) — runs via the
REM `ae-autopush` scheduled task every 15 minutes. Push-only, never force;
REM a no-op when nothing is ahead. Conflicts are logged for manual
REM `git pull --rebase` (never auto-resolved).
setlocal
set REPO=D:\用户\trevv\桌面\projects-bdprk-副号\ae
set STATE=C:\Users\trevv\.local\state\git-autopush
if not exist "%STATE%" mkdir "%STATE%" >nul 2>&1
cd /d "%REPO%" || exit /b 1
for /f %%i in ('git rev-list --count origin/main..main 2^>nul') do set AHEAD=%%i
if "%AHEAD%"=="0" exit /b 0
echo [%date% %time%] ahead %AHEAD%, pushing >> "%STATE%\latest.log"
for /f %%h in ('git rev-parse --short HEAD') do set PUSHED=%%h
git push origin main >> "%STATE%\latest.log" 2>&1 && (
  echo [%date% %time%] pushed %PUSHED% >> "%STATE%\latest.log"
  exit /b 0
)
timeout /t 5 /nobreak >nul
git push origin main >> "%STATE%\latest.log" 2>&1 && exit /b 0
timeout /t 15 /nobreak >nul
git push origin main >> "%STATE%\latest.log" 2>&1 && exit /b 0
echo [%date% %time%] PUSH FAILED after 3 attempts; if non-fast-forward run: git pull --rebase >> "%STATE%\latest.log"
exit /b 1
