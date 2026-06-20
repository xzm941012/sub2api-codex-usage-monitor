# AGENTS.md

This repository is a native Windows Rust app. It renders a taskbar widget, tray icons, and optional floating/detail popups for Claude Code and Codex usage. Most behavior lives in Win32 drawing and window-management code, so small UI changes can have side effects.

## Project Shape

- `src/main.rs`: startup, `--diagnose`, CLI updater entrypoint, then hands off to `window::run()`.
- `src/window.rs`: main application state, Win32 window procedures, painting, taskbar embedding, floating mode, detail popup behavior, settings persistence.
- `src/poller.rs`: credential discovery, network polling, Claude/Codex/Sub2API usage fetching, token refresh helpers.
- `src/tray_icon.rs`: tray icon drawing, tooltip text, badge updates.
- `src/native_interop.rs`: low-level Win32 helpers for embedding, floating, moving, and locating taskbar/tray windows.
- `src/models.rs`: shared usage data structs.
- `src/localization/`: localized strings.
- `src/updater.rs`: GitHub/WinGet update flows.
- `build.rs`: embeds the icon and PE version metadata into the final exe.

## Build And Run

Do not assume the default Rust install on this machine works. The default `C:\Users\82462\.rustup` / `C:\Users\82462\.cargo\bin` setup may be incomplete and can fail with `no active toolchain` or `Missing manifest`.

Use the D-drive Rust environment:

```powershell
$env:RUSTUP_HOME='D:\ai\rust\rustup'
$env:CARGO_HOME='D:\ai\rust\cargo'
cargo +stable-x86_64-pc-windows-msvc check
cargo +stable-x86_64-pc-windows-msvc build --release
```

Direct toolchain path also works:

```powershell
D:\ai\rust\rustup\toolchains\stable-x86_64-pc-windows-msvc\bin\cargo.exe build --release
```

If release build fails with `failed to remove ... claude-code-usage-monitor.exe`, stop the running app first:

```powershell
Get-Process claude-code-usage-monitor | Stop-Process
```

The release binary is:

```text
D:\ai\Claude-Code-Usage-Monitor\target\release\claude-code-usage-monitor.exe
```

## Runtime Files

- Settings: `%APPDATA%\ClaudeCodeUsageMonitor\settings.json`
- Diagnose log: `%TEMP%\claude-code-usage-monitor.log`
- The app icon is embedded into the exe by `build.rs`; there is no external runtime asset bundle.

If moving the app to another machine and you want to preserve behavior, copy both the exe and `settings.json`.

## Sub2API Notes

This repo has a custom Sub2API-backed Codex mode in addition to the original direct Codex polling.

Important config fields in `settings.json`:

- `usage_mode`
- `sub2api.base_url`
- `sub2api.admin_api_key`
- `sub2api.group_id`
- `sub2api.group_name`
- `sub2api.platform`
- `sub2api.account_type`
- `sub2api.force_active_usage`

Sub2API group list is fetched dynamically. Group switching and usage polling are sensitive to account filtering and can affect what appears in the widget.

## UI And Win32 Guardrails

- Fixed taskbar mode and floating mode behave differently on purpose. Do not assume a layout fix in one mode is safe in the other.
- Main widget, tray icon, and detail popup all derive from shared state, but they are not interchangeable surfaces.
- Be especially careful when changing:
  - taskbar embedding vs topmost popup transitions
  - layered window rendering and transparent-key handling
  - detail popup ownership and outside-click behavior
  - width/height calculations tied to `detail_visible`
- If fixed-mode detail behavior is being changed, prefer an independent popup over resizing or detaching the embedded taskbar widget.

## Editing Guidance

- Keep changes narrow. `window.rs` is large and stateful; read the relevant branch carefully before editing.
- Follow the existing style. This project is mostly straightforward Rust plus direct Win32 API calls, not heavy abstraction.
- Use `apply_patch` for edits.
- After any UI or state-flow change, run at least:

```powershell
cargo +stable-x86_64-pc-windows-msvc fmt
cargo +stable-x86_64-pc-windows-msvc check
```

- If you changed runtime behavior, rebuild release and restart the exe to verify the actual shipped binary.

## Verification Checklist

When changing widget or popup behavior, verify:

- startup still works
- tray icon still appears
- fixed mode stays embedded in taskbar when expected
- floating mode still drags and snaps correctly
- detail popup opens/closes correctly
- settings persist across restart
- Sub2API mode still loads usage and groups

