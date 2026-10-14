@echo off
chcp 65001 >nul
cd /d "%~dp0"
title 构建 抖音关注清理助手

set "TC=%USERPROFILE%\.rustup\toolchains\stable-x86_64-pc-windows-gnu"
if not exist "%TC%" (
  echo.
  echo   [错误] 未找到 Rust 工具链（GNU 目标）
  echo   请先安装： https://rustup.rs
  echo   安装时选择 x86_64-pc-windows-gnu 目标即可，无需安装 Visual Studio。
  echo.
  pause
  exit /b 1
)

rem GNU 目标需要 binutils（提供 dlltool / as）。rustup 自带的可能不完整。
set "BINUTILS=%USERPROFILE%\.rustup\binutils"
if exist "%BINUTILS%" (
  set "PATH=%BINUTILS%;%PATH%"
) else (
  echo   提示：未找到 %BINUTILS%
  echo   如果编译时报 dlltool 相关错误，请从 MSYS2 仓库下载 mingw-w64-x86_64-binutils，
  echo   解压后把 mingw64\bin 里的内容放到 %BINUTILS%
  echo.
)

set "PATH=%TC%\bin;%PATH%"
set "CARGO_HOME=%USERPROFILE%\.cargo"

echo   正在编译（release，首次约需 10 分钟）...
cargo build --release
if errorlevel 1 (
  echo.
  echo   [错误] 编译失败
  pause
  exit /b 1
)

rem WebView2Loader.dll 必须与 exe 放在同一目录
for /d %%d in ("target\release\build\webview2-com-sys-*") do (
  if exist "%%d\out\x64\WebView2Loader.dll" (
    copy /Y "%%d\out\x64\WebView2Loader.dll" "target\release\" >nul
  )
)

echo.
echo   构建完成！产物在 target\release\
echo     douyin-follow-cleaner.exe   （主程序，双击运行）
echo     WebView2Loader.dll          （WebView2 加载器，必须与 exe 放在一起）
echo.
pause
