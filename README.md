# Sub2API Codex Usage Monitor

![Windows](https://img.shields.io/badge/platform-Windows-blue)
![Rust](https://img.shields.io/badge/language-Rust-orange)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

A lightweight Windows taskbar widget for monitoring Codex usage, with native support for fetching usage through Sub2API.

This fork keeps the original native Windows widget idea, and extends it for Sub2API-backed Codex usage display, group switching, floating/fixed placement, and richer account detail views.

## Fork Source

This repository is a forked and modified derivative of:

- [CodeZeno/Claude-Code-Usage-Monitor](https://github.com/CodeZeno/Claude-Code-Usage-Monitor)

The original project focused on Claude Code usage with optional Codex support. This fork is adapted for Sub2API-driven Codex usage monitoring on Windows.

## What This Fork Adds

- Sub2API-backed Codex usage mode
- Group-based account switching from the right-click menu
- Average usage aggregation across accounts in a selected group
- Reset time display based on the nearest relevant account reset
- Account detail popup with per-account 5h / 7d usage
- Fixed and floating widget placement modes
- Light and dark widget themes
- Simplified Chinese UI support

## Main Features

- Native Windows taskbar widget
- 5h and 7d usage bars
- Countdown to reset
- System tray icon badges
- Right-click refresh, placement, theme, color mode, group switching, update frequency, startup, language, and exit
- Optional floating mode with draggable placement

## Requirements

- Windows 10 or Windows 11
- Rust only if you want to build from source
- A working Sub2API instance if you want to use Sub2API mode
- Optional: Codex local auth if you also use direct Codex mode

## Configuration

Settings are stored in:

```text
%APPDATA%\ClaudeCodeUsageMonitor\settings.json
```

Sub2API mode uses fields like:

```json
{
  "usage_mode": "sub2api",
  "sub2api": {
    "base_url": "https://your-sub2api-host",
    "admin_api_key": "YOUR_ADMIN_KEY",
    "group_id": 123,
    "group_name": "openai-plus",
    "platform": "openai",
    "account_type": "oauth",
    "force_active_usage": true
  }
}
```

Notes:

- `group_id` can be filled automatically after switching groups in the widget menu.
- Do not commit real API keys, tokens, or local settings files.
- Do not hardcode production secrets in source files.

## Build From Source

This is a pure Rust Windows application.

Standard command:

```powershell
cargo build --release
```

On this development machine, the default Rust environment may be broken, so the working build flow is:

```powershell
$env:RUSTUP_HOME='D:\ai\rust\rustup'
$env:CARGO_HOME='D:\ai\rust\cargo'
cargo +stable-x86_64-pc-windows-msvc build --release
```

Release output:

```text
target\release\claude-code-usage-monitor.exe
```

If the build cannot overwrite the release exe, stop the running app first:

```powershell
Get-Process claude-code-usage-monitor | Stop-Process
```

## Run

After building, launch:

```powershell
.\target\release\claude-code-usage-monitor.exe
```

The widget reads and writes its settings through `%APPDATA%`, not beside the exe.

## Diagnostics

Run with:

```powershell
claude-code-usage-monitor --diagnose
```

Log file:

```text
%TEMP%\claude-code-usage-monitor.log
```

## Privacy And Security

This project can access authenticated usage information and should be handled carefully.

Please keep these rules:

- Never commit `%APPDATA%\ClaudeCodeUsageMonitor\settings.json`
- Never commit real `admin_api_key` values
- Never commit local auth files such as `.codex/auth.json` or `.claude/.credentials.json`
- Never publish logs that contain sensitive headers, tokens, or private endpoints

This repository should contain only source code and safe documentation.

## Git Hygiene

Before pushing:

- verify `.gitignore` excludes local artifacts
- review `git status`
- scan staged changes for keys, tokens, private hostnames, and local config

Recommended quick checks:

```powershell
git status
git diff --staged
rg -n "admin-[a-f0-9]{20,}|sk-[A-Za-z0-9_-]{10,}|api[_-]?key|bearer" .
```

## License

This project remains under the MIT license inherited from the upstream project unless changed explicitly by repository owners.
