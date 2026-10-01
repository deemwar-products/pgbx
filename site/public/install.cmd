@echo off
rem pgbx installer for cmd.exe: runs install.ps1 with ExecutionPolicy Bypass, passing arguments through.
rem   curl -fsSL https://deemwar-products.github.io/pgbx/install.cmd -o install.cmd && install.cmd [-Version vX.Y.Z] [-NoSkill]
setlocal
set "PGBX_PS1=%TEMP%\pgbx-install-%RANDOM%.ps1"
curl -fsSL https://deemwar-products.github.io/pgbx/install.ps1 -o "%PGBX_PS1%" || (echo pgbx install: cannot download install.ps1 & exit /b 1)
powershell -NoProfile -ExecutionPolicy Bypass -File "%PGBX_PS1%" %*
set "RC=%ERRORLEVEL%"
del "%PGBX_PS1%" >nul 2>&1
exit /b %RC%
