use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
use windows::Win32::System::Registry::*;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Accessibility::HWINEVENTHOOK;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
    VK_LBUTTON, VK_RBUTTON,
};
use windows::Win32::UI::Shell::ExtractIconExW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::diagnose;
use crate::localization::{self, LanguageId, Strings};
use crate::models::{AccountUsageDetail, AppUsageData, UsageData, UsageSection};
use crate::native_interop::{
    self, Color, TIMER_COUNTDOWN, TIMER_EDGE_ANIMATION, TIMER_PET_RENDER, TIMER_POLL,
    TIMER_RESET_POLL, TIMER_UPDATE_CHECK, WM_APP_TRAY, WM_APP_USAGE_UPDATED,
};
use crate::poller;
use crate::theme;
use crate::tray_icon;
use crate::updater::{self, InstallChannel, ReleaseDescriptor, UpdateCheckResult};
use crate::usage_summary::{self, UsageSummary, UsageSummarySection};

/// Wrapper to make HWND sendable across threads (safe for PostMessage usage)
#[derive(Clone, Copy)]
struct SendHwnd(isize);

unsafe impl Send for SendHwnd {}

impl SendHwnd {
    fn from_hwnd(hwnd: HWND) -> Self {
        Self(hwnd.0 as isize)
    }
    fn to_hwnd(self) -> HWND {
        HWND(self.0 as *mut _)
    }
}

/// Shared application state
struct AppState {
    hwnd: SendHwnd,
    detail_hwnd: Option<SendHwnd>,
    taskbar_hwnd: Option<HWND>,
    tray_notify_hwnd: Option<HWND>,
    win_event_hook: Option<HWINEVENTHOOK>,
    is_dark: bool,
    embedded: bool,
    language_override: Option<LanguageId>,
    language: LanguageId,
    install_channel: InstallChannel,

    session_percent: f64,
    session_text: String,
    weekly_percent: f64,
    weekly_text: String,
    codex_session_percent: f64,
    codex_session_text: String,
    codex_weekly_percent: f64,
    codex_weekly_text: String,
    show_claude_code: bool,
    show_codex: bool,
    placement_mode: PlacementMode,
    widget_theme: WidgetTheme,
    season: crate::pixel_room::Season,
    progress_color_mode: ProgressColorMode,
    floating_x: Option<i32>,
    floating_y: Option<i32>,
    edge_collapse: EdgeCollapse,
    edge_expanded: bool,
    mouse_tracking: bool,
    edge_animation: Option<EdgeAnimation>,
    detail_visible: bool,
    detail_closing: bool,
    detail_mouse_down_seen: bool,
    pet_animation_started: Instant,
    pet_action_seed: u64,
    pet_motion_enabled: bool,
    pet_transition: Option<PetTransition>,
    sub2api_groups: Vec<poller::Sub2apiGroup>,
    sub2api_groups_loading: bool,
    sub2api_groups_loaded: bool,

    data: Option<AppUsageData>,

    poll_interval_ms: u32,
    retry_count: u32,
    force_notify_auth_error: bool,
    auth_error_paused_polling: bool,
    auth_watch_mode: poller::CredentialWatchMode,
    auth_watch_snapshot: poller::CredentialWatchSnapshot,
    last_poll_ok: bool,
    update_status: UpdateStatus,
    last_update_check_unix: Option<u64>,

    tray_offset: i32,
    dragging: bool,
    drag_start_mouse_x: i32,
    drag_start_mouse_y: i32,
    drag_start_offset: i32,
    drag_start_floating_x: i32,
    drag_start_floating_y: i32,

    widget_visible: bool,
}

#[derive(Clone, Copy, Debug)]
struct EdgeAnimation {
    from_x: i32,
    to_x: i32,
    y: i32,
    height: i32,
    start: Instant,
    duration: Duration,
}

#[derive(Clone, Copy, Debug)]
struct PetTransition {
    opening: bool,
    start: Instant,
}

#[derive(Clone, Debug)]
enum UpdateStatus {
    Idle,
    Checking,
    Applying,
    UpToDate,
    Available(ReleaseDescriptor),
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum PlacementMode {
    Fixed,
    Floating,
}

impl Default for PlacementMode {
    fn default() -> Self {
        Self::Fixed
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum WidgetTheme {
    Light,
    Dark,
}

impl Default for WidgetTheme {
    fn default() -> Self {
        Self::Dark
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ProgressColorMode {
    Mono,
    Antd,
}

impl Default for ProgressColorMode {
    fn default() -> Self {
        Self::Mono
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum EdgeCollapse {
    None,
    Left,
    Right,
}

impl Default for EdgeCollapse {
    fn default() -> Self {
        Self::None
    }
}

const RETRY_BASE_MS: u32 = 30_000; // 30 seconds

const POLL_10_SEC: u32 = 10_000;
const POLL_30_SEC: u32 = 30_000;
const POLL_1_MIN: u32 = 60_000;
const POLL_5_MIN: u32 = 300_000;
const POLL_15_MIN: u32 = 900_000;
const POLL_1_HOUR: u32 = 3_600_000;

// Menu item IDs for update frequency
const IDM_FREQ_1MIN: u16 = 10;
const IDM_FREQ_5MIN: u16 = 11;
const IDM_FREQ_15MIN: u16 = 12;
const IDM_FREQ_1HOUR: u16 = 13;
const IDM_FREQ_10SEC: u16 = 14;
const IDM_FREQ_30SEC: u16 = 15;
const IDM_START_WITH_WINDOWS: u16 = 20;
const IDM_RESET_POSITION: u16 = 30;
const IDM_VERSION_ACTION: u16 = 31;
const IDM_PLACEMENT_FIXED: u16 = 70;
const IDM_PLACEMENT_FLOATING: u16 = 71;
const IDM_THEME_LIGHT: u16 = 72;
const IDM_THEME_DARK: u16 = 73;
const IDM_COLOR_MONO: u16 = 74;
const IDM_COLOR_ANTD: u16 = 75;
const IDM_SEASON_AUTO: u16 = 80;
const IDM_SEASON_WINTER: u16 = 84;
const IDM_LANG_SYSTEM: u16 = 100;
const IDM_LANG_ENGLISH: u16 = 101;
const IDM_LANG_DUTCH: u16 = 102;
const IDM_LANG_SPANISH: u16 = 103;
const IDM_LANG_FRENCH: u16 = 104;
const IDM_LANG_GERMAN: u16 = 105;
const IDM_LANG_JAPANESE: u16 = 106;
const IDM_LANG_KOREAN: u16 = 107;
const IDM_LANG_SIMPLIFIED_CHINESE: u16 = 108;
const IDM_LANG_TRADITIONAL_CHINESE: u16 = 109;
const IDM_LANG_RUSSIAN: u16 = 110;
const IDM_LANG_PORTUGUESE_BRAZIL: u16 = 111;
const IDM_MODEL_CLAUDE_CODE: u16 = 60;
const IDM_MODEL_CODEX: u16 = 61;
const IDM_GROUP_REFRESH: u16 = 120;
const IDM_GROUP_BASE: u16 = 1000;
const IDM_GROUP_MAX: u16 = 1199;

const DIVIDER_HIT_ZONE: i32 = 13; // LEFT_DIVIDER_W + DIVIDER_RIGHT_MARGIN

const WM_DPICHANGED_MSG: u32 = 0x02E0;
const WM_MOUSELEAVE_MSG: u32 = 0x02A3;
const DETAIL_ROW_HEIGHT: i32 = 90;
const DETAIL_PANEL_WIDTH: i32 = 480;
const DETAIL_PANEL_PADDING: i32 = 6;
const DETAIL_PANEL_MAX_ROWS: usize = 6;
const DETAIL_PANEL_MAX_ACCOUNT_ROWS: usize = DETAIL_PANEL_MAX_ROWS - 1;
const TIMER_DETAIL_OUTSIDE: usize = 6;
const TIMER_DETAIL_RENDER: usize = 7;
const WM_APP_UPDATE_CHECK_COMPLETE: u32 = WM_APP + 2;
const WM_APP_SUB2API_GROUPS_UPDATED: u32 = WM_APP + 4;
const TRAY_ICON_UPDATE_REPOSITION_SUPPRESS_MS: u64 = 750;

static SUPPRESS_TRAY_REPOSITION_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);

/// Current system DPI (96 = 100% scaling, 144 = 150%, 192 = 200%, etc.)
static CURRENT_DPI: AtomicU32 = AtomicU32::new(96);

/// Scale a base pixel value (designed at 96 DPI) to the current DPI.
fn sc(px: i32) -> i32 {
    let dpi = CURRENT_DPI.load(Ordering::Relaxed);
    (px as f64 * dpi as f64 / 96.0).round() as i32
}

/// Re-query the monitor DPI for our window and update the cached value.
/// Uses GetDpiForWindow which returns the live DPI (unlike GetDpiForSystem
/// which is cached at process startup and never changes).
fn refresh_dpi() {
    let hwnd = {
        let state = lock_state();
        state.as_ref().map(|s| s.hwnd.to_hwnd())
    };
    if let Some(hwnd) = hwnd {
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        if dpi > 0 {
            CURRENT_DPI.store(dpi, Ordering::Relaxed);
        }
    }
}

fn load_embedded_app_icons() -> (HICON, HICON) {
    unsafe {
        let mut exe_buf = [0u16; 260];
        let len = GetModuleFileNameW(None, &mut exe_buf) as usize;
        if len == 0 {
            return (HICON::default(), HICON::default());
        }

        let mut large_icon = HICON::default();
        let mut small_icon = HICON::default();
        let extracted = ExtractIconExW(
            PCWSTR::from_raw(exe_buf.as_ptr()),
            0,
            Some(&mut large_icon),
            Some(&mut small_icon),
            1,
        );

        if extracted == 0 {
            (HICON::default(), HICON::default())
        } else {
            (large_icon, small_icon)
        }
    }
}

unsafe impl Send for AppState {}

static STATE: Mutex<Option<AppState>> = Mutex::new(None);

/// Lock STATE safely, recovering from poisoned mutex
fn lock_state() -> MutexGuard<'static, Option<AppState>> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn settings_path() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(appdata)
        .join("ClaudeCodeUsageMonitor")
        .join("settings.json")
}

#[derive(Debug, Serialize, Deserialize)]
struct SettingsFile {
    #[serde(default)]
    tray_offset: i32,
    #[serde(default = "default_poll_interval")]
    poll_interval_ms: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_update_check_unix: Option<u64>,
    #[serde(default = "default_widget_visible")]
    widget_visible: bool,
    #[serde(default = "default_show_claude_code")]
    show_claude_code: bool,
    #[serde(default = "default_show_codex")]
    show_codex: bool,
    #[serde(default)]
    placement_mode: PlacementMode,
    #[serde(default)]
    widget_theme: WidgetTheme,
    #[serde(default)]
    season: crate::pixel_room::Season,
    #[serde(default)]
    progress_color_mode: ProgressColorMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    floating_x: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    floating_y: Option<i32>,
    #[serde(default)]
    edge_collapse: EdgeCollapse,
    #[serde(default)]
    usage_mode: poller::UsageMode,
    #[serde(default)]
    sub2api: poller::Sub2apiConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cached_usage: Option<CachedUsageFile>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CachedUsageFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    codex_session_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    codex_session_resets_at_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    codex_weekly_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    codex_weekly_resets_at_unix: Option<u64>,
}

impl Default for SettingsFile {
    fn default() -> Self {
        Self {
            tray_offset: 0,
            poll_interval_ms: default_poll_interval(),
            language: None,
            last_update_check_unix: None,
            widget_visible: true,
            show_claude_code: false,
            show_codex: true,
            placement_mode: PlacementMode::Fixed,
            widget_theme: WidgetTheme::Dark,
            season: crate::pixel_room::Season::Auto,
            progress_color_mode: ProgressColorMode::Mono,
            floating_x: None,
            floating_y: None,
            edge_collapse: EdgeCollapse::None,
            usage_mode: poller::UsageMode::Codex,
            sub2api: poller::Sub2apiConfig::default(),
            cached_usage: None,
        }
    }
}

fn default_poll_interval() -> u32 {
    POLL_15_MIN
}

fn default_widget_visible() -> bool {
    true
}

fn default_show_claude_code() -> bool {
    false
}

fn default_show_codex() -> bool {
    true
}

fn load_settings() -> SettingsFile {
    let content = match std::fs::read_to_string(settings_path()) {
        Ok(c) => c,
        Err(_) => return SettingsFile::default(),
    };
    let mut settings: SettingsFile = serde_json::from_str(&content).unwrap_or_default();
    if !settings.show_claude_code && !settings.show_codex {
        settings.show_codex = true;
    }
    settings.show_claude_code = false;
    settings.show_codex = true;
    settings
}

fn save_settings(settings: &SettingsFile) {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(settings) {
        let _ = std::fs::write(path, json);
    }
}

fn save_state_settings() {
    let state = lock_state();
    if let Some(s) = state.as_ref() {
        let existing = load_settings();
        save_settings(&SettingsFile {
            tray_offset: s.tray_offset,
            poll_interval_ms: s.poll_interval_ms,
            language: s
                .language_override
                .map(|language| language.code().to_string()),
            last_update_check_unix: s.last_update_check_unix,
            widget_visible: s.widget_visible,
            show_claude_code: s.show_claude_code,
            show_codex: s.show_codex,
            placement_mode: s.placement_mode,
            widget_theme: s.widget_theme,
            season: s.season,
            progress_color_mode: s.progress_color_mode,
            floating_x: s.floating_x,
            floating_y: s.floating_y,
            edge_collapse: s.edge_collapse,
            usage_mode: existing.usage_mode,
            sub2api: existing.sub2api,
            cached_usage: cached_usage_from_state(s).or(existing.cached_usage),
        });
    }
}

fn cached_usage_from_state(state: &AppState) -> Option<CachedUsageFile> {
    if !state.last_poll_ok {
        return None;
    }
    let codex = state.data.as_ref()?.codex.as_ref()?;
    Some(CachedUsageFile {
        codex_session_percent: Some(codex.session.percentage),
        codex_session_resets_at_unix: codex.session.resets_at.and_then(system_time_to_unix_secs),
        codex_weekly_percent: Some(codex.weekly.percentage),
        codex_weekly_resets_at_unix: codex.weekly.resets_at.and_then(system_time_to_unix_secs),
    })
}

fn app_usage_from_cached(settings: &SettingsFile) -> Option<AppUsageData> {
    let cached = settings.cached_usage.as_ref()?;
    let session_percent = cached.codex_session_percent?;
    let weekly_percent = cached.codex_weekly_percent?;
    Some(AppUsageData {
        claude_code: None,
        codex: Some(UsageData {
            session: UsageSection {
                percentage: session_percent,
                resets_at: cached
                    .codex_session_resets_at_unix
                    .map(unix_secs_to_system_time),
            },
            weekly: UsageSection {
                percentage: weekly_percent,
                resets_at: cached
                    .codex_weekly_resets_at_unix
                    .map(unix_secs_to_system_time),
            },
            account_details: Vec::new(),
        }),
    })
}

fn system_time_to_unix_secs(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
}

fn unix_secs_to_system_time(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

fn tray_icon_data_from_state() -> Vec<tray_icon::TrayIconData> {
    let state = lock_state();
    match state.as_ref() {
        Some(s) if s.last_poll_ok => {
            let mut icons = Vec::new();
            if s.show_claude_code {
                icons.push(tray_icon::TrayIconData {
                    kind: tray_icon::TrayIconKind::Claude,
                    percent: Some(s.session_percent),
                    tooltip: format!(
                        "{} 5h: {} | 7d: {}",
                        s.language.strings().claude_code_model,
                        s.session_text,
                        s.weekly_text
                    ),
                });
            }
            if s.show_codex {
                icons.push(tray_icon::TrayIconData {
                    kind: tray_icon::TrayIconKind::Codex,
                    percent: Some(s.codex_session_percent),
                    tooltip: format!(
                        "{} 5h: {} | 7d: {}",
                        s.language.strings().codex_model,
                        s.codex_session_text,
                        s.codex_weekly_text
                    ),
                });
            }
            icons
        }
        Some(s) => {
            let mut icons = Vec::new();
            if s.show_claude_code {
                icons.push(tray_icon::TrayIconData {
                    kind: tray_icon::TrayIconKind::Claude,
                    percent: None,
                    tooltip: s.language.strings().window_title.to_string(),
                });
            }
            if s.show_codex {
                icons.push(tray_icon::TrayIconData {
                    kind: tray_icon::TrayIconKind::Codex,
                    percent: None,
                    tooltip: s.language.strings().codex_window_title.to_string(),
                });
            }
            icons
        }
        None => Vec::new(),
    }
}

fn sync_tray_icons(hwnd: HWND) {
    let icons = tray_icon_data_from_state();
    tray_icon::sync(hwnd, &icons);
}

fn ensure_floating_topmost(hwnd: HWND) {
    if !native_interop::ensure_topmost(hwnd, false) {
        diagnose::log("unable to keep floating widget topmost");
    }
}

fn toggle_widget_visibility(hwnd: HWND) {
    let (new_visible, placement_mode) = {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            s.widget_visible = !s.widget_visible;
            (s.widget_visible, s.placement_mode)
        } else {
            return;
        }
    };
    save_state_settings();
    unsafe {
        if new_visible {
            position_at_taskbar();
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            if placement_mode == PlacementMode::Floating {
                ensure_floating_topmost(hwnd);
            }
            render_layered();
        } else {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn update_check_interval() -> Duration {
    Duration::from_secs(24 * 60 * 60)
}

fn auto_update_check_due(last_update_check_unix: Option<u64>) -> bool {
    let Some(last_update_check_unix) = last_update_check_unix else {
        return true;
    };

    now_unix_secs().saturating_sub(last_update_check_unix) >= update_check_interval().as_secs()
}

fn schedule_auto_update_check(hwnd: HWND) {
    let delay_ms = {
        let state = lock_state();
        let Some(s) = state.as_ref() else {
            return;
        };

        if auto_update_check_due(s.last_update_check_unix) {
            None
        } else {
            let elapsed = now_unix_secs().saturating_sub(s.last_update_check_unix.unwrap_or(0));
            let remaining_secs = update_check_interval().as_secs().saturating_sub(elapsed);
            Some((remaining_secs.saturating_mul(1000)).min(u32::MAX as u64) as u32)
        }
    };

    unsafe {
        let _ = KillTimer(hwnd, TIMER_UPDATE_CHECK);
        if let Some(delay_ms) = delay_ms {
            SetTimer(hwnd, TIMER_UPDATE_CHECK, delay_ms.max(1), None);
        }
    }
}

fn refresh_usage_texts(state: &mut AppState) {
    if !state.last_poll_ok {
        return;
    }

    let strings = state.language.strings();
    let Some(data) = state.data.as_ref() else {
        return;
    };

    if let Some(claude_code) = data.claude_code.as_ref() {
        state.session_text = poller::format_line(&claude_code.session, strings);
        state.weekly_text = poller::format_line(&claude_code.weekly, strings);
    } else if state.show_claude_code {
        state.session_text = "!".to_string();
        state.weekly_text = "!".to_string();
    }

    if let Some(codex) = data.codex.as_ref() {
        state.codex_session_text = poller::format_line(&codex.session, strings);
        state.codex_weekly_text = poller::format_line(&codex.weekly, strings);
    } else if state.show_codex {
        state.codex_session_text = "!".to_string();
        state.codex_weekly_text = "!".to_string();
    }
}

fn set_window_title(hwnd: HWND, strings: Strings) {
    unsafe {
        let title = native_interop::wide_str(strings.window_title);
        let _ = SetWindowTextW(hwnd, PCWSTR::from_raw(title.as_ptr()));
    }
}

fn show_info_message(hwnd: HWND, title: &str, message: &str) {
    unsafe {
        let title_wide = native_interop::wide_str(title);
        let message_wide = native_interop::wide_str(message);
        let _ = MessageBoxW(
            hwnd,
            PCWSTR::from_raw(message_wide.as_ptr()),
            PCWSTR::from_raw(title_wide.as_ptr()),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

fn show_error_message(hwnd: HWND, title: &str, message: &str) {
    unsafe {
        let title_wide = native_interop::wide_str(title);
        let message_wide = native_interop::wide_str(message);
        let _ = MessageBoxW(
            hwnd,
            PCWSTR::from_raw(message_wide.as_ptr()),
            PCWSTR::from_raw(title_wide.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn show_first_run_config_message(hwnd: HWND) {
    let path = settings_path();
    let message = format!(
        "未找到配置文件，已创建默认配置文件。\n\n请打开并修改：\n{}\n\n使用 Sub2API 时，至少需要配置：\n- usage_mode: \"Sub2api\"\n- sub2api.base_url\n- sub2api.admin_api_key\n\ngroup_id 不需要手动填写。配置 base_url 和 admin_api_key 后，可在右键菜单的“分组”里切换分组，应用会自动保存。",
        path.display()
    );
    show_info_message(hwnd, "首次配置", &message);
}

fn show_update_prompt(hwnd: HWND, strings: Strings, release: &ReleaseDescriptor) -> bool {
    let message = strings
        .update_prompt_now
        .replace("{version}", &release.latest_version);

    unsafe {
        let title_wide = native_interop::wide_str(strings.update_available);
        let message_wide = native_interop::wide_str(&message);
        MessageBoxW(
            hwnd,
            PCWSTR::from_raw(message_wide.as_ptr()),
            PCWSTR::from_raw(title_wide.as_ptr()),
            MB_YESNO | MB_ICONQUESTION,
        ) == IDYES
    }
}

fn apply_language_to_state(state: &mut AppState, language_override: Option<LanguageId>) {
    state.language_override = language_override;
    state.language = localization::resolve_language(language_override);
    set_window_title(state.hwnd.to_hwnd(), state.language.strings());
    refresh_usage_texts(state);
}

fn update_language_change() -> bool {
    let mut state = lock_state();
    let Some(app_state) = state.as_mut() else {
        return false;
    };

    if app_state.language_override.is_some() {
        return false;
    }

    let new_language = localization::detect_system_language();
    if new_language == app_state.language {
        return false;
    }

    apply_language_to_state(app_state, None);
    true
}

fn version_action_label(
    strings: Strings,
    language: LanguageId,
    install_channel: InstallChannel,
    status: &UpdateStatus,
) -> String {
    let current = env!("CARGO_PKG_VERSION");
    match status {
        UpdateStatus::Idle => format!("v{current} - {}", strings.check_for_updates),
        UpdateStatus::Checking => format!("v{current} - {}", strings.checking_for_updates),
        UpdateStatus::Applying => format!("v{current} - {}", strings.applying_update),
        UpdateStatus::UpToDate => format!("v{current} - {}", strings.up_to_date_short),
        UpdateStatus::Available(release) => match install_channel {
            InstallChannel::Portable => {
                format!(
                    "v{current} - {} v{}",
                    strings.update_to, release.latest_version
                )
            }
            InstallChannel::Winget => format!(
                "v{current} - {} v{}",
                localization::update_via_winget(language),
                release.latest_version
            ),
        },
    }
}

fn begin_update_check(hwnd: HWND, interactive: bool) {
    let send_hwnd = SendHwnd::from_hwnd(hwnd);
    let (strings, install_channel) = {
        let mut state = lock_state();
        let Some(app_state) = state.as_mut() else {
            return;
        };

        if matches!(
            app_state.update_status,
            UpdateStatus::Checking | UpdateStatus::Applying
        ) {
            if interactive {
                show_info_message(
                    hwnd,
                    app_state.language.strings().updates,
                    app_state.language.strings().update_in_progress,
                );
            }
            return;
        }

        app_state.update_status = UpdateStatus::Checking;
        (app_state.language.strings(), app_state.install_channel)
    };

    std::thread::spawn(move || {
        let hwnd = send_hwnd.to_hwnd();
        let checked_at = now_unix_secs();
        match updater::check_for_updates() {
            Ok(UpdateCheckResult::UpToDate) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::UpToDate;
                        s.last_update_check_unix = Some(checked_at);
                    }
                }
                save_state_settings();
                if interactive {
                    show_info_message(hwnd, strings.updates, strings.up_to_date);
                }
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
            Ok(UpdateCheckResult::Available(release)) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::Available(release.clone());
                        s.last_update_check_unix = Some(checked_at);
                    }
                }
                save_state_settings();
                if interactive && show_update_prompt(hwnd, strings, &release) {
                    match install_channel {
                        InstallChannel::Portable => begin_update_apply(hwnd, release),
                        InstallChannel::Winget => begin_winget_update(hwnd),
                    }
                }
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
            Err(error) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::Idle;
                        s.last_update_check_unix = Some(checked_at);
                    }
                }
                save_state_settings();
                if interactive {
                    let message = format!("{}.\n\n{}", strings.update_failed, error);
                    show_error_message(hwnd, strings.updates, &message);
                }
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
        }
    });
}

fn begin_update_apply(hwnd: HWND, release: ReleaseDescriptor) {
    let send_hwnd = SendHwnd::from_hwnd(hwnd);
    let strings = {
        let mut state = lock_state();
        let Some(app_state) = state.as_mut() else {
            return;
        };

        if matches!(
            app_state.update_status,
            UpdateStatus::Checking | UpdateStatus::Applying
        ) {
            show_info_message(
                hwnd,
                app_state.language.strings().updates,
                app_state.language.strings().update_in_progress,
            );
            return;
        }

        app_state.update_status = UpdateStatus::Applying;
        app_state.language.strings()
    };

    std::thread::spawn(move || {
        let hwnd = send_hwnd.to_hwnd();
        match updater::begin_self_update(&release) {
            Ok(()) => unsafe {
                let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
            },
            Err(error) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::Available(release);
                    }
                }
                let message = format!("{}.\n\n{}", strings.update_failed, error);
                show_error_message(hwnd, strings.updates, &message);
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
        }
    });
}

fn begin_winget_update(hwnd: HWND) {
    let strings = {
        let state = lock_state();
        state.as_ref().map(|s| s.language.strings())
    }
    .unwrap_or(LanguageId::English.strings());

    match updater::begin_winget_update() {
        Ok(()) => unsafe {
            let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
        },
        Err(error) => {
            let message = format!("{}.\n\n{}", strings.update_failed, error);
            show_error_message(hwnd, strings.updates, &message);
        }
    }
}

const STARTUP_REGISTRY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const STARTUP_REGISTRY_KEY: &str = "ClaudeCodeUsageMonitor";

/// Returns true only if the startup registry value points to this executable.
fn is_startup_enabled() -> bool {
    unsafe {
        let path = native_interop::wide_str(STARTUP_REGISTRY_PATH);
        let key_name = native_interop::wide_str(STARTUP_REGISTRY_KEY);

        let mut hkey = HKEY::default();
        let result = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(path.as_ptr()),
            0,
            KEY_READ,
            &mut hkey,
        );
        if result.is_err() {
            return false;
        }

        // Query the size of the value
        let mut data_size: u32 = 0;
        let result = RegQueryValueExW(
            hkey,
            PCWSTR::from_raw(key_name.as_ptr()),
            None,
            None,
            None,
            Some(&mut data_size),
        );
        if result.is_err() || data_size == 0 {
            let _ = RegCloseKey(hkey);
            return false;
        }

        // Read the value
        let mut buf = vec![0u8; data_size as usize];
        let result = RegQueryValueExW(
            hkey,
            PCWSTR::from_raw(key_name.as_ptr()),
            None,
            None,
            Some(buf.as_mut_ptr()),
            Some(&mut data_size),
        );
        let _ = RegCloseKey(hkey);
        if result.is_err() {
            return false;
        }

        // Convert the registry value (UTF-16) to a string
        let wide_slice =
            std::slice::from_raw_parts(buf.as_ptr() as *const u16, data_size as usize / 2);
        let reg_value = String::from_utf16_lossy(wide_slice)
            .trim_end_matches('\0')
            .to_string();

        // Get the current executable path
        let mut exe_buf = [0u16; 260];
        let len = GetModuleFileNameW(None, &mut exe_buf) as usize;
        if len == 0 {
            return false;
        }
        let current_exe = String::from_utf16_lossy(&exe_buf[..len]);

        // Case-insensitive comparison (Windows paths are case-insensitive)
        reg_value.eq_ignore_ascii_case(&current_exe)
    }
}

fn set_startup_enabled(enable: bool) {
    unsafe {
        let path = native_interop::wide_str(STARTUP_REGISTRY_PATH);

        let mut hkey = HKEY::default();
        let result = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(path.as_ptr()),
            0,
            KEY_SET_VALUE,
            &mut hkey,
        );
        if result.is_err() {
            return;
        }

        let key_name = native_interop::wide_str(STARTUP_REGISTRY_KEY);

        if enable {
            let mut exe_buf = [0u16; 260];
            let len = GetModuleFileNameW(None, &mut exe_buf) as usize;
            if len > 0 {
                // Write the wide string including null terminator
                let byte_len = ((len + 1) * 2) as u32;
                let _ = RegSetValueExW(
                    hkey,
                    PCWSTR::from_raw(key_name.as_ptr()),
                    0,
                    REG_SZ,
                    Some(std::slice::from_raw_parts(
                        exe_buf.as_ptr() as *const u8,
                        byte_len as usize,
                    )),
                );
            }
        } else {
            let _ = RegDeleteValueW(hkey, PCWSTR::from_raw(key_name.as_ptr()));
        }

        let _ = RegCloseKey(hkey);
    }
}

// Dimensions matching the C# version
const SEGMENT_W: i32 = 10;
const SEGMENT_H: i32 = 13;
const SEGMENT_GAP: i32 = 1;
const SEGMENT_COUNT: i32 = 10;
const CORNER_RADIUS: i32 = 2;

const LEFT_DIVIDER_W: i32 = 3;
const DIVIDER_RIGHT_MARGIN: i32 = 10;
const LABEL_WIDTH: i32 = 18;
const LABEL_RIGHT_MARGIN: i32 = 10;
const BAR_RIGHT_MARGIN: i32 = 4;
const TEXT_WIDTH: i32 = 82;
const MODEL_RIGHT_MARGIN: i32 = 5;
const RIGHT_MARGIN: i32 = 2;
const WIDGET_HEIGHT: i32 = 46;
const FLOATING_WIDGET_HEIGHT: i32 = 58;
const FLOATING_CORNER_RADIUS: i32 = 10;
const FLOATING_TRANSPARENT_KEY: u32 = 0x00FF00FF;
const FLOATING_EDGE_MARGIN: i32 = 18;
const FLOATING_EDGE_SNAP_DISTANCE: i32 = 28;
const FLOATING_COLLAPSED_VISIBLE_WIDTH: i32 = 88;
const PET_SCENE_MAX_WIDTH: i32 = 220;
const PET_TRANSITION_MS: u128 = 300;
const PET_FRAME_MS: u128 = 120;
const PET_ACTION_CHECK_MS: u128 = 24_000;
const PET_ACTION_FRAME_COUNT: usize = crate::pixel_room::ACTION_FRAMES;
const PET_ACTION_CHANCE_PERCENT: u64 = 14;

fn pet_random_seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0x9E3779B97F4A7C15)
}

fn pet_action_roll(seed: u64, cycle: u128) -> u64 {
    let mut value = seed ^ (cycle as u64).wrapping_mul(0x9E3779B97F4A7C15);
    value ^= value << 13;
    value ^= value >> 7;
    value ^= value << 17;
    value
}

fn system_animations_enabled() -> bool {
    unsafe {
        let mut enabled = BOOL(1);
        if SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some(&mut enabled as *mut BOOL as *mut std::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_ok()
        {
            enabled.as_bool()
        } else {
            true
        }
    }
}

fn pet_reveal_for_state(state: &AppState) -> f64 {
    let Some(transition) = state.pet_transition else {
        return if state.detail_visible { 1.0 } else { 0.0 };
    };
    let elapsed = transition.start.elapsed().as_millis();
    let t = (elapsed as f64 / PET_TRANSITION_MS as f64).clamp(0.0, 1.0);
    let eased = 1.0 - (1.0 - t).powi(4);
    if transition.opening {
        eased
    } else {
        1.0 - eased
    }
}

fn pet_frame_for_state(state: &AppState) -> usize {
    if !state.pet_motion_enabled || !state.detail_visible {
        return 0;
    }
    let elapsed = state.pet_animation_started.elapsed().as_millis();
    let idle_frame =
        (elapsed / (PET_FRAME_MS * 3) % crate::pixel_room::IDLE_FRAMES as u128) as usize;
    let cycle = elapsed / PET_ACTION_CHECK_MS;
    let cycle_elapsed = elapsed % PET_ACTION_CHECK_MS;
    let action_duration = PET_FRAME_MS * PET_ACTION_FRAME_COUNT as u128;
    let roll = pet_action_roll(state.pet_action_seed, cycle);
    if cycle_elapsed < action_duration && roll % 100 < PET_ACTION_CHANCE_PERCENT {
        let action_kind = ((roll / 100) % 3) as usize;
        let action_frame = (cycle_elapsed / PET_FRAME_MS) as usize;
        return crate::pixel_room::IDLE_FRAMES
            + action_kind * PET_ACTION_FRAME_COUNT
            + action_frame.min(PET_ACTION_FRAME_COUNT - 1);
    }
    idle_frame
}

fn active_model_count(show_claude_code: bool, show_codex: bool) -> i32 {
    (show_claude_code as i32 + show_codex as i32).max(1)
}

fn row_bar_segment_count(active_models: i32) -> i32 {
    if active_models > 1 {
        5
    } else {
        SEGMENT_COUNT
    }
}

fn total_widget_width_for(active_models: i32) -> i32 {
    let bar_segments = row_bar_segment_count(active_models);
    let model_width = (sc(SEGMENT_W) + sc(SEGMENT_GAP)) * bar_segments - sc(SEGMENT_GAP)
        + sc(BAR_RIGHT_MARGIN)
        + sc(TEXT_WIDTH);

    sc(LEFT_DIVIDER_W)
        + sc(DIVIDER_RIGHT_MARGIN)
        + sc(LABEL_WIDTH)
        + sc(LABEL_RIGHT_MARGIN)
        + model_width * active_models
        + sc(MODEL_RIGHT_MARGIN) * (active_models - 1)
        + sc(RIGHT_MARGIN)
}

fn total_widget_width_for_state(state: &AppState) -> i32 {
    let main_width =
        total_widget_width_for(active_model_count(state.show_claude_code, state.show_codex));
    if state.detail_visible && state.placement_mode == PlacementMode::Floating {
        main_width.max(sc(DETAIL_PANEL_WIDTH))
    } else {
        main_width
    }
}

fn detail_row_count_for_state(state: &AppState) -> usize {
    let account_rows = state
        .data
        .as_ref()
        .and_then(|data| data.codex.as_ref())
        .map(|codex| codex.account_details.len())
        .unwrap_or(0);
    if account_rows == 0 {
        1
    } else {
        1 + account_rows.min(DETAIL_PANEL_MAX_ACCOUNT_ROWS)
    }
}

fn widget_height_for_state(state: &AppState) -> i32 {
    let base = if state.placement_mode == PlacementMode::Floating {
        sc(FLOATING_WIDGET_HEIGHT)
    } else {
        sc(WIDGET_HEIGHT)
    };
    if state.detail_visible && state.placement_mode == PlacementMode::Floating {
        base + detail_panel_height_for_rows(detail_row_count_for_state(state))
    } else {
        base
    }
}

fn detail_panel_height_for_rows(rows: usize) -> i32 {
    sc(DETAIL_PANEL_PADDING * 2 + DETAIL_ROW_HEIGHT * rows as i32 + 12)
}

fn main_widget_height_for_placement(placement_mode: PlacementMode) -> i32 {
    if placement_mode == PlacementMode::Floating {
        sc(FLOATING_WIDGET_HEIGHT)
    } else {
        sc(WIDGET_HEIGHT)
    }
}

fn current_widget_height() -> i32 {
    let state = lock_state();
    state
        .as_ref()
        .map(widget_height_for_state)
        .unwrap_or(sc(WIDGET_HEIGHT))
}

fn total_widget_width() -> i32 {
    let state = {
        let state = lock_state();
        state.as_ref().map(|s| {
            let main_width =
                total_widget_width_for(active_model_count(s.show_claude_code, s.show_codex));
            if s.detail_visible && s.placement_mode == PlacementMode::Floating {
                main_width.max(sc(DETAIL_PANEL_WIDTH))
            } else {
                main_width
            }
        })
    };
    match state {
        Some(width) => width,
        None => total_widget_width_for(1),
    }
}

fn claude_accent_color() -> Color {
    Color::from_hex("#D97757")
}

fn codex_accent_color(is_dark: bool) -> Color {
    if is_dark {
        Color::from_hex("#F5F5F5")
    } else {
        Color::from_hex("#1F1F1F")
    }
}

fn claude_usage_text_color(is_dark: bool) -> Color {
    if is_dark {
        Color::from_hex("#F09A7A")
    } else {
        Color::from_hex("#A94F32")
    }
}

fn codex_usage_text_color(is_dark: bool) -> Color {
    if is_dark {
        Color::from_hex("#F5F5F5")
    } else {
        Color::from_hex("#1F1F1F")
    }
}

pub fn run() {
    // Enable Per-Monitor DPI Awareness V2 for crisp rendering at any scale factor
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CURRENT_DPI.store(GetDpiForSystem(), Ordering::Relaxed);
    }
    diagnose::log("window::run started");

    // Single-instance guard: silently exit if another instance is running
    let mutex_name = native_interop::wide_str("Global\\ClaudeCodeUsageMonitor");
    let _mutex = unsafe {
        let handle = CreateMutexW(None, false, PCWSTR::from_raw(mutex_name.as_ptr()));
        match handle {
            Ok(h) => {
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    diagnose::log("startup aborted: another instance is already running");
                    return;
                }
                h
            }
            Err(error) => {
                diagnose::log_error(
                    "startup aborted: unable to create single-instance mutex",
                    error,
                );
                return;
            }
        }
    };

    let class_name = native_interop::wide_str("ClaudeCodeUsageMonitor");
    let detail_class_name = native_interop::wide_str("ClaudeCodeUsageDetail");

    unsafe {
        let hinstance = GetModuleHandleW(PCWSTR::null()).unwrap();
        let (large_icon, small_icon) = load_embedded_app_icons();

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: HINSTANCE(hinstance.0),
            hIcon: large_icon,
            hIconSm: small_icon,
            hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
            ..Default::default()
        };

        let atom = RegisterClassExW(&wc);
        if atom == 0 {
            diagnose::log("RegisterClassExW returned 0");
        }

        let detail_wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(detail_wnd_proc),
            hInstance: HINSTANCE(hinstance.0),
            hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: PCWSTR::from_raw(detail_class_name.as_ptr()),
            ..Default::default()
        };
        let detail_atom = RegisterClassExW(&detail_wc);
        if detail_atom == 0 {
            diagnose::log("RegisterClassExW detail returned 0");
        }

        let settings_file_missing = !settings_path().exists();
        let settings = load_settings();
        if settings_file_missing {
            save_settings(&settings);
        }
        let language_override = settings.language.as_deref().and_then(LanguageId::from_code);
        let language = localization::resolve_language(language_override);
        let install_channel = updater::current_install_channel();
        let cached_usage = app_usage_from_cached(&settings);

        // Create as layered popup (will be reparented into taskbar)
        let title = native_interop::wide_str(language.strings().window_title);
        let initial_model_count =
            active_model_count(settings.show_claude_code, settings.show_codex);
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
            PCWSTR::from_raw(class_name.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            WS_POPUP,
            0,
            0,
            total_widget_width_for(initial_model_count),
            if settings.placement_mode == PlacementMode::Floating {
                sc(FLOATING_WIDGET_HEIGHT)
            } else {
                sc(WIDGET_HEIGHT)
            },
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        )
        .unwrap();

        if !large_icon.is_invalid() {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                WPARAM(ICON_BIG as usize),
                LPARAM(large_icon.0 as isize),
            );
        }
        if !small_icon.is_invalid() {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                WPARAM(ICON_SMALL as usize),
                LPARAM(small_icon.0 as isize),
            );
        }

        diagnose::log(format!("main window created hwnd={:?}", hwnd));

        let is_dark = theme::is_dark_mode();
        let mut embedded = false;

        {
            let mut state = lock_state();
            *state = Some(AppState {
                hwnd: SendHwnd::from_hwnd(hwnd),
                detail_hwnd: None,
                taskbar_hwnd: None,
                tray_notify_hwnd: None,
                win_event_hook: None,
                is_dark,
                embedded: false,
                language_override,
                language,
                install_channel,
                session_percent: 0.0,
                session_text: "--".to_string(),
                weekly_percent: 0.0,
                weekly_text: "--".to_string(),
                codex_session_percent: 0.0,
                codex_session_text: "--".to_string(),
                codex_weekly_percent: 0.0,
                codex_weekly_text: "--".to_string(),
                show_claude_code: settings.show_claude_code,
                show_codex: settings.show_codex,
                placement_mode: settings.placement_mode,
                widget_theme: settings.widget_theme,
                season: settings.season,
                progress_color_mode: settings.progress_color_mode,
                floating_x: settings.floating_x,
                floating_y: settings.floating_y,
                edge_collapse: settings.edge_collapse,
                edge_expanded: false,
                mouse_tracking: false,
                edge_animation: None,
                detail_visible: false,
                detail_closing: false,
                detail_mouse_down_seen: false,
                pet_animation_started: Instant::now(),
                pet_action_seed: pet_random_seed(),
                pet_motion_enabled: system_animations_enabled(),
                pet_transition: None,
                sub2api_groups: Vec::new(),
                sub2api_groups_loading: false,
                sub2api_groups_loaded: false,
                data: cached_usage,
                poll_interval_ms: settings.poll_interval_ms,
                retry_count: 0,
                force_notify_auth_error: false,
                auth_error_paused_polling: false,
                auth_watch_mode: poller::CredentialWatchMode::ActiveSource,
                auth_watch_snapshot: Vec::new(),
                last_poll_ok: false,
                update_status: UpdateStatus::Idle,
                last_update_check_unix: settings.last_update_check_unix,
                tray_offset: settings.tray_offset,
                dragging: false,
                drag_start_mouse_x: 0,
                drag_start_mouse_y: 0,
                drag_start_offset: 0,
                drag_start_floating_x: 0,
                drag_start_floating_y: 0,
                widget_visible: settings.widget_visible,
            });
            if let Some(s) = state.as_mut() {
                if let Some(codex) = s.data.as_ref().and_then(|data| data.codex.as_ref()) {
                    s.codex_session_percent = codex.session.percentage;
                    s.codex_weekly_percent = codex.weekly.percentage;
                    s.last_poll_ok = true;
                    refresh_usage_texts(s);
                    diagnose::log("initialized widget from cached usage");
                }
            }
        }

        // Try to embed in taskbar
        if let Some(taskbar_hwnd) = native_interop::find_taskbar() {
            diagnose::log(format!("taskbar found hwnd={:?}", taskbar_hwnd));
            native_interop::embed_in_taskbar(hwnd, taskbar_hwnd);
            embedded = true;

            let mut state = lock_state();
            let s = state.as_mut().unwrap();
            s.taskbar_hwnd = Some(taskbar_hwnd);
            s.embedded = true;

            let tray_notify = native_interop::find_child_window(taskbar_hwnd, "TrayNotifyWnd");
            s.tray_notify_hwnd = tray_notify;
            if tray_notify.is_some() {
                diagnose::log("TrayNotifyWnd found");
            } else {
                diagnose::log("TrayNotifyWnd not found");
            }

            if let Some(tray_hwnd) = tray_notify {
                let thread_id = native_interop::get_window_thread_id(tray_hwnd);
                let hook = native_interop::set_tray_event_hook(thread_id, on_tray_location_changed);
                s.win_event_hook = hook;
                if hook.is_some() {
                    diagnose::log("tray event hook installed");
                } else {
                    diagnose::log("tray event hook could not be installed");
                }
            }
        } else {
            diagnose::log("taskbar not found; using fallback popup window");
        }

        // If not embedded, fall back to topmost popup with SetLayeredWindowAttributes
        if !embedded {
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
            let _ = SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }

        // Register system tray icon(s)
        sync_tray_icons(hwnd);

        // Position and show (only if widget_visible preference is true)
        position_at_taskbar();
        if settings.widget_visible {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        diagnose::log("window shown");

        // Initial render via UpdateLayeredWindow (for embedded) or InvalidateRect (fallback)
        render_layered();

        if settings_file_missing {
            show_first_run_config_message(hwnd);
        }

        // Poll timer: 15 minutes
        let initial_poll_ms = {
            let state = lock_state();
            state
                .as_ref()
                .map(|s| s.poll_interval_ms)
                .unwrap_or(POLL_15_MIN)
        };
        SetTimer(hwnd, TIMER_POLL, initial_poll_ms, None);

        // Initial poll
        let send_hwnd = SendHwnd::from_hwnd(hwnd);
        std::thread::spawn(move || {
            diagnose::log("initial poll thread started");
            do_poll(send_hwnd);
        });
        refresh_sub2api_groups(SendHwnd::from_hwnd(hwnd));

        schedule_auto_update_check(hwnd);
        let should_check_updates = {
            let state = lock_state();
            state
                .as_ref()
                .map(|s| auto_update_check_due(s.last_update_check_unix))
                .unwrap_or(false)
        };
        if should_check_updates {
            begin_update_check(hwnd, false);
        }

        // Initial theme check
        check_theme_change();

        // Message loop
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Render widget content and push to the layered window via UpdateLayeredWindow.
/// Renders fully opaque with the actual taskbar background colour so that
/// ClearType sub-pixel font rendering can be used for crisp, OS-native text.
fn render_layered() {
    refresh_dpi();
    let (
        hwnd_val,
        is_dark,
        _embedded,
        strings,
        session_pct,
        session_text,
        weekly_pct,
        weekly_text,
        codex_session_pct,
        codex_session_text,
        codex_weekly_pct,
        codex_weekly_text,
        show_claude_code,
        show_codex,
        placement_mode,
        widget_theme,
        progress_color_mode,
        edge_collapse,
        edge_expanded,
        detail_visible,
        pet_reveal,
        pet_frame,
    ) = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => (
                s.hwnd,
                s.is_dark,
                s.embedded,
                s.language.strings(),
                s.session_percent,
                s.session_text.clone(),
                s.weekly_percent,
                s.weekly_text.clone(),
                s.codex_session_percent,
                s.codex_session_text.clone(),
                s.codex_weekly_percent,
                s.codex_weekly_text.clone(),
                s.show_claude_code,
                s.show_codex,
                s.placement_mode,
                s.widget_theme,
                s.progress_color_mode,
                s.edge_collapse,
                s.edge_expanded,
                s.detail_visible,
                pet_reveal_for_state(s),
                pet_frame_for_state(s),
            ),
            None => return,
        }
    };

    let hwnd = hwnd_val.to_hwnd();

    let width = total_widget_width();
    let main_width = total_widget_width_for(active_model_count(show_claude_code, show_codex));
    let expanded_header = detail_visible && placement_mode == PlacementMode::Floating;
    let main_surface_width = if expanded_header { width } else { main_width };
    let height = current_widget_height();
    let accent = claude_accent_color();
    let codex_accent = codex_accent_color(is_dark);
    let effective_dark = if placement_mode == PlacementMode::Floating {
        widget_theme == WidgetTheme::Dark
    } else {
        is_dark
    };
    let track = if placement_mode == PlacementMode::Floating {
        if effective_dark {
            Color::from_hex("#303640")
        } else {
            Color::from_hex("#CDD3DB")
        }
    } else if is_dark {
        Color::from_hex("#444444")
    } else {
        Color::from_hex("#AAAAAA")
    };
    let text_color = if effective_dark {
        if placement_mode == PlacementMode::Floating {
            Color::from_hex("#C8D0DA")
        } else {
            Color::from_hex("#888888")
        }
    } else {
        Color::from_hex("#404040")
    };
    let accent = if placement_mode == PlacementMode::Floating && effective_dark {
        Color::from_hex("#FFB28A")
    } else {
        accent
    };
    let codex_accent = if placement_mode == PlacementMode::Floating && effective_dark {
        Color::from_hex("#EAF2FF")
    } else if placement_mode == PlacementMode::Floating {
        Color::from_hex("#172033")
    } else {
        codex_accent
    };
    let bg_color = if placement_mode == PlacementMode::Floating {
        if widget_theme == WidgetTheme::Dark {
            Color::from_hex("#11151B")
        } else {
            Color::from_hex("#FFFFFF")
        }
    } else if is_dark {
        Color::from_hex("#1C1C1C")
    } else {
        Color::from_hex("#F3F3F3")
    };

    unsafe {
        let screen_dc = GetDC(HWND::default());

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let mem_dc = CreateCompatibleDC(screen_dc);
        let dib =
            CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();

        if dib.is_invalid() || bits.is_null() {
            let _ = DeleteDC(mem_dc);
            ReleaseDC(HWND::default(), screen_dc);
            return;
        }

        let old_bmp = SelectObject(mem_dc, dib);
        let pixel_count = (width * height) as usize;
        let clear_brush = CreateSolidBrush(COLORREF(FLOATING_TRANSPARENT_KEY));
        let clear_rect = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        FillRect(mem_dc, &clear_rect, clear_brush);
        let _ = DeleteObject(clear_brush);

        // Render once with the actual taskbar background colour.
        // Using an opaque background lets us use CLEARTYPE_QUALITY for
        // sub-pixel font rendering that matches the rest of the OS.
        paint_content(
            mem_dc,
            main_surface_width,
            main_widget_height_for_placement(placement_mode),
            is_dark,
            &bg_color,
            &text_color,
            &accent,
            &track,
            strings,
            session_pct,
            &session_text,
            weekly_pct,
            &weekly_text,
            codex_session_pct,
            &codex_session_text,
            codex_weekly_pct,
            &codex_weekly_text,
            show_claude_code,
            show_codex,
            placement_mode,
            progress_color_mode,
            if edge_expanded {
                EdgeCollapse::None
            } else {
                edge_collapse
            },
            &codex_accent,
            expanded_header,
            pet_reveal,
            pet_frame,
        );

        if detail_visible && placement_mode == PlacementMode::Floating {
            paint_detail_panel(mem_dc, width, height);
        }

        // Fixed mode: taskbar background pixels → alpha 1 (nearly invisible but hittable).
        // Floating mode: transparent-key pixels outside the rounded card → alpha 0.
        // Content pixels → fully opaque (preserves ClearType sub-pixel rendering).
        let bg_bgr = bg_color.to_colorref();
        let pixel_data = std::slice::from_raw_parts_mut(bits as *mut u32, pixel_count);
        for px in pixel_data.iter_mut() {
            let rgb = *px & 0x00FFFFFF;
            if (placement_mode == PlacementMode::Floating || detail_visible)
                && rgb == FLOATING_TRANSPARENT_KEY
            {
                *px = 0x00000000;
            } else if placement_mode == PlacementMode::Fixed && rgb == bg_bgr {
                *px = 0x01000000;
            } else {
                *px = rgb | 0xFF000000;
            }
        }

        // Push to window via UpdateLayeredWindow
        let pt_src = POINT { x: 0, y: 0 };
        let sz = SIZE {
            cx: width,
            cy: height,
        };
        let blend = BLENDFUNCTION {
            BlendOp: 0, // AC_SRC_OVER
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: 1, // AC_SRC_ALPHA
        };

        let _ = UpdateLayeredWindow(
            hwnd,
            screen_dc,
            None,
            Some(&sz),
            mem_dc,
            Some(&pt_src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );

        // Cleanup
        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(HWND::default(), screen_dc);
    }
}

/// Paint all widget content onto a DC with a given background color.
fn paint_content(
    hdc: HDC,
    width: i32,
    main_height: i32,
    is_dark: bool,
    bg: &Color,
    text_color: &Color,
    accent: &Color,
    track: &Color,
    strings: Strings,
    session_pct: f64,
    session_text: &str,
    weekly_pct: f64,
    weekly_text: &str,
    codex_session_pct: f64,
    codex_session_text: &str,
    codex_weekly_pct: f64,
    codex_weekly_text: &str,
    show_claude_code: bool,
    show_codex: bool,
    placement_mode: PlacementMode,
    progress_color_mode: ProgressColorMode,
    edge_collapse: EdgeCollapse,
    codex_accent: &Color,
    wide_header: bool,
    pet_reveal: f64,
    pet_frame: usize,
) {
    unsafe {
        let client_rect = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: main_height,
        };

        if placement_mode == PlacementMode::Floating {
            let clear_brush = CreateSolidBrush(COLORREF(native_interop::colorref(255, 0, 255)));
            FillRect(hdc, &client_rect, clear_brush);
            let _ = DeleteObject(clear_brush);

            let card_rect = RECT {
                left: sc(3),
                top: sc(3),
                right: width - sc(3),
                bottom: main_height - sc(3),
            };
            if bg.r >= 100 {
                let shadow_rect = RECT {
                    left: sc(2),
                    top: sc(3),
                    right: width - sc(2),
                    bottom: main_height - sc(2),
                };
                draw_rounded_frame(
                    hdc,
                    &shadow_rect,
                    &Color::from_hex("#D1D5DB"),
                    sc(FLOATING_CORNER_RADIUS + 2),
                );
            }
            draw_rounded_rect(hdc, &card_rect, bg, sc(FLOATING_CORNER_RADIUS));

            let border = if bg.r < 100 {
                Color::from_hex("#353C47")
            } else {
                Color::from_hex("#F4F5F7")
            };
            draw_rounded_frame(hdc, &card_rect, &border, sc(FLOATING_CORNER_RADIUS));
        } else {
            let bg_brush = CreateSolidBrush(COLORREF(bg.to_colorref()));
            FillRect(hdc, &client_rect, bg_brush);
            let _ = DeleteObject(bg_brush);
        }

        if placement_mode == PlacementMode::Fixed {
            // Left divider
            let divider_h = sc(25);
            let divider_top = (main_height - divider_h) / 2;
            let divider_bottom = divider_top + divider_h;

            let (div_left, div_right) = if is_dark {
                ((80, 80, 80), (40, 40, 40))
            } else {
                ((160, 160, 160), (230, 230, 230))
            };

            let left_brush = CreateSolidBrush(COLORREF(native_interop::colorref(
                div_left.0, div_left.1, div_left.2,
            )));
            let left_rect = RECT {
                left: 0,
                top: divider_top,
                right: sc(2),
                bottom: divider_bottom,
            };
            FillRect(hdc, &left_rect, left_brush);
            let _ = DeleteObject(left_brush);

            let right_brush = CreateSolidBrush(COLORREF(native_interop::colorref(
                div_right.0,
                div_right.1,
                div_right.2,
            )));
            let right_rect = RECT {
                left: sc(2),
                top: divider_top,
                right: sc(3),
                bottom: divider_bottom,
            };
            FillRect(hdc, &right_rect, right_brush);
            let _ = DeleteObject(right_brush);
        }

        let content_x = if placement_mode == PlacementMode::Floating {
            if wide_header {
                sc(12)
            } else {
                let natural_width =
                    total_widget_width_for(active_model_count(show_claude_code, show_codex));
                sc(12) + (width - natural_width).max(0) / 2
            }
        } else {
            sc(LEFT_DIVIDER_W) + sc(DIVIDER_RIGHT_MARGIN)
        };
        let row2_y = if placement_mode == PlacementMode::Floating {
            main_height - sc(10) - sc(SEGMENT_H)
        } else {
            main_height - sc(5) - sc(SEGMENT_H)
        };
        let row1_y = if placement_mode == PlacementMode::Floating {
            row2_y - sc(12) - sc(SEGMENT_H)
        } else {
            row2_y - sc(10) - sc(SEGMENT_H)
        };

        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, COLORREF(text_color.to_colorref()));

        if placement_mode == PlacementMode::Floating && edge_collapse != EdgeCollapse::None {
            draw_collapsed_floating_content(
                hdc,
                width,
                main_height,
                edge_collapse,
                text_color,
                codex_session_pct,
                codex_session_text,
                codex_weekly_pct,
                codex_weekly_text,
                progress_color_mode,
            );
            return;
        }

        let font_name = native_interop::wide_str("Segoe UI");
        let font = CreateFontW(
            sc(-12),
            0,
            0,
            0,
            FW_MEDIUM.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_TT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            PCWSTR::from_raw(font_name.as_ptr()),
        );
        let old_font = SelectObject(hdc, font);

        draw_row(
            hdc,
            content_x,
            row1_y,
            is_dark,
            text_color,
            strings.session_window,
            session_pct,
            session_text,
            codex_session_pct,
            codex_session_text,
            show_claude_code,
            show_codex,
            accent,
            codex_accent,
            track,
            progress_color_mode,
        );
        draw_row(
            hdc,
            content_x,
            row2_y,
            is_dark,
            text_color,
            strings.weekly_window,
            weekly_pct,
            weekly_text,
            codex_weekly_pct,
            codex_weekly_text,
            show_claude_code,
            show_codex,
            accent,
            codex_accent,
            track,
            progress_color_mode,
        );

        if wide_header {
            draw_pet_scene(
                hdc,
                width,
                main_height,
                show_claude_code,
                show_codex,
                pet_reveal,
                pet_frame,
            );
        }

        SelectObject(hdc, old_font);
        let _ = DeleteObject(font);
    }
}

fn pet_scene_rect(width: i32, height: i32, show_claude_code: bool, show_codex: bool) -> RECT {
    let natural_width = total_widget_width_for(active_model_count(show_claude_code, show_codex));
    let left = natural_width + sc(8);
    let right = width - sc(12);
    let available_width = (right - left).max(0);
    let scene_width = available_width.min(sc(PET_SCENE_MAX_WIDTH));
    RECT {
        left: right - scene_width,
        top: sc(5),
        right,
        bottom: height - sc(5),
    }
}

fn draw_pet_scene(
    hdc: HDC,
    width: i32,
    height: i32,
    show_claude_code: bool,
    show_codex: bool,
    reveal: f64,
    frame: usize,
) {
    if reveal <= 0.0 {
        return;
    }
    let rect = pet_scene_rect(width, height, show_claude_code, show_codex);
    let scene_width = rect.right - rect.left;
    let scene_height = rect.bottom - rect.top;
    if scene_width <= 0 || scene_height <= 0 {
        return;
    }
    let hour = std::env::var("CLAUDE_USAGE_MONITOR_PET_PREVIEW_HOUR")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|hour| *hour < 24)
        .unwrap_or_else(|| unsafe { GetLocalTime().wHour });
    // Match the already-painted card, so negative space works in both themes.
    let card_color = unsafe { GetPixel(hdc, rect.left, rect.top) }.0;
    let background =
        ((card_color & 0xff) << 16) | (card_color & 0xff00) | ((card_color >> 16) & 0xff);
    let (ambient_ms, season) = {
        let state = lock_state();
        state
            .as_ref()
            .map(|s| {
                (
                    if s.pet_motion_enabled {
                        s.pet_animation_started.elapsed().as_millis() as u64
                    } else {
                        0
                    },
                    s.season.resolve(unsafe { GetLocalTime().wMonth }),
                )
            })
            .unwrap_or((
                0,
                crate::pixel_room::Season::Auto.resolve(unsafe { GetLocalTime().wMonth }),
            ))
    };
    let pixels = crate::pixel_room::render(hour, frame, background, ambient_ms, season);
    let scale = (scene_width as f64 / crate::pixel_room::WIDTH as f64)
        .min(scene_height as f64 / crate::pixel_room::HEIGHT as f64);
    let art_width = (crate::pixel_room::WIDTH as f64 * scale).round() as i32;
    let art_height = (crate::pixel_room::HEIGHT as f64 * scale).round() as i32;
    let x = rect.left + (scene_width - art_width) / 2;
    let y = rect.top + (scene_height - art_height) / 2;
    let reveal_width = (scene_width as f64 * reveal.clamp(0.0, 1.0)).round() as i32;
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: crate::pixel_room::WIDTH as i32,
            biHeight: -(crate::pixel_room::HEIGHT as i32),
            biPlanes: 1,
            biBitCount: 32,
            ..Default::default()
        },
        ..Default::default()
    };
    unsafe {
        let saved = SaveDC(hdc);
        if saved == 0 {
            return;
        }
        let _ = IntersectClipRect(
            hdc,
            rect.right - reveal_width,
            rect.top,
            rect.right,
            rect.bottom,
        );
        // COLORONCOLOR preserves hard pixel edges, including fractional Windows DPI scales.
        SetStretchBltMode(hdc, COLORONCOLOR);
        StretchDIBits(
            hdc,
            x,
            y,
            art_width,
            art_height,
            0,
            0,
            crate::pixel_room::WIDTH as i32,
            crate::pixel_room::HEIGHT as i32,
            Some(pixels.as_ptr().cast()),
            &bmi,
            DIB_RGB_COLORS,
            SRCCOPY,
        );
        let _ = RestoreDC(hdc, saved);
    }
}

fn draw_collapsed_floating_content(
    hdc: HDC,
    width: i32,
    height: i32,
    edge_collapse: EdgeCollapse,
    text_color: &Color,
    codex_session_pct: f64,
    codex_session_text: &str,
    codex_weekly_pct: f64,
    codex_weekly_text: &str,
    progress_color_mode: ProgressColorMode,
) {
    unsafe {
        let font_name = native_interop::wide_str("Segoe UI");
        let font = CreateFontW(
            sc(-12),
            0,
            0,
            0,
            FW_SEMIBOLD.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_TT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            PCWSTR::from_raw(font_name.as_ptr()),
        );
        let old_font = SelectObject(hdc, font);
        let _ = SetTextColor(hdc, COLORREF(text_color.to_colorref()));

        let lines = [
            collapsed_usage_parts(codex_session_pct, codex_session_text),
            collapsed_usage_parts(codex_weekly_pct, codex_weekly_text),
        ];
        let row_h = sc(19);
        let top = (height - row_h * 2) / 2;
        for (idx, (pct_text, countdown)) in lines.iter().enumerate() {
            let visible_w = sc(FLOATING_COLLAPSED_VISIBLE_WIDTH);
            let left = if edge_collapse == EdgeCollapse::Left {
                width - visible_w
            } else {
                0
            };
            let row_top = top + idx as i32 * row_h;
            let pct_color = progress_fill_color(
                if idx == 0 {
                    codex_session_pct
                } else {
                    codex_weekly_pct
                }
                .clamp(0.0, 100.0),
                text_color,
                progress_color_mode,
            );
            let mut pct_wide = native_interop::wide_str(pct_text);
            let pct_rect = RECT {
                left: left + sc(13),
                top: row_top,
                right: left + sc(47),
                bottom: row_top + row_h,
            };
            let _ = SetTextColor(hdc, COLORREF(pct_color.to_colorref()));
            let _ = DrawTextW(
                hdc,
                &mut pct_wide,
                &pct_rect as *const RECT as *mut RECT,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
            );

            let mut countdown_wide = native_interop::wide_str(countdown);
            let countdown_rect = RECT {
                left: left + sc(47),
                top: top + idx as i32 * row_h,
                right: left + visible_w - sc(3),
                bottom: top + (idx as i32 + 1) * row_h,
            };
            let _ = SetTextColor(hdc, COLORREF(text_color.to_colorref()));
            let _ = DrawTextW(
                hdc,
                &mut countdown_wide,
                &countdown_rect as *const RECT as *mut RECT,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
            );
        }

        SelectObject(hdc, old_font);
        let _ = DeleteObject(font);
    }
}

fn collapsed_usage_parts(percent: f64, text: &str) -> (String, String) {
    let countdown = text
        .split('\u{00b7}')
        .nth(1)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(compact_countdown_text)
        .unwrap_or_else(|| "--".to_string());
    (format!("{:.0}%", percent), countdown)
}

fn compact_countdown_text(text: &str) -> String {
    let token = text.split_whitespace().next().unwrap_or(text).trim();
    if token.eq_ignore_ascii_case("now") {
        return "1m".to_string();
    }
    if token.ends_with('d') || token.ends_with('h') || token.ends_with('m') {
        return token.to_string();
    }
    if let Some(raw_secs) = token.strip_suffix('s').and_then(|v| v.parse::<u64>().ok()) {
        let minutes = raw_secs.div_ceil(60).max(1);
        return format!("{minutes}m");
    }
    token.to_string()
}

fn work_area_rect() -> RECT {
    unsafe {
        let mut rect = RECT::default();
        if SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut rect as *mut RECT as *mut std::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_ok()
        {
            rect
        } else {
            RECT {
                left: 0,
                top: 0,
                right: GetSystemMetrics(SM_CXSCREEN),
                bottom: GetSystemMetrics(SM_CYSCREEN),
            }
        }
    }
}

fn full_floating_widget_width_for_state(state: &AppState) -> i32 {
    total_widget_width_for_state(state)
}

fn collapsed_floating_x(edge: EdgeCollapse, work: &RECT, width: i32) -> i32 {
    match edge {
        EdgeCollapse::Left => work.left - width + sc(FLOATING_COLLAPSED_VISIBLE_WIDTH),
        EdgeCollapse::Right => work.right - sc(FLOATING_COLLAPSED_VISIBLE_WIDTH) - sc(1),
        EdgeCollapse::None => work.left,
    }
}

fn clamp_floating_position(
    desired_x: i32,
    desired_y: i32,
    width: i32,
    height: i32,
    work: &RECT,
) -> (i32, i32) {
    let margin = sc(FLOATING_EDGE_MARGIN);
    let min_x = work.left + margin;
    let max_x = (work.right - width - margin).max(min_x);
    let min_y = work.top + margin;
    let max_y = (work.bottom - height - margin).max(min_y);
    (desired_x.clamp(min_x, max_x), desired_y.clamp(min_y, max_y))
}

fn floating_edge_for_position(desired_x: i32, width: i32, work: &RECT) -> EdgeCollapse {
    let snap = sc(FLOATING_EDGE_SNAP_DISTANCE);
    if desired_x <= work.left + snap {
        EdgeCollapse::Left
    } else if desired_x + width >= work.right - snap {
        EdgeCollapse::Right
    } else {
        EdgeCollapse::None
    }
}

fn track_mouse_leave(hwnd: HWND) {
    unsafe {
        let mut track = TRACKMOUSEEVENT {
            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: 0,
        };
        let _ = TrackMouseEvent(&mut track);
    }
}

fn expanded_floating_x(edge: EdgeCollapse, work: &RECT, width: i32) -> i32 {
    match edge {
        EdgeCollapse::Left => work.left + sc(1),
        EdgeCollapse::Right => work.right - width - sc(1),
        EdgeCollapse::None => work.left + sc(FLOATING_EDGE_MARGIN),
    }
}

fn edge_target_rect(edge: EdgeCollapse, expanded: bool) -> Option<(i32, i32, i32, i32)> {
    if edge == EdgeCollapse::None {
        return None;
    }
    let work = work_area_rect();
    let state = lock_state();
    let s = state.as_ref()?;
    let width = full_floating_widget_width_for_state(s);
    let height = widget_height_for_state(s);
    let x = if expanded {
        expanded_floating_x(edge, &work, width)
    } else {
        collapsed_floating_x(edge, &work, width)
    };
    let y = s
        .floating_y
        .unwrap_or(work.top + sc(FLOATING_EDGE_MARGIN))
        .clamp(
            work.top + sc(FLOATING_EDGE_MARGIN),
            (work.bottom - height - sc(FLOATING_EDGE_MARGIN))
                .max(work.top + sc(FLOATING_EDGE_MARGIN)),
        );
    Some((x, y, width, height))
}

fn start_edge_animation(hwnd: HWND, edge: EdgeCollapse, expanded: bool) {
    if edge == EdgeCollapse::None {
        return;
    }
    let Some((to_x, y, _to_width, height)) = edge_target_rect(edge, expanded) else {
        return;
    };
    let mut from_x = to_x;
    if let Some(rect) = native_interop::get_window_rect_safe(hwnd) {
        from_x = rect.left;
    }

    {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            s.edge_expanded = expanded;
            s.floating_x = Some(to_x);
            s.floating_y = Some(y);
            s.edge_animation = Some(EdgeAnimation {
                from_x,
                to_x,
                y,
                height,
                start: Instant::now(),
                duration: Duration::from_millis(180),
            });
        }
    }
    unsafe {
        SetTimer(hwnd, TIMER_EDGE_ANIMATION, 15, None);
    }
}

fn tick_edge_animation(hwnd: HWND) {
    let (x, y, width, height, finished) = {
        let mut state = lock_state();
        let Some(s) = state.as_mut() else {
            return;
        };
        let Some(animation) = s.edge_animation else {
            unsafe {
                let _ = KillTimer(hwnd, TIMER_EDGE_ANIMATION);
            }
            return;
        };
        let elapsed = animation.start.elapsed().as_secs_f64();
        let duration = animation.duration.as_secs_f64().max(0.001);
        let t = (elapsed / duration).clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - t).powi(4);
        let lerp_i32 = |from: i32, to: i32| from + ((to - from) as f64 * eased).round() as i32;
        let finished = t >= 1.0;
        let x = lerp_i32(animation.from_x, animation.to_x);
        let width = full_floating_widget_width_for_state(s);
        if finished {
            s.edge_animation = None;
            s.floating_x = Some(animation.to_x);
            s.floating_y = Some(animation.y);
        }
        (x, animation.y, width, animation.height, finished)
    };

    native_interop::move_window(hwnd, x, y, width, height);
    if finished {
        ensure_floating_topmost(hwnd);
        unsafe {
            let _ = KillTimer(hwnd, TIMER_EDGE_ANIMATION);
        }
        render_layered();
    }
}

fn do_poll(send_hwnd: SendHwnd) {
    let hwnd = send_hwnd.to_hwnd();
    let (show_claude_code, show_codex, usage_mode, sub2api_config) = {
        let state = lock_state();
        let (show_claude_code, show_codex) = state
            .as_ref()
            .map(|s| (s.show_claude_code, s.show_codex))
            .unwrap_or((false, true));
        let settings = load_settings();
        (
            show_claude_code,
            show_codex,
            settings.usage_mode,
            settings.sub2api,
        )
    };

    if show_codex && usage_mode == poller::UsageMode::Sub2api {
        match poller::poll_sub2api_codex_quick(&sub2api_config) {
            Ok(codex) => {
                diagnose::log("quick usage poll succeeded");
                apply_codex_quick_poll_success(hwnd, codex.clone());
                match poller::poll_sub2api_codex_usage_for_details(&sub2api_config, &codex) {
                    Ok(detailed_codex) => {
                        diagnose::log("usage detail poll succeeded");
                        apply_codex_poll_success(hwnd, detailed_codex);
                    }
                    Err(e) => {
                        diagnose::log(format!("usage detail poll failed: {e:?}"));
                    }
                }
                return;
            }
            Err(e) => {
                diagnose::log(format!("quick usage poll failed: {e:?}"));
            }
        }
    }

    match poller::poll_with_mode(
        show_claude_code,
        show_codex,
        usage_mode,
        Some(&sub2api_config),
    ) {
        Ok(data) => {
            diagnose::log("usage poll succeeded");
            apply_poll_success(hwnd, data);
        }
        Err(e) => {
            diagnose::log(format!("usage poll failed: {e:?}"));
            let auth_watch = match e {
                poller::PollError::AuthRequired | poller::PollError::TokenExpired => Some((
                    poller::CredentialWatchMode::ActiveSource,
                    poller::credential_watch_snapshot(poller::CredentialWatchMode::ActiveSource),
                )),
                poller::PollError::NoCredentials => Some((
                    poller::CredentialWatchMode::AllSources,
                    poller::credential_watch_snapshot(poller::CredentialWatchMode::AllSources),
                )),
                poller::PollError::RequestFailed => None,
            };
            // Distinguish auth-required errors from transient errors.
            let notify_auth_error = {
                let mut state = lock_state();
                let mut should_notify = false;
                if let Some(s) = state.as_mut() {
                    s.last_poll_ok = false;
                    match auth_watch {
                        Some((watch_mode, watch_snapshot)) => {
                            // Only show the balloon on the first failure so it doesn't spam.
                            if s.retry_count == 0 || s.force_notify_auth_error {
                                should_notify = true;
                            }
                            s.force_notify_auth_error = false;
                            s.auth_error_paused_polling = true;
                            s.auth_watch_mode = watch_mode;
                            s.auth_watch_snapshot = watch_snapshot;
                            s.session_text = "!".to_string();
                            s.weekly_text = "!".to_string();
                            s.codex_session_text = "!".to_string();
                            s.codex_weekly_text = "!".to_string();
                            s.retry_count = s.retry_count.saturating_add(1);
                            unsafe {
                                let _ = KillTimer(hwnd, TIMER_POLL);
                                let _ = KillTimer(hwnd, TIMER_RESET_POLL);
                                let _ = KillTimer(hwnd, TIMER_COUNTDOWN);
                                SetTimer(hwnd, TIMER_POLL, s.poll_interval_ms, None);
                            }
                        }
                        _ => {
                            // Transient network / credential-missing errors: exponential backoff.
                            s.force_notify_auth_error = false;
                            s.auth_error_paused_polling = false;
                            s.auth_watch_mode = poller::CredentialWatchMode::ActiveSource;
                            s.auth_watch_snapshot.clear();
                            s.session_text = "...".to_string();
                            s.weekly_text = "...".to_string();
                            s.codex_session_text = "...".to_string();
                            s.codex_weekly_text = "...".to_string();
                            s.retry_count = s.retry_count.saturating_add(1);
                            let backoff = RETRY_BASE_MS.saturating_mul(
                                1u32.checked_shl(s.retry_count - 1).unwrap_or(u32::MAX),
                            );
                            let retry_ms = backoff.min(s.poll_interval_ms);
                            unsafe {
                                let _ = KillTimer(hwnd, TIMER_RESET_POLL);
                                SetTimer(hwnd, TIMER_POLL, retry_ms, None);
                            }
                        }
                    }
                }
                should_notify
            };

            if notify_auth_error {
                let balloon = {
                    let state = lock_state();
                    state.as_ref().map(|s| {
                        if s.show_claude_code {
                            (
                                s.language.strings(),
                                tray_icon::TrayIconKind::Claude,
                                s.language.strings().token_expired_title,
                                s.language.strings().token_expired_body,
                            )
                        } else {
                            (
                                s.language.strings(),
                                tray_icon::TrayIconKind::Codex,
                                s.language.strings().codex_token_expired_title,
                                s.language.strings().codex_token_expired_body,
                            )
                        }
                    })
                };
                if let Some((_strings, kind, title, body)) = balloon {
                    tray_icon::notify_balloon(hwnd, kind, title, body);
                }
            }

            unsafe {
                let _ = PostMessageW(hwnd, WM_APP_USAGE_UPDATED, WPARAM(0), LPARAM(0));
            }
        }
    }
}

fn apply_codex_quick_poll_success(hwnd: HWND, mut codex: UsageData) {
    let previous_details = {
        let state = lock_state();
        state
            .as_ref()
            .and_then(|s| s.data.as_ref())
            .and_then(|data| data.codex.as_ref())
            .map(|codex| codex.account_details.clone())
            .unwrap_or_default()
    };
    preserve_account_detail_stats(&mut codex.account_details, &previous_details);
    apply_codex_poll_success(hwnd, codex);
}

fn preserve_account_detail_stats(
    new_details: &mut [AccountUsageDetail],
    previous_details: &[AccountUsageDetail],
) {
    for detail in new_details.iter_mut() {
        if let Some(previous) = previous_details
            .iter()
            .find(|previous| previous.id == detail.id)
        {
            detail.session_requests = previous.session_requests;
            detail.session_tokens = previous.session_tokens;
            detail.session_cost = previous.session_cost;
            detail.weekly_requests = previous.weekly_requests;
            detail.weekly_tokens = previous.weekly_tokens;
            detail.weekly_cost = previous.weekly_cost;
        }
    }
}

fn apply_codex_poll_success(hwnd: HWND, codex: UsageData) {
    let mut data = {
        let state = lock_state();
        state
            .as_ref()
            .and_then(|s| s.data.clone())
            .unwrap_or_default()
    };
    data.codex = Some(codex);
    apply_poll_success(hwnd, data);
}

fn apply_poll_success(hwnd: HWND, data: AppUsageData) {
    let mut state = lock_state();
    if let Some(s) = state.as_mut() {
        if let Some(claude_code) = data.claude_code.as_ref() {
            s.session_percent = claude_code.session.percentage;
            s.weekly_percent = claude_code.weekly.percentage;
        } else if s.show_claude_code {
            s.session_percent = 0.0;
            s.weekly_percent = 0.0;
        }
        if let Some(codex) = data.codex.as_ref() {
            s.codex_session_percent = codex.session.percentage;
            s.codex_weekly_percent = codex.weekly.percentage;
        } else if s.show_codex {
            s.codex_session_percent = 0.0;
            s.codex_weekly_percent = 0.0;
        }
        // Stop fast-poll if reset data is now fresh
        if !poller::app_is_past_reset(&data) {
            unsafe {
                let _ = KillTimer(hwnd, TIMER_RESET_POLL);
            }
        }

        s.data = Some(data);
        s.last_poll_ok = true;
        refresh_usage_texts(s);

        // Recovered from errors — restore normal poll interval
        if s.retry_count > 0 {
            s.retry_count = 0;
            let interval = s.poll_interval_ms;
            unsafe {
                SetTimer(hwnd, TIMER_POLL, interval, None);
            }
        }
        s.force_notify_auth_error = false;
        s.auth_error_paused_polling = false;
        s.auth_watch_mode = poller::CredentialWatchMode::ActiveSource;
        s.auth_watch_snapshot.clear();
    }
    drop(state);
    save_state_settings();

    unsafe {
        let _ = PostMessageW(hwnd, WM_APP_USAGE_UPDATED, WPARAM(0), LPARAM(0));
    }
}

fn refresh_sub2api_groups(send_hwnd: SendHwnd) {
    let should_start = {
        let mut state = lock_state();
        match state.as_mut() {
            Some(s) if !s.sub2api_groups_loading => {
                s.sub2api_groups_loading = true;
                true
            }
            _ => false,
        }
    };
    if !should_start {
        return;
    }

    std::thread::spawn(move || {
        let hwnd = send_hwnd.to_hwnd();
        let config = load_settings().sub2api;
        let result = poller::fetch_sub2api_groups(&config);
        {
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.sub2api_groups_loading = false;
                match result {
                    Ok(groups) => {
                        diagnose::log(format!("Sub2API group list refreshed: {}", groups.len()));
                        s.sub2api_groups = groups;
                        s.sub2api_groups_loaded = true;
                    }
                    Err(error) => {
                        diagnose::log(format!("Sub2API group list refresh failed: {error:?}"));
                    }
                }
            }
        }
        unsafe {
            let _ = PostMessageW(hwnd, WM_APP_SUB2API_GROUPS_UPDATED, WPARAM(0), LPARAM(0));
        }
    });
}

fn schedule_countdown_timer() {
    let state = lock_state();
    let s = match state.as_ref() {
        Some(s) => s,
        None => return,
    };

    let hwnd = s.hwnd.to_hwnd();
    if !s.last_poll_ok {
        unsafe {
            let _ = KillTimer(hwnd, TIMER_COUNTDOWN);
            let _ = KillTimer(hwnd, TIMER_RESET_POLL);
        }
        return;
    }

    let data = match &s.data {
        Some(d) => d,
        None => return,
    };

    // If a reset time has passed, poll every 5s to pick up fresh data
    if poller::app_is_past_reset(data) {
        unsafe {
            SetTimer(hwnd, TIMER_RESET_POLL, 5_000, None);
        }
    }

    let delays = [
        data.claude_code
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.session.resets_at)),
        data.claude_code
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.weekly.resets_at)),
        data.codex
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.session.resets_at)),
        data.codex
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.weekly.resets_at)),
    ];
    let min_delay = delays.into_iter().flatten().min();

    let ms = min_delay
        .unwrap_or(Duration::from_secs(60))
        .as_millis()
        .max(1000) as u32;

    unsafe {
        SetTimer(hwnd, TIMER_COUNTDOWN, ms, None);
    }
}

fn check_theme_change() {
    let new_dark = theme::is_dark_mode();
    let changed = {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            if s.is_dark != new_dark {
                s.is_dark = new_dark;
                true
            } else {
                false
            }
        } else {
            false
        }
    };
    if changed {
        render_layered();
    }
}

fn refresh_widget_layout() {
    position_at_taskbar();
    render_layered();
}

fn refresh_widget_theme_and_layout() {
    refresh_widget_layout();
}

fn resize_widget_at_current_position(hwnd: HWND) {
    let (width, height, placement_mode, edge_collapse, edge_expanded) = {
        let state = lock_state();
        let Some(s) = state.as_ref() else {
            return;
        };
        (
            total_widget_width_for_state(s),
            widget_height_for_state(s),
            s.placement_mode,
            s.edge_collapse,
            s.edge_expanded,
        )
    };

    if placement_mode != PlacementMode::Floating {
        position_at_taskbar();
        return;
    }

    let rect = native_interop::get_window_rect_safe(hwnd);
    let work = work_area_rect();
    let (mut x, y) = if let Some(rect) = rect {
        (
            rect.left.clamp(
                work.left + sc(4),
                (work.right - width - sc(4)).max(work.left + sc(4)),
            ),
            rect.top.clamp(
                work.top + sc(4),
                (work.bottom - height - sc(4)).max(work.top + sc(4)),
            ),
        )
    } else {
        (work.right - width - sc(18), work.top + sc(18))
    };
    if edge_collapse != EdgeCollapse::None {
        x = if edge_expanded {
            expanded_floating_x(edge_collapse, &work, width)
        } else {
            collapsed_floating_x(edge_collapse, &work, width)
        };
    }
    native_interop::move_window(hwnd, x, y, width, height);
    ensure_floating_topmost(hwnd);
}

fn check_language_change() {
    if update_language_change() {
        render_layered();
    }
}

fn update_display() {
    let mut state = lock_state();
    let s = match state.as_mut() {
        Some(s) => s,
        None => return,
    };

    // Don't overwrite error text with stale cached data
    if !s.last_poll_ok {
        return;
    }

    refresh_usage_texts(s);
}

fn suppress_tray_reposition_for(duration: Duration) {
    let mut until = SUPPRESS_TRAY_REPOSITION_UNTIL
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    *until = Some(Instant::now() + duration);
}

fn tray_reposition_is_suppressed() -> bool {
    let now = Instant::now();
    let mut until = SUPPRESS_TRAY_REPOSITION_UNTIL
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    match *until {
        Some(deadline) if now < deadline => true,
        Some(_) => {
            *until = None;
            false
        }
        None => false,
    }
}

fn position_at_taskbar() {
    refresh_dpi();
    // Drop the app-state lock before any Win32 call that may synchronously
    // re-enter our window procedure.
    let (
        hwnd,
        was_embedded,
        tray_offset,
        taskbar_hwnd,
        placement_mode,
        floating_x,
        floating_y,
        edge_collapse,
        edge_expanded,
        detail_visible,
        detail_rows,
    ) = {
        let state = lock_state();
        let s = match state.as_ref() {
            Some(s) => s,
            None => return,
        };

        // Don't fight the user's drag
        if s.dragging {
            return;
        }

        let taskbar_hwnd = match s.taskbar_hwnd {
            Some(h) => h,
            None => {
                diagnose::log("position_at_taskbar skipped: no taskbar handle");
                return;
            }
        };

        (
            s.hwnd.to_hwnd(),
            s.embedded,
            s.tray_offset,
            taskbar_hwnd,
            s.placement_mode,
            s.floating_x,
            s.floating_y,
            s.edge_collapse,
            s.edge_expanded,
            s.detail_visible,
            detail_row_count_for_state(s),
        )
    };

    let taskbar_rect = match native_interop::get_taskbar_rect(taskbar_hwnd) {
        Some(r) => r,
        None => {
            diagnose::log("position_at_taskbar skipped: unable to query taskbar rect");
            return;
        }
    };

    let taskbar_height = taskbar_rect.bottom - taskbar_rect.top;
    let mut tray_left = taskbar_rect.right;
    let mut task_buttons_right = taskbar_rect.left;
    let anchor_top = taskbar_rect.top;
    let anchor_height = taskbar_height;

    if let Some(tray_hwnd) = native_interop::find_child_window(taskbar_hwnd, "TrayNotifyWnd") {
        if let Some(tray_rect) = native_interop::get_window_rect_safe(tray_hwnd) {
            tray_left = tray_rect.left;
        }
    }
    if let Some(tasks_hwnd) =
        native_interop::find_descendant_window(taskbar_hwnd, "MSTaskListWClass")
    {
        if let Some(tasks_rect) = native_interop::get_window_rect_safe(tasks_hwnd) {
            task_buttons_right = tasks_rect.right;
        }
    }

    let widget_width = total_widget_width();
    let widget_height = current_widget_height();
    let requested_x = tray_left - widget_width - tray_offset;
    diagnose::log(format!(
        "widget placement mode={placement_mode:?} tray_left={tray_left} task_buttons_right={task_buttons_right} requested_x={requested_x} widget_width={widget_width}"
    ));

    if placement_mode == PlacementMode::Floating {
        if was_embedded {
            native_interop::float_as_topmost(hwnd);
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.embedded = false;
            }
        }

        let work = work_area_rect();
        let default_x = (tray_left - widget_width).max(work.left + sc(FLOATING_EDGE_MARGIN));
        let default_y =
            (taskbar_rect.top - widget_height - sc(10)).max(work.top + sc(FLOATING_EDGE_MARGIN));
        let (mut x, y) = clamp_floating_position(
            floating_x.unwrap_or(default_x),
            floating_y.unwrap_or(default_y),
            widget_width,
            widget_height,
            &work,
        );
        if edge_collapse != EdgeCollapse::None {
            x = if edge_expanded {
                expanded_floating_x(edge_collapse, &work, widget_width)
            } else {
                collapsed_floating_x(edge_collapse, &work, widget_width)
            };
        }
        native_interop::move_window(hwnd, x, y, widget_width, widget_height);
        ensure_floating_topmost(hwnd);
        diagnose::log(format!(
            "positioned floating widget at x={x} y={y} w={widget_width} h={widget_height}"
        ));
    } else if detail_visible && placement_mode == PlacementMode::Floating {
        if was_embedded {
            native_interop::float_as_topmost(hwnd);
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.embedded = false;
            }
        }

        let y = taskbar_rect.top - detail_panel_height_for_rows(detail_rows);
        native_interop::move_window(hwnd, requested_x, y, widget_width, widget_height);
        diagnose::log(format!(
            "positioned fixed detail widget at x={requested_x} y={y} w={widget_width} h={widget_height}"
        ));
    } else {
        if !was_embedded {
            native_interop::embed_in_taskbar(hwnd, taskbar_hwnd);
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.embedded = true;
            }
        }

        let y = compute_anchor_y(anchor_top, anchor_height, widget_height);
        // Child window: coordinates relative to parent (taskbar)
        let x = requested_x - taskbar_rect.left;
        native_interop::move_window(hwnd, x, y - taskbar_rect.top, widget_width, widget_height);
        diagnose::log(format!(
            "positioned embedded widget at x={x} y={} w={widget_width} h={widget_height}",
            y - taskbar_rect.top
        ));
    }
}

fn compute_anchor_y(anchor_top: i32, anchor_height: i32, widget_height: i32) -> i32 {
    let anchor_bottom = anchor_top + anchor_height;
    (anchor_bottom - widget_height).max(anchor_top)
}

/// WinEvent callback for tray icon location changes
unsafe extern "system" fn on_tray_location_changed(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    static LAST_REPOSITION: Mutex<Option<std::time::Instant>> = Mutex::new(None);

    let is_tray = {
        let state = lock_state();
        state
            .as_ref()
            .and_then(|s| s.tray_notify_hwnd)
            .map(|h| h == hwnd)
            .unwrap_or(false)
    };

    if is_tray {
        if tray_reposition_is_suppressed() {
            return;
        }

        let should_reposition = {
            let mut last = LAST_REPOSITION.lock().unwrap_or_else(|e| e.into_inner());
            let now = std::time::Instant::now();
            if last
                .map(|t| now.duration_since(t).as_millis() > 500)
                .unwrap_or(true)
            {
                *last = Some(now);
                true
            } else {
                false
            }
        };
        if should_reposition {
            position_at_taskbar();
            render_layered();
        }
    }
}

/// Main window procedure
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            // For non-embedded fallback, paint normally
            let embedded = {
                let state = lock_state();
                state.as_ref().map(|s| s.embedded).unwrap_or(false)
            };
            if embedded {
                // Layered windows don't use WM_PAINT; just validate the region
                let mut ps = PAINTSTRUCT::default();
                let _ = BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
            } else {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                paint(hdc, hwnd);
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_DISPLAYCHANGE | WM_DPICHANGED_MSG | WM_SETTINGCHANGE => {
            if msg == WM_DPICHANGED_MSG {
                let new_dpi = (wparam.0 & 0xFFFF) as u32;
                CURRENT_DPI.store(new_dpi, Ordering::Relaxed);
            }
            if msg == WM_SETTINGCHANGE {
                check_theme_change();
                check_language_change();
            }
            refresh_dpi();
            position_at_taskbar();
            render_layered();
            LRESULT(0)
        }
        WM_TIMER => {
            let timer_id = wparam.0;
            match timer_id {
                TIMER_POLL => {
                    let auth_watch = {
                        let state = lock_state();
                        state.as_ref().map(|s| {
                            (
                                s.auth_error_paused_polling,
                                s.auth_watch_mode,
                                s.auth_watch_snapshot.clone(),
                            )
                        })
                    };
                    match auth_watch {
                        Some((true, watch_mode, previous_snapshot)) => {
                            let current_snapshot = poller::credential_watch_snapshot(watch_mode);
                            if current_snapshot != previous_snapshot {
                                let mut state = lock_state();
                                if let Some(s) = state.as_mut() {
                                    if s.auth_error_paused_polling
                                        && s.auth_watch_mode == watch_mode
                                    {
                                        s.auth_watch_snapshot = current_snapshot;
                                    }
                                }
                                drop(state);
                                let sh = SendHwnd::from_hwnd(hwnd);
                                std::thread::spawn(move || {
                                    do_poll(sh);
                                });
                            }
                        }
                        Some((false, _, _)) => {
                            let sh = SendHwnd::from_hwnd(hwnd);
                            std::thread::spawn(move || {
                                do_poll(sh);
                            });
                        }
                        None => {}
                    }
                }
                TIMER_COUNTDOWN => {
                    update_display();
                    render_layered();
                    schedule_countdown_timer();
                }
                TIMER_RESET_POLL => {
                    let should_poll = {
                        let state = lock_state();
                        state
                            .as_ref()
                            .map(|s| !s.auth_error_paused_polling)
                            .unwrap_or(false)
                    };
                    if should_poll {
                        let sh = SendHwnd::from_hwnd(hwnd);
                        std::thread::spawn(move || {
                            do_poll(sh);
                        });
                    }
                }
                TIMER_UPDATE_CHECK => {
                    begin_update_check(hwnd, false);
                }
                TIMER_EDGE_ANIMATION => {
                    tick_edge_animation(hwnd);
                }
                TIMER_DETAIL_OUTSIDE => {
                    close_detail_if_clicked_outside();
                }
                TIMER_DETAIL_RENDER => {
                    let _ = KillTimer(hwnd, TIMER_DETAIL_RENDER);
                    render_layered();
                }
                TIMER_PET_RENDER => {
                    tick_pet_render(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_APP_USAGE_UPDATED => {
            check_theme_change();
            check_language_change();
            refresh_widget_theme_and_layout();
            schedule_countdown_timer();
            suppress_tray_reposition_for(Duration::from_millis(
                TRAY_ICON_UPDATE_REPOSITION_SUPPRESS_MS,
            ));
            sync_tray_icons(hwnd);
            invalidate_detail_window();
            LRESULT(0)
        }
        WM_APP_UPDATE_CHECK_COMPLETE => {
            schedule_auto_update_check(hwnd);
            LRESULT(0)
        }
        WM_APP_SUB2API_GROUPS_UPDATED => LRESULT(0),
        WM_SETCURSOR => {
            let (is_dragging, placement_mode) = {
                let state = lock_state();
                state
                    .as_ref()
                    .map(|s| (s.dragging, s.placement_mode))
                    .unwrap_or((false, PlacementMode::Fixed))
            };
            // Always show resize cursor while dragging or when hovering divider zone
            let hit_test = (lparam.0 & 0xFFFF) as u16;
            if is_dragging {
                let cursor = LoadCursorW(
                    HINSTANCE::default(),
                    if placement_mode == PlacementMode::Floating {
                        IDC_SIZEALL
                    } else {
                        IDC_SIZEWE
                    },
                )
                .unwrap_or_default();
                SetCursor(cursor);
                return LRESULT(1);
            }
            if hit_test == 1 {
                if placement_mode == PlacementMode::Floating {
                    let cursor = LoadCursorW(HINSTANCE::default(), IDC_SIZEALL).unwrap_or_default();
                    SetCursor(cursor);
                    return LRESULT(1);
                }
                // HTCLIENT
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let _ = ScreenToClient(hwnd, &mut pt);
                if pt.x < sc(DIVIDER_HIT_ZONE) {
                    let cursor = LoadCursorW(HINSTANCE::default(), IDC_SIZEWE).unwrap_or_default();
                    SetCursor(cursor);
                    return LRESULT(1);
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONDOWN => {
            let client_x = (lparam.0 & 0xFFFF) as i16 as i32;
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                if s.placement_mode == PlacementMode::Floating || client_x < sc(DIVIDER_HIT_ZONE) {
                    s.dragging = true;
                    s.drag_start_mouse_x = pt.x;
                    s.drag_start_mouse_y = pt.y;
                    s.drag_start_offset = s.tray_offset;
                    s.drag_start_floating_x = s.floating_x.unwrap_or(pt.x);
                    s.drag_start_floating_y = s.floating_y.unwrap_or(pt.y);
                    if s.placement_mode == PlacementMode::Floating {
                        if let Some(rect) = native_interop::get_window_rect_safe(hwnd) {
                            s.drag_start_floating_x = rect.left;
                            s.drag_start_floating_y = rect.top;
                        }
                        if s.edge_collapse != EdgeCollapse::None {
                            let work = work_area_rect();
                            let full_width = full_floating_widget_width_for_state(s);
                            s.drag_start_floating_x = match s.edge_collapse {
                                EdgeCollapse::Left => work.left + sc(FLOATING_EDGE_MARGIN),
                                EdgeCollapse::Right => {
                                    work.right - full_width - sc(FLOATING_EDGE_MARGIN)
                                }
                                EdgeCollapse::None => s.drag_start_floating_x,
                            };
                        }
                    }
                    SetCapture(hwnd);
                }
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let (is_dragging, should_expand, should_redraw_detail_hover) = {
                let state = lock_state();
                state
                    .as_ref()
                    .map(|s| {
                        (
                            s.dragging,
                            s.placement_mode == PlacementMode::Floating
                                && s.edge_collapse != EdgeCollapse::None
                                && !s.edge_expanded
                                && !s.dragging
                                && !s.detail_visible,
                            s.detail_visible && !s.dragging,
                        )
                    })
                    .unwrap_or((false, false, false))
            };
            if should_expand {
                let edge = {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        if !s.mouse_tracking {
                            track_mouse_leave(hwnd);
                            s.mouse_tracking = true;
                        }
                        s.edge_collapse
                    } else {
                        EdgeCollapse::None
                    }
                };
                start_edge_animation(hwnd, edge, true);
            }
            if is_dragging {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let mut needs_render = false;
                let move_target = {
                    let mut state = lock_state();
                    let s = match state.as_mut() {
                        Some(s) => s,
                        None => return LRESULT(0),
                    };

                    if s.placement_mode == PlacementMode::Floating {
                        let hwnd_val = s.hwnd.to_hwnd();
                        let full_width = full_floating_widget_width_for_state(s);
                        let widget_height = widget_height_for_state(s);
                        let desired_x = s.drag_start_floating_x + (pt.x - s.drag_start_mouse_x);
                        let desired_y = s.drag_start_floating_y + (pt.y - s.drag_start_mouse_y);
                        let work = work_area_rect();
                        let edge = floating_edge_for_position(desired_x, full_width, &work);
                        let widget_width = full_width;
                        let (mut new_x, new_y) = clamp_floating_position(
                            desired_x,
                            desired_y,
                            widget_width,
                            widget_height,
                            &work,
                        );
                        if edge != EdgeCollapse::None {
                            new_x = expanded_floating_x(edge, &work, widget_width);
                        }
                        s.floating_x = Some(new_x);
                        s.floating_y = Some(new_y);
                        if s.edge_collapse != edge {
                            s.edge_collapse = edge;
                            s.edge_expanded = edge != EdgeCollapse::None;
                            needs_render = true;
                        }
                        Some((
                            hwnd_val,
                            false,
                            new_x,
                            new_y,
                            0,
                            widget_width,
                            widget_height,
                        ))
                    } else {
                        // Moving mouse left = positive delta = larger offset (further left)
                        let delta = s.drag_start_mouse_x - pt.x;
                        let mut new_offset = s.drag_start_offset + delta;

                        // Clamp: offset >= 0 (can't go right of default)
                        if new_offset < 0 {
                            new_offset = 0;
                        }

                        let taskbar_hwnd = s.taskbar_hwnd;
                        let embedded = s.embedded;
                        let hwnd_val = s.hwnd.to_hwnd();

                        // Clamp: don't go past left edge of taskbar
                        if let Some(taskbar_hwnd) = taskbar_hwnd {
                            if let Some(taskbar_rect) =
                                native_interop::get_taskbar_rect(taskbar_hwnd)
                            {
                                let mut tray_left = taskbar_rect.right;
                                if let Some(tray_hwnd) =
                                    native_interop::find_child_window(taskbar_hwnd, "TrayNotifyWnd")
                                {
                                    if let Some(tray_rect) =
                                        native_interop::get_window_rect_safe(tray_hwnd)
                                    {
                                        tray_left = tray_rect.left;
                                    }
                                }
                                let mut task_buttons_right = taskbar_rect.left;
                                if let Some(tasks_hwnd) = native_interop::find_descendant_window(
                                    taskbar_hwnd,
                                    "MSTaskListWClass",
                                ) {
                                    if let Some(tasks_rect) =
                                        native_interop::get_window_rect_safe(tasks_hwnd)
                                    {
                                        task_buttons_right = tasks_rect.right;
                                    }
                                }

                                let widget_width = total_widget_width_for_state(s);
                                let taskbar_gap = sc(6);
                                let max_offset =
                                    (tray_left - task_buttons_right - taskbar_gap - widget_width)
                                        .max(0);
                                if new_offset > max_offset {
                                    new_offset = max_offset;
                                }

                                s.tray_offset = new_offset;

                                let taskbar_height = taskbar_rect.bottom - taskbar_rect.top;
                                let anchor_top = taskbar_rect.top;
                                let anchor_height = taskbar_height;
                                let widget_height = sc(WIDGET_HEIGHT);
                                let y = compute_anchor_y(anchor_top, anchor_height, widget_height);
                                let x = if embedded {
                                    tray_left - taskbar_rect.left - widget_width - new_offset
                                } else {
                                    tray_left - widget_width - new_offset
                                };
                                let y = if embedded {
                                    y
                                } else {
                                    (taskbar_rect.top - widget_height - sc(2)).max(0)
                                };
                                Some((
                                    hwnd_val,
                                    embedded,
                                    x,
                                    y,
                                    taskbar_rect.top,
                                    widget_width,
                                    widget_height,
                                ))
                            } else {
                                s.tray_offset = new_offset;
                                None
                            }
                        } else {
                            s.tray_offset = new_offset;
                            None
                        }
                    }
                };

                if needs_render {
                    render_layered();
                }
                if let Some((hwnd_val, embedded, x, y, taskbar_top, widget_width, widget_height)) =
                    move_target
                {
                    if embedded {
                        native_interop::move_window(
                            hwnd_val,
                            x,
                            y - taskbar_top,
                            widget_width,
                            widget_height,
                        );
                    } else {
                        native_interop::move_window(hwnd_val, x, y, widget_width, widget_height);
                    }
                }
            } else if should_redraw_detail_hover {
                render_layered();
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE_MSG => {
            let edge = {
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    s.mouse_tracking = false;
                    if s.placement_mode == PlacementMode::Floating
                        && s.edge_collapse != EdgeCollapse::None
                        && s.edge_expanded
                        && !s.dragging
                        && !s.detail_visible
                    {
                        s.edge_collapse
                    } else {
                        EdgeCollapse::None
                    }
                } else {
                    EdgeCollapse::None
                }
            };
            if edge != EdgeCollapse::None {
                start_edge_animation(hwnd, edge, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (was_dragging, should_track_leave, should_toggle_detail) = {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    if s.dragging {
                        let moved = (pt.x - s.drag_start_mouse_x).abs()
                            + (pt.y - s.drag_start_mouse_y).abs();
                        s.dragging = false;
                        let offset = s.tray_offset;
                        (
                            Some(offset),
                            s.placement_mode == PlacementMode::Floating
                                && s.edge_collapse != EdgeCollapse::None
                                && s.edge_expanded,
                            moved <= sc(4),
                        )
                    } else {
                        (None, false, true)
                    }
                } else {
                    (None, false, false)
                }
            };
            if was_dragging.is_some() {
                let _ = ReleaseCapture();
                let placement_mode = {
                    let state = lock_state();
                    state.as_ref().map(|s| s.placement_mode)
                };
                if placement_mode == Some(PlacementMode::Floating) {
                    ensure_floating_topmost(hwnd);
                }
                if should_track_leave {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        if !s.mouse_tracking {
                            track_mouse_leave(hwnd);
                            s.mouse_tracking = true;
                        }
                    }
                }
                save_state_settings();
            }
            if should_toggle_detail {
                toggle_detail_window(hwnd);
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            show_context_menu(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = wparam.0 as u16;
            match id {
                1 => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.force_notify_auth_error = true;
                        }
                    }
                    let sh = SendHwnd::from_hwnd(hwnd);
                    std::thread::spawn(move || {
                        do_poll(sh);
                    });
                }
                IDM_VERSION_ACTION => {
                    let (install_channel, release) = {
                        let state = lock_state();
                        match state.as_ref() {
                            Some(s) => (
                                s.install_channel,
                                match &s.update_status {
                                    UpdateStatus::Available(release) => Some(release.clone()),
                                    _ => None,
                                },
                            ),
                            None => (InstallChannel::Portable, None),
                        }
                    };

                    match install_channel {
                        InstallChannel::Winget => {
                            if release.is_some() {
                                begin_winget_update(hwnd);
                            } else {
                                begin_update_check(hwnd, true);
                            }
                        }
                        InstallChannel::Portable => {
                            if let Some(release) = release {
                                begin_update_apply(hwnd, release);
                            } else {
                                begin_update_check(hwnd, true);
                            }
                        }
                    }
                }
                2 => {
                    let hook = {
                        let state = lock_state();
                        state.as_ref().and_then(|s| s.win_event_hook)
                    };
                    if let Some(h) = hook {
                        native_interop::unhook_win_event(h);
                    }
                    PostQuitMessage(0);
                }
                IDM_RESET_POSITION => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.tray_offset = 0;
                            s.floating_x = None;
                            s.floating_y = None;
                            s.edge_collapse = EdgeCollapse::None;
                        }
                    }
                    save_state_settings();
                    position_at_taskbar();
                }
                IDM_START_WITH_WINDOWS => {
                    set_startup_enabled(!is_startup_enabled());
                }
                IDM_FREQ_10SEC | IDM_FREQ_30SEC | IDM_FREQ_1MIN | IDM_FREQ_5MIN
                | IDM_FREQ_15MIN | IDM_FREQ_1HOUR => {
                    let new_interval = match id {
                        IDM_FREQ_10SEC => POLL_10_SEC,
                        IDM_FREQ_30SEC => POLL_30_SEC,
                        IDM_FREQ_1MIN => POLL_1_MIN,
                        IDM_FREQ_5MIN => POLL_5_MIN,
                        IDM_FREQ_15MIN => POLL_15_MIN,
                        IDM_FREQ_1HOUR => POLL_1_HOUR,
                        _ => POLL_15_MIN,
                    };
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.poll_interval_ms = new_interval;
                        }
                    }
                    save_state_settings();
                    // Reset the poll timer with the new interval
                    SetTimer(hwnd, TIMER_POLL, new_interval, None);
                }
                IDM_MODEL_CLAUDE_CODE | IDM_MODEL_CODEX => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.show_claude_code = false;
                            s.show_codex = true;
                            s.session_text = "...".to_string();
                            s.weekly_text = "...".to_string();
                            s.codex_session_text = "...".to_string();
                            s.codex_weekly_text = "...".to_string();
                        }
                    }
                    save_state_settings();
                    position_at_taskbar();
                    render_layered();
                    sync_tray_icons(hwnd);
                    let sh = SendHwnd::from_hwnd(hwnd);
                    std::thread::spawn(move || {
                        do_poll(sh);
                    });
                }
                IDM_PLACEMENT_FIXED | IDM_PLACEMENT_FLOATING => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.placement_mode = if id == IDM_PLACEMENT_FLOATING {
                                PlacementMode::Floating
                            } else {
                                PlacementMode::Fixed
                            };
                            if s.placement_mode == PlacementMode::Fixed {
                                s.edge_collapse = EdgeCollapse::None;
                            }
                            s.detail_closing = false;
                            s.pet_transition = None;
                        }
                    }
                    unsafe {
                        let _ = KillTimer(hwnd, TIMER_PET_RENDER);
                    }
                    save_state_settings();
                    position_at_taskbar();
                    render_layered();
                }
                IDM_SEASON_AUTO..=IDM_SEASON_WINTER => {
                    let selected = [
                        crate::pixel_room::Season::Auto,
                        crate::pixel_room::Season::Spring,
                        crate::pixel_room::Season::Summer,
                        crate::pixel_room::Season::Autumn,
                        crate::pixel_room::Season::Winter,
                    ][(id - IDM_SEASON_AUTO) as usize];
                    if let Some(s) = lock_state().as_mut() {
                        s.season = selected;
                    }
                    save_state_settings();
                    render_layered();
                }
                IDM_THEME_LIGHT | IDM_THEME_DARK => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.widget_theme = if id == IDM_THEME_LIGHT {
                                WidgetTheme::Light
                            } else {
                                WidgetTheme::Dark
                            };
                        }
                    }
                    save_state_settings();
                    refresh_widget_theme_and_layout();
                }
                IDM_COLOR_MONO | IDM_COLOR_ANTD => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.progress_color_mode = if id == IDM_COLOR_ANTD {
                                ProgressColorMode::Antd
                            } else {
                                ProgressColorMode::Mono
                            };
                        }
                    }
                    save_state_settings();
                    render_layered();
                }
                IDM_GROUP_REFRESH => {
                    refresh_sub2api_groups(SendHwnd::from_hwnd(hwnd));
                }
                IDM_GROUP_BASE..=IDM_GROUP_MAX => {
                    let idx = (id - IDM_GROUP_BASE) as usize;
                    let selected = {
                        let state = lock_state();
                        state
                            .as_ref()
                            .and_then(|s| s.sub2api_groups.get(idx).cloned())
                    };
                    if let Some(group) = selected {
                        let mut settings = load_settings();
                        settings.sub2api.group_id = Some(group.id);
                        settings.sub2api.group_name = group.name;
                        settings.cached_usage = None;
                        save_settings(&settings);
                        {
                            let mut state = lock_state();
                            if let Some(s) = state.as_mut() {
                                s.data = None;
                                s.last_poll_ok = true;
                                s.session_percent = 0.0;
                                s.weekly_percent = 0.0;
                                s.codex_session_percent = 0.0;
                                s.codex_weekly_percent = 0.0;
                                s.session_text = "...".to_string();
                                s.weekly_text = "...".to_string();
                                s.codex_session_text = "...".to_string();
                                s.codex_weekly_text = "...".to_string();
                                s.detail_visible = false;
                                s.detail_mouse_down_seen = false;
                            }
                        }
                        position_at_taskbar();
                        render_layered();
                        let sh = SendHwnd::from_hwnd(hwnd);
                        std::thread::spawn(move || {
                            do_poll(sh);
                        });
                    }
                }
                IDM_LANG_SYSTEM
                | IDM_LANG_ENGLISH
                | IDM_LANG_DUTCH
                | IDM_LANG_SPANISH
                | IDM_LANG_FRENCH
                | IDM_LANG_GERMAN
                | IDM_LANG_JAPANESE
                | IDM_LANG_KOREAN
                | IDM_LANG_SIMPLIFIED_CHINESE
                | IDM_LANG_TRADITIONAL_CHINESE
                | IDM_LANG_RUSSIAN
                | IDM_LANG_PORTUGUESE_BRAZIL => {
                    let language_override = match id {
                        IDM_LANG_SYSTEM => None,
                        IDM_LANG_ENGLISH => Some(LanguageId::English),
                        IDM_LANG_DUTCH => Some(LanguageId::Dutch),
                        IDM_LANG_SPANISH => Some(LanguageId::Spanish),
                        IDM_LANG_FRENCH => Some(LanguageId::French),
                        IDM_LANG_GERMAN => Some(LanguageId::German),
                        IDM_LANG_JAPANESE => Some(LanguageId::Japanese),
                        IDM_LANG_KOREAN => Some(LanguageId::Korean),
                        IDM_LANG_SIMPLIFIED_CHINESE => Some(LanguageId::SimplifiedChinese),
                        IDM_LANG_TRADITIONAL_CHINESE => Some(LanguageId::TraditionalChinese),
                        IDM_LANG_RUSSIAN => Some(LanguageId::Russian),
                        IDM_LANG_PORTUGUESE_BRAZIL => Some(LanguageId::PortugueseBrazil),
                        _ => None,
                    };
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            apply_language_to_state(s, language_override);
                        }
                    }
                    save_state_settings();
                    render_layered();
                }
                id if id == tray_icon::IDM_TOGGLE_WIDGET => {
                    toggle_widget_visibility(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }
        _ if msg == WM_APP_TRAY => {
            match tray_icon::handle_message(lparam) {
                tray_icon::TrayAction::ToggleWidget => {
                    toggle_widget_visibility(hwnd);
                }
                tray_icon::TrayAction::ShowContextMenu => {
                    show_context_menu(hwnd);
                }
                tray_icon::TrayAction::None => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let hook = {
                let state = lock_state();
                state.as_ref().and_then(|s| s.win_event_hook)
            };
            if let Some(h) = hook {
                native_interop::unhook_win_event(h);
            }
            tray_icon::remove_all(hwnd);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn toggle_detail_window(owner: HWND) {
    let (visible, closing) = {
        let state = lock_state();
        state
            .as_ref()
            .map(|s| (s.detail_visible, s.detail_closing))
            .unwrap_or((false, false))
    };
    if visible && !closing {
        hide_detail_window();
    } else {
        show_detail_panel(owner);
    }
}

fn tick_pet_render(hwnd: HWND) {
    let transition = {
        let state = lock_state();
        state.as_ref().and_then(|s| {
            s.pet_transition.map(|transition| {
                (
                    transition.opening,
                    transition.start.elapsed().as_millis() >= PET_TRANSITION_MS,
                )
            })
        })
    };

    match transition {
        Some((false, true)) => {
            finalize_hide_detail_window(hwnd);
        }
        Some((true, true)) => {
            {
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    s.pet_transition = None;
                }
            }
            unsafe {
                SetTimer(hwnd, TIMER_PET_RENDER, 120, None);
            }
            render_layered();
        }
        Some((_, false)) => {
            render_layered();
        }
        None => {
            let active = {
                let state = lock_state();
                state
                    .as_ref()
                    .map(|s| s.detail_visible && s.placement_mode == PlacementMode::Floating)
                    .unwrap_or(false)
            };
            if active {
                render_layered();
            } else {
                unsafe {
                    let _ = KillTimer(hwnd, TIMER_PET_RENDER);
                }
            }
        }
    }
}

unsafe extern "system" fn detail_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rect = RECT::default();
            let _ = GetClientRect(hwnd, &mut rect);
            paint_detail_layered(hwnd, rect.right - rect.left, rect.bottom - rect.top);
            let _ = EndPaint(hwnd, &ps);
            let _ = hdc;
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_LBUTTONUP => {
            hide_detail_window();
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn hide_detail_window() {
    let (main_hwnd, should_animate) = {
        let mut state = lock_state();
        state
            .as_mut()
            .map(|s| {
                let should_animate = s.placement_mode == PlacementMode::Floating
                    && s.detail_visible
                    && s.pet_motion_enabled
                    && !s.detail_closing;
                if should_animate {
                    s.detail_closing = true;
                    s.pet_transition = Some(PetTransition {
                        opening: false,
                        start: Instant::now(),
                    });
                }
                s.detail_mouse_down_seen = false;
                (s.hwnd.to_hwnd(), should_animate)
            })
            .unwrap_or((HWND::default(), false))
    };
    if should_animate {
        unsafe {
            let _ = KillTimer(main_hwnd, TIMER_DETAIL_OUTSIDE);
            SetTimer(main_hwnd, TIMER_PET_RENDER, 30, None);
        }
        render_layered();
        return;
    }
    finalize_hide_detail_window(main_hwnd);
}

fn finalize_hide_detail_window(main_hwnd: HWND) {
    let detail_hwnd = {
        let mut state = lock_state();
        state
            .as_mut()
            .map(|s| {
                s.detail_visible = false;
                s.detail_closing = false;
                s.pet_transition = None;
                s.detail_mouse_down_seen = false;
                s.detail_hwnd.take().map(|h| h.to_hwnd())
            })
            .unwrap_or(None)
    };
    unsafe {
        let _ = KillTimer(main_hwnd, TIMER_DETAIL_OUTSIDE);
        let _ = KillTimer(main_hwnd, TIMER_DETAIL_RENDER);
        let _ = KillTimer(main_hwnd, TIMER_PET_RENDER);
        if let Some(hwnd) = detail_hwnd {
            let _ = DestroyWindow(hwnd);
        }
    }
    resize_widget_at_current_position(main_hwnd);
    render_layered();
}

fn invalidate_detail_window() {
    let hwnd = {
        let state = lock_state();
        state.as_ref().and_then(|s| {
            if s.detail_visible {
                s.detail_hwnd.map(|h| h.to_hwnd())
            } else {
                None
            }
        })
    };
    if let Some(hwnd) = hwnd {
        unsafe {
            let _ = InvalidateRect(hwnd, None, false);
        }
    }
}

fn detail_items() -> Vec<AccountUsageDetail> {
    let state = lock_state();
    state
        .as_ref()
        .and_then(|s| s.data.as_ref())
        .and_then(|data| data.codex.as_ref())
        .map(|codex| codex.account_details.clone())
        .unwrap_or_default()
}

fn show_detail_panel(owner: HWND) {
    diagnose::log("detail: show requested");
    let placement_mode = {
        let state = lock_state();
        state
            .as_ref()
            .map(|s| s.placement_mode)
            .unwrap_or(PlacementMode::Fixed)
    };
    {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            s.detail_visible = true;
            s.detail_closing = false;
            s.pet_animation_started = Instant::now();
            s.pet_action_seed = pet_random_seed();
            s.pet_transition = if placement_mode == PlacementMode::Floating && s.pet_motion_enabled
            {
                Some(PetTransition {
                    opening: true,
                    start: Instant::now(),
                })
            } else {
                None
            };
            s.detail_mouse_down_seen = true;
            s.edge_expanded = s.edge_collapse != EdgeCollapse::None;
            s.edge_animation = None;
        }
    }
    unsafe {
        let _ = KillTimer(owner, TIMER_EDGE_ANIMATION);
        SetTimer(owner, TIMER_DETAIL_OUTSIDE, 80, None);
        if placement_mode == PlacementMode::Floating {
            SetTimer(owner, TIMER_PET_RENDER, 30, None);
            SetTimer(owner, TIMER_DETAIL_RENDER, 30, None);
        }
    }
    if placement_mode == PlacementMode::Fixed {
        show_fixed_detail_popup(owner);
        render_layered();
    } else {
        resize_widget_at_current_position(owner);
        render_layered();
    }
    diagnose::log(format!(
        "detail: panel shown items={}",
        detail_items().len()
    ));
}

fn show_fixed_detail_popup(owner: HWND) {
    let (detail_width, detail_height, detail_rows) = {
        let state = lock_state();
        let rows = state.as_ref().map(detail_row_count_for_state).unwrap_or(1);
        (
            sc(DETAIL_PANEL_WIDTH),
            detail_panel_height_for_rows(rows),
            rows,
        )
    };
    let Some(main_rect) = native_interop::get_window_rect_safe(owner) else {
        return;
    };
    let x = (main_rect.right - detail_width).max(0);
    let y = (main_rect.top - detail_height - sc(4)).max(0);
    unsafe {
        let hwnd = {
            let mut state = lock_state();
            if let Some(existing) = state
                .as_ref()
                .and_then(|s| s.detail_hwnd.map(|h| h.to_hwnd()))
            {
                existing
            } else {
                let class_name = native_interop::wide_str("ClaudeCodeUsageDetail");
                let title = native_interop::wide_str("Codex Usage Detail");
                let hinstance = GetModuleHandleW(PCWSTR::null()).unwrap();
                let created = CreateWindowExW(
                    WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
                    PCWSTR::from_raw(class_name.as_ptr()),
                    PCWSTR::from_raw(title.as_ptr()),
                    WS_POPUP,
                    x,
                    y,
                    detail_width,
                    detail_height,
                    HWND::default(),
                    HMENU::default(),
                    hinstance,
                    None,
                )
                .unwrap();
                if let Some(s) = state.as_mut() {
                    s.detail_hwnd = Some(SendHwnd::from_hwnd(created));
                }
                created
            }
        };
        native_interop::move_window(hwnd, x, y, detail_width, detail_height);
        paint_detail_layered(hwnd, detail_width, detail_height);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
    diagnose::log(format!(
        "fixed detail popup shown rows={detail_rows} x={x} y={y} w={detail_width} h={detail_height}"
    ));
}

fn point_in_window(hwnd: HWND, pt: POINT) -> bool {
    native_interop::get_window_rect_safe(hwnd)
        .map(|rect| {
            pt.x >= rect.left && pt.x < rect.right && pt.y >= rect.top && pt.y < rect.bottom
        })
        .unwrap_or(false)
}

fn close_detail_if_clicked_outside() {
    let left_down = unsafe { (GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000) != 0 };
    let right_down = unsafe { (GetAsyncKeyState(VK_RBUTTON.0 as i32) as u16 & 0x8000) != 0 };
    if !left_down && !right_down {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            if s.detail_visible {
                s.detail_mouse_down_seen = false;
            }
        }
        return;
    }

    let (main_hwnd, detail_hwnd, visible, already_seen_down) = {
        let mut state = lock_state();
        state
            .as_mut()
            .map(|s| {
                let already_seen = s.detail_mouse_down_seen;
                s.detail_mouse_down_seen = true;
                (
                    s.hwnd.to_hwnd(),
                    s.detail_hwnd.map(|h| h.to_hwnd()),
                    s.detail_visible,
                    already_seen,
                )
            })
            .unwrap_or((HWND::default(), None, false, true))
    };
    if !visible || already_seen_down {
        return;
    }

    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    let in_main = point_in_window(main_hwnd, pt);
    let in_detail = detail_hwnd
        .map(|hwnd| point_in_window(hwnd, pt))
        .unwrap_or(false);
    if !in_main && !in_detail {
        hide_detail_window();
    }
}

fn paint_detail_layered(hwnd: HWND, width: i32, height: i32) {
    if width <= 0 || height <= 0 {
        return;
    }
    unsafe {
        let screen_dc = GetDC(HWND::default());
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let mem_dc = CreateCompatibleDC(screen_dc);
        let dib =
            CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();
        if dib.is_invalid() || bits.is_null() {
            let _ = DeleteDC(mem_dc);
            ReleaseDC(HWND::default(), screen_dc);
            return;
        }
        let old_bmp = SelectObject(mem_dc, dib);
        let clear_brush = CreateSolidBrush(COLORREF(FLOATING_TRANSPARENT_KEY));
        let clear_rect = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        FillRect(mem_dc, &clear_rect, clear_brush);
        let _ = DeleteObject(clear_brush);

        paint_detail_panel(mem_dc, width, height);

        let pixel_count = (width * height) as usize;
        let pixel_data = std::slice::from_raw_parts_mut(bits as *mut u32, pixel_count);
        for px in pixel_data.iter_mut() {
            let rgb = *px & 0x00FFFFFF;
            if rgb == FLOATING_TRANSPARENT_KEY {
                *px = 0x00000000;
            } else {
                *px = rgb | 0xFF000000;
            }
        }

        let pt_src = POINT { x: 0, y: 0 };
        let sz = SIZE {
            cx: width,
            cy: height,
        };
        let blend = BLENDFUNCTION {
            BlendOp: 0,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: 1,
        };
        let _ = UpdateLayeredWindow(
            hwnd,
            screen_dc,
            None,
            Some(&sz),
            mem_dc,
            Some(&pt_src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );

        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(HWND::default(), screen_dc);
    }
}

fn paint_detail_panel(hdc: HDC, width: i32, height: i32) {
    let (items, summary, placement_mode, widget_theme, progress_color_mode, strings, hwnd) = {
        let state = lock_state();
        let Some(s) = state.as_ref() else {
            return;
        };
        let codex = s.data.as_ref().and_then(|data| data.codex.as_ref());
        (
            codex
                .map(|codex| codex.account_details.clone())
                .unwrap_or_default(),
            codex
                .filter(|codex| !codex.account_details.is_empty())
                .map(usage_summary::summarize),
            s.placement_mode,
            s.widget_theme,
            s.progress_color_mode,
            s.language.strings(),
            s.hwnd.to_hwnd(),
        )
    };
    let effective_dark = widget_theme == WidgetTheme::Dark;
    let bg = if effective_dark {
        Color::from_hex("#11151B")
    } else {
        Color::from_hex("#FFFFFF")
    };
    let border = if effective_dark {
        Color::from_hex("#2D3542")
    } else {
        Color::from_hex("#E4E8EF")
    };
    let muted = if effective_dark {
        Color::from_hex("#9AA6B5")
    } else {
        Color::from_hex("#667085")
    };
    let summary_bg = if effective_dark {
        Color::from_hex("#171D26")
    } else {
        Color::from_hex("#F6F8FA")
    };
    let text = if effective_dark {
        Color::from_hex("#E7EDF5")
    } else {
        Color::from_hex("#263142")
    };
    let track = if effective_dark {
        Color::from_hex("#303640")
    } else {
        Color::from_hex("#D7DDE6")
    };
    let accent = if effective_dark {
        Color::from_hex("#EAF2FF")
    } else {
        Color::from_hex("#172033")
    };

    unsafe {
        if width <= 0 || height <= 0 {
            return;
        }

        let _ = SetBkMode(hdc, TRANSPARENT);
        let font_name = native_interop::wide_str("Segoe UI");
        let text_font = CreateFontW(
            sc(-12),
            0,
            0,
            0,
            FW_MEDIUM.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_TT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            PCWSTR::from_raw(font_name.as_ptr()),
        );
        let old_font = SelectObject(hdc, text_font);
        let top = if placement_mode == PlacementMode::Fixed {
            sc(2)
        } else {
            sc(FLOATING_WIDGET_HEIGHT) + sc(2)
        };
        let hover_pt = detail_hover_point(hwnd);
        if items.is_empty() {
            let rect = RECT {
                left: sc(3),
                top,
                right: width - sc(3),
                bottom: height - sc(8),
            };
            draw_rounded_rect(hdc, &rect, &bg, sc(7));
            draw_rounded_frame(hdc, &rect, &border, sc(7));
            let mut label_wide = native_interop::wide_str("暂无账号数据");
            let _ = SetTextColor(hdc, COLORREF(muted.to_colorref()));
            let mut text_rect = RECT {
                left: rect.left + sc(12),
                top: rect.top,
                right: rect.right - sc(12),
                bottom: rect.bottom,
            };
            let _ = DrawTextW(
                hdc,
                &mut label_wide,
                &mut text_rect,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
            );
        } else {
            let visible_account_rows = items.len().min(DETAIL_PANEL_MAX_ACCOUNT_ROWS);
            let visible_rows = 1 + visible_account_rows;
            let panel_rect = RECT {
                left: sc(3),
                top,
                right: width - sc(3),
                bottom: top + visible_rows as i32 * sc(DETAIL_ROW_HEIGHT) - sc(6),
            };
            draw_rounded_rect(hdc, &panel_rect, &bg, sc(10));
            draw_rounded_frame(hdc, &panel_rect, &border, sc(10));

            if let Some(summary) = summary.as_ref() {
                let summary_rect = RECT {
                    left: panel_rect.left + sc(1),
                    top: panel_rect.top + sc(1),
                    right: panel_rect.right - sc(1),
                    bottom: top + sc(DETAIL_ROW_HEIGHT) - sc(5),
                };
                draw_rounded_rect(hdc, &summary_rect, &summary_bg, sc(9));
                draw_compact_summary_row(
                    hdc,
                    summary,
                    sc(10),
                    top,
                    width - sc(20),
                    strings,
                    &text,
                    &muted,
                    &track,
                    &accent,
                    progress_color_mode,
                    hover_pt,
                );
            }
            for (idx, item) in items.iter().take(DETAIL_PANEL_MAX_ACCOUNT_ROWS).enumerate() {
                let y = top + (idx as i32 + 1) * sc(DETAIL_ROW_HEIGHT);
                let separator = RECT {
                    left: panel_rect.left + sc(16),
                    top: y,
                    right: panel_rect.right - sc(16),
                    bottom: y + sc(1),
                };
                let separator_brush = CreateSolidBrush(COLORREF(border.to_colorref()));
                FillRect(hdc, &separator, separator_brush);
                let _ = DeleteObject(separator_brush);
                draw_compact_detail_row(
                    hdc,
                    item,
                    sc(10),
                    y,
                    width - sc(20),
                    strings,
                    &text,
                    &muted,
                    &track,
                    &accent,
                    progress_color_mode,
                    hover_pt,
                );
            }
        }
        SelectObject(hdc, old_font);
        let _ = DeleteObject(text_font);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_compact_summary_row(
    hdc: HDC,
    summary: &UsageSummary,
    x: i32,
    y: i32,
    width: i32,
    strings: Strings,
    text: &Color,
    muted: &Color,
    track: &Color,
    accent: &Color,
    progress_color_mode: ProgressColorMode,
    hover_pt: Option<POINT>,
) {
    unsafe {
        let marker_rect = RECT {
            left: x + sc(10),
            top: y + sc(11),
            right: x + sc(13),
            bottom: y + sc(27),
        };
        draw_rounded_rect(hdc, &marker_rect, accent, sc(1));

        let mut name_wide =
            native_interop::wide_str(&format!("整体汇总 · {} 个账号", summary.account_count));
        let mut name_rect = RECT {
            left: x + sc(20),
            top: y + sc(8),
            right: x + sc(192),
            bottom: y + sc(30),
        };
        let _ = SetTextColor(hdc, COLORREF(text.to_colorref()));
        let _ = DrawTextW(
            hdc,
            &mut name_wide,
            &mut name_rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
        draw_detail_column_headers(hdc, x + sc(12), x + width - sc(12), y + sc(8), muted);

        draw_compact_summary_line(
            hdc,
            x + sc(12),
            x + width - sc(12),
            y + sc(36),
            "5h",
            &summary.session,
            strings,
            text,
            muted,
            track,
            accent,
            progress_color_mode,
            hover_pt,
        );
        draw_compact_summary_line(
            hdc,
            x + sc(12),
            x + width - sc(12),
            y + sc(62),
            "7d",
            &summary.weekly,
            strings,
            text,
            muted,
            track,
            accent,
            progress_color_mode,
            hover_pt,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_compact_summary_line(
    hdc: HDC,
    x: i32,
    right: i32,
    y: i32,
    label: &str,
    summary: &UsageSummarySection,
    strings: Strings,
    text: &Color,
    muted: &Color,
    track: &Color,
    accent: &Color,
    progress_color_mode: ProgressColorMode,
    hover_pt: Option<POINT>,
) {
    draw_compact_usage_line(
        hdc,
        x,
        right,
        y,
        label,
        summary.requests,
        summary.tokens,
        summary.used_cost,
        &summary.usage,
        true,
        summary.total_quota,
        strings,
        text,
        muted,
        track,
        accent,
        progress_color_mode,
        hover_pt,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_compact_detail_row(
    hdc: HDC,
    item: &AccountUsageDetail,
    x: i32,
    y: i32,
    width: i32,
    strings: Strings,
    text: &Color,
    muted: &Color,
    track: &Color,
    accent: &Color,
    progress_color_mode: ProgressColorMode,
    hover_pt: Option<POINT>,
) {
    unsafe {
        let name = shorten_account_name(&item.name, item.id);
        let mut name_wide = native_interop::wide_str(&name);
        let mut name_rect = RECT {
            left: x + sc(12),
            top: y + sc(8),
            right: x + sc(192),
            bottom: y + sc(30),
        };
        let _ = SetTextColor(hdc, COLORREF(text.to_colorref()));
        let _ = DrawTextW(
            hdc,
            &mut name_wide,
            &mut name_rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
        draw_detail_column_headers(hdc, x + sc(12), x + width - sc(12), y + sc(8), muted);

        draw_compact_usage_line(
            hdc,
            x + sc(12),
            x + width - sc(12),
            y + sc(36),
            "5h",
            item.session_requests,
            item.session_tokens,
            item.session_cost,
            &item.session,
            true,
            usage_summary::estimate_total_quota(item.session_cost, item.session.percentage),
            strings,
            text,
            muted,
            track,
            accent,
            progress_color_mode,
            hover_pt,
        );
        draw_compact_usage_line(
            hdc,
            x + sc(12),
            x + width - sc(12),
            y + sc(62),
            "7d",
            item.weekly_requests,
            item.weekly_tokens,
            item.weekly_cost,
            &item.weekly,
            true,
            usage_summary::estimate_total_quota(item.weekly_cost, item.weekly.percentage),
            strings,
            text,
            muted,
            track,
            accent,
            progress_color_mode,
            hover_pt,
        );
    }
}

#[derive(Clone, Copy)]
struct DetailColumns {
    bar_left: i32,
    usage_left: i32,
    request_left: i32,
    token_left: i32,
    cost_left: i32,
    right: i32,
}

fn detail_columns(x: i32, right: i32) -> DetailColumns {
    let bar_left = x + sc(20);
    let usage_left = bar_left + sc(79);
    let request_left = usage_left + sc(81);
    let metric_width = (right - request_left).max(0) / 3;
    let token_left = request_left + metric_width;
    let cost_left = token_left + metric_width;
    DetailColumns {
        bar_left,
        usage_left,
        request_left,
        token_left,
        cost_left,
        right,
    }
}

fn draw_detail_column_headers(hdc: HDC, x: i32, right: i32, y: i32, muted: &Color) {
    unsafe {
        let columns = detail_columns(x, right);
        let headers = [
            ("请求", columns.request_left, columns.token_left),
            ("Token", columns.token_left, columns.cost_left),
            ("额度", columns.cost_left, columns.right),
        ];
        let _ = SetTextColor(hdc, COLORREF(muted.to_colorref()));
        for (label, left, right) in headers {
            let mut wide = native_interop::wide_str(label);
            let mut rect = RECT {
                left,
                top: y,
                right,
                bottom: y + sc(22),
            };
            let _ = DrawTextW(
                hdc,
                &mut wide,
                &mut rect,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_compact_usage_line(
    hdc: HDC,
    x: i32,
    right: i32,
    y: i32,
    label: &str,
    requests: u64,
    tokens: u64,
    cost: f64,
    section: &UsageSection,
    show_total_quota: bool,
    total_quota: Option<f64>,
    strings: Strings,
    text: &Color,
    muted: &Color,
    track: &Color,
    accent: &Color,
    progress_color_mode: ProgressColorMode,
    hover_pt: Option<POINT>,
) {
    unsafe {
        let columns = detail_columns(x, right);
        let mut label_wide = native_interop::wide_str(label);
        let mut label_rect = RECT {
            left: x,
            top: y,
            right: columns.bar_left,
            bottom: y + sc(18),
        };
        let _ = SetTextColor(hdc, COLORREF(muted.to_colorref()));
        let _ = DrawTextW(
            hdc,
            &mut label_wide,
            &mut label_rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );

        let bar_x = columns.bar_left;
        let segment_count = 8;
        let seg_w = sc(7);
        let seg_h = sc(12);
        let seg_gap = sc(1);
        let stats = detail_stat_values(requests, tokens, cost, show_total_quota, total_quota);
        let metric_rects = [
            RECT {
                left: columns.request_left,
                top: y,
                right: columns.token_left,
                bottom: y + sc(18),
            },
            RECT {
                left: columns.token_left,
                top: y,
                right: columns.cost_left,
                bottom: y + sc(18),
            },
            RECT {
                left: columns.cost_left,
                top: y,
                right: columns.right,
                bottom: y + sc(18),
            },
        ];
        let stat_labels = ["请求数", "Token数", "当前额度 / 总额度"];
        let mut tooltip: Option<(RECT, &str)> = None;
        for (idx, value) in stats.iter().enumerate() {
            let metric_rect = metric_rects[idx];
            let is_hovered = hover_pt
                .map(|pt| point_in_rect(pt, &metric_rect))
                .unwrap_or(false);
            if is_hovered {
                tooltip = Some((metric_rect, stat_labels[idx]));
            }
            let mut stats_wide = native_interop::wide_str(value);
            let mut stats_rect = RECT {
                left: metric_rect.left,
                top: metric_rect.top,
                right: metric_rect.right,
                bottom: metric_rect.bottom,
            };
            let _ = SetTextColor(hdc, COLORREF(muted.to_colorref()));
            let _ = DrawTextW(
                hdc,
                &mut stats_wide,
                &mut stats_rect,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
            );
        }
        let fill = progress_fill_color(
            section.percentage.clamp(0.0, 100.0),
            accent,
            progress_color_mode,
        );
        let percent = section.percentage.clamp(0.0, 100.0);
        let segment_percent = 100.0 / segment_count as f64;
        for i in 0..segment_count {
            let seg_x = bar_x + i * (seg_w + seg_gap);
            let seg_start = i as f64 * segment_percent;
            let seg_end = seg_start + segment_percent;
            let rect = RECT {
                left: seg_x,
                top: y + sc(3),
                right: seg_x + seg_w,
                bottom: y + sc(3) + seg_h,
            };
            if percent >= seg_end {
                draw_rounded_rect(hdc, &rect, &fill, sc(2));
            } else if percent <= seg_start {
                draw_rounded_rect(hdc, &rect, track, sc(2));
            } else {
                draw_rounded_rect(hdc, &rect, track, sc(2));
                let fraction = (percent - seg_start) / segment_percent;
                let fill_width = (seg_w as f64 * fraction) as i32;
                if fill_width > 0 {
                    let fill_rect = RECT {
                        left: seg_x,
                        top: y + sc(3),
                        right: seg_x + fill_width,
                        bottom: y + sc(3) + seg_h,
                    };
                    let rgn = CreateRoundRectRgn(
                        rect.left,
                        rect.top,
                        rect.right + 1,
                        rect.bottom + 1,
                        sc(4),
                        sc(4),
                    );
                    let _ = SelectClipRgn(hdc, rgn);
                    let brush = CreateSolidBrush(COLORREF(fill.to_colorref()));
                    FillRect(hdc, &fill_rect, brush);
                    let _ = DeleteObject(brush);
                    let _ = SelectClipRgn(hdc, HRGN::default());
                    let _ = DeleteObject(rgn);
                }
            }
        }

        let line = poller::format_line(section, strings);
        let mut line_wide = native_interop::wide_str(&line);
        let mut line_rect = RECT {
            left: columns.usage_left,
            top: y,
            right: columns.request_left,
            bottom: y + sc(18),
        };
        let _ = SetTextColor(hdc, COLORREF(text.to_colorref()));
        let _ = DrawTextW(
            hdc,
            &mut line_wide,
            &mut line_rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
        if let Some((metric_rect, tip)) = tooltip {
            draw_detail_tip(hdc, &metric_rect, tip, text);
        }
    }
}

fn detail_hover_point(hwnd: HWND) -> Option<POINT> {
    unsafe {
        let mut pt = POINT::default();
        if GetCursorPos(&mut pt).is_err() {
            return None;
        }
        let _ = ScreenToClient(hwnd, &mut pt);
        Some(pt)
    }
}

fn point_in_rect(pt: POINT, rect: &RECT) -> bool {
    pt.x >= rect.left && pt.x < rect.right && pt.y >= rect.top && pt.y < rect.bottom
}

fn draw_detail_tip(hdc: HDC, anchor: &RECT, text: &str, text_color: &Color) {
    unsafe {
        let tip_w = match text {
            "当前额度 / 总额度" => sc(112),
            "已用额度" => sc(66),
            "Token数" => sc(56),
            _ => sc(50),
        };
        let tip_h = sc(18);
        let center = (anchor.left + anchor.right) / 2;
        let mut tip_rect = RECT {
            left: center - tip_w / 2,
            top: anchor.top - tip_h - sc(4),
            right: center + tip_w / 2,
            bottom: anchor.top - sc(4),
        };
        if tip_rect.top < sc(2) {
            tip_rect.top = anchor.bottom + sc(4);
            tip_rect.bottom = tip_rect.top + tip_h;
        }
        draw_rounded_rect(hdc, &tip_rect, &Color::from_hex("#F2F4F7"), sc(6));
        draw_rounded_frame(hdc, &tip_rect, &Color::from_hex("#E2E6EC"), sc(6));
        let mut wide = native_interop::wide_str(text);
        let mut text_rect = RECT {
            left: tip_rect.left + sc(5),
            top: tip_rect.top,
            right: tip_rect.right - sc(5),
            bottom: tip_rect.bottom,
        };
        let _ = SetTextColor(hdc, COLORREF(text_color.to_colorref()));
        let _ = DrawTextW(
            hdc,
            &mut wide,
            &mut text_rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
    }
}

fn detail_stat_values(
    requests: u64,
    tokens: u64,
    cost: f64,
    show_total_quota: bool,
    total_quota: Option<f64>,
) -> [String; 3] {
    let cost = cost.max(0.0);
    let cost_text = if show_total_quota {
        match total_quota {
            Some(quota) => format!("${cost:.1} / ${:.1}", quota.max(cost)),
            None => format!("${cost:.1} / --"),
        }
    } else {
        format!("${cost:.1}")
    };
    [compact_number(requests), compact_number(tokens), cost_text]
}

fn compact_number(value: u64) -> String {
    if value >= 1_000_000_000 {
        format!("{:.1}B", value as f64 / 1_000_000_000.0)
    } else if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn shorten_account_name(name: &str, id: i64) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        format!("#{id}")
    } else {
        trimmed.to_string()
    }
}

fn show_context_menu(hwnd: HWND) {
    unsafe {
        let (
            current_interval,
            strings,
            language,
            language_override,
            install_channel,
            update_status,
            widget_visible,
            show_claude_code,
            show_codex,
            placement_mode,
            widget_theme,
            progress_color_mode,
            sub2api_groups,
            sub2api_groups_loading,
            current_group_id,
            usage_mode,
        ) = {
            let state = lock_state();
            let settings = load_settings();
            match state.as_ref() {
                Some(s) => (
                    s.poll_interval_ms,
                    s.language.strings(),
                    s.language,
                    s.language_override,
                    s.install_channel,
                    s.update_status.clone(),
                    s.widget_visible,
                    s.show_claude_code,
                    s.show_codex,
                    s.placement_mode,
                    s.widget_theme,
                    s.progress_color_mode,
                    s.sub2api_groups.clone(),
                    s.sub2api_groups_loading,
                    settings.sub2api.group_id,
                    settings.usage_mode,
                ),
                None => (
                    POLL_15_MIN,
                    LanguageId::English.strings(),
                    LanguageId::English,
                    None,
                    InstallChannel::Portable,
                    UpdateStatus::Idle,
                    true,
                    true,
                    false,
                    PlacementMode::Fixed,
                    WidgetTheme::Dark,
                    ProgressColorMode::Mono,
                    Vec::new(),
                    false,
                    None,
                    poller::UsageMode::Codex,
                ),
            }
        };

        let menu = CreatePopupMenu().unwrap();

        let refresh_str = native_interop::wide_str(strings.refresh);
        let _ = AppendMenuW(
            menu,
            MENU_ITEM_FLAGS(0),
            1,
            PCWSTR::from_raw(refresh_str.as_ptr()),
        );

        // Update Frequency submenu
        let freq_menu = CreatePopupMenu().unwrap();
        let ten_seconds = format!("10{}", strings.second_suffix);
        let thirty_seconds = format!("30{}", strings.second_suffix);
        let freq_items: [(u16, u32, &str); 6] = [
            (IDM_FREQ_10SEC, POLL_10_SEC, ten_seconds.as_str()),
            (IDM_FREQ_30SEC, POLL_30_SEC, thirty_seconds.as_str()),
            (IDM_FREQ_1MIN, POLL_1_MIN, strings.one_minute),
            (IDM_FREQ_5MIN, POLL_5_MIN, strings.five_minutes),
            (IDM_FREQ_15MIN, POLL_15_MIN, strings.fifteen_minutes),
            (IDM_FREQ_1HOUR, POLL_1_HOUR, strings.one_hour),
        ];
        for (id, interval, label) in freq_items {
            let label_str = native_interop::wide_str(label);
            let flags = if interval == current_interval {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                freq_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label_str.as_ptr()),
            );
        }

        let freq_label = native_interop::wide_str(strings.update_frequency);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            freq_menu.0 as usize,
            PCWSTR::from_raw(freq_label.as_ptr()),
        );

        // Models submenu
        let models_menu = CreatePopupMenu().unwrap();

        let codex_model = native_interop::wide_str(strings.codex_model);
        let codex_flags = if show_codex && !show_claude_code {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            models_menu,
            codex_flags,
            IDM_MODEL_CODEX as usize,
            PCWSTR::from_raw(codex_model.as_ptr()),
        );

        let models_label = native_interop::wide_str(strings.models);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            models_menu.0 as usize,
            PCWSTR::from_raw(models_label.as_ptr()),
        );

        if usage_mode == poller::UsageMode::Sub2api {
            let groups_menu = CreatePopupMenu().unwrap();
            if sub2api_groups.is_empty() {
                let label = if sub2api_groups_loading {
                    "加载分组中..."
                } else {
                    "刷新分组"
                };
                let label = native_interop::wide_str(label);
                let _ = AppendMenuW(
                    groups_menu,
                    if sub2api_groups_loading {
                        MF_GRAYED
                    } else {
                        MENU_ITEM_FLAGS(0)
                    },
                    IDM_GROUP_REFRESH as usize,
                    PCWSTR::from_raw(label.as_ptr()),
                );
            } else {
                for (idx, group) in sub2api_groups
                    .iter()
                    .take((IDM_GROUP_MAX - IDM_GROUP_BASE + 1) as usize)
                    .enumerate()
                {
                    let label = native_interop::wide_str(&group.name);
                    let flags = if Some(group.id) == current_group_id {
                        MF_CHECKED
                    } else {
                        MENU_ITEM_FLAGS(0)
                    };
                    let _ = AppendMenuW(
                        groups_menu,
                        flags,
                        (IDM_GROUP_BASE + idx as u16) as usize,
                        PCWSTR::from_raw(label.as_ptr()),
                    );
                }
                let _ = AppendMenuW(groups_menu, MF_SEPARATOR, 0, PCWSTR::null());
                let refresh_label = native_interop::wide_str("刷新分组");
                let _ = AppendMenuW(
                    groups_menu,
                    if sub2api_groups_loading {
                        MF_GRAYED
                    } else {
                        MENU_ITEM_FLAGS(0)
                    },
                    IDM_GROUP_REFRESH as usize,
                    PCWSTR::from_raw(refresh_label.as_ptr()),
                );
            }

            let groups_label = native_interop::wide_str("分组");
            let _ = AppendMenuW(
                menu,
                MF_POPUP,
                groups_menu.0 as usize,
                PCWSTR::from_raw(groups_label.as_ptr()),
            );
        }

        let placement_menu = CreatePopupMenu().unwrap();
        let fixed_label = native_interop::wide_str("固定");
        let floating_label = native_interop::wide_str("浮动");
        let _ = AppendMenuW(
            placement_menu,
            if placement_mode == PlacementMode::Fixed {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            },
            IDM_PLACEMENT_FIXED as usize,
            PCWSTR::from_raw(fixed_label.as_ptr()),
        );
        let _ = AppendMenuW(
            placement_menu,
            if placement_mode == PlacementMode::Floating {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            },
            IDM_PLACEMENT_FLOATING as usize,
            PCWSTR::from_raw(floating_label.as_ptr()),
        );
        let placement_label = native_interop::wide_str("位置");
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            placement_menu.0 as usize,
            PCWSTR::from_raw(placement_label.as_ptr()),
        );

        let selected_season = lock_state().as_ref().map(|s| s.season).unwrap_or_default();
        let season_menu = CreatePopupMenu().unwrap();
        for (idx, (season, label)) in [
            (crate::pixel_room::Season::Auto, "自动（跟随月份）"),
            (crate::pixel_room::Season::Spring, "春季"),
            (crate::pixel_room::Season::Summer, "夏季"),
            (crate::pixel_room::Season::Autumn, "秋季"),
            (crate::pixel_room::Season::Winter, "冬季"),
        ]
        .iter()
        .enumerate()
        {
            let label = native_interop::wide_str(label);
            let _ = AppendMenuW(
                season_menu,
                if *season == selected_season {
                    MF_CHECKED
                } else {
                    MENU_ITEM_FLAGS(0)
                },
                IDM_SEASON_AUTO as usize + idx,
                PCWSTR::from_raw(label.as_ptr()),
            );
        }
        let season_label = native_interop::wide_str("季节");
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            season_menu.0 as usize,
            PCWSTR::from_raw(season_label.as_ptr()),
        );

        let theme_menu = CreatePopupMenu().unwrap();
        let light_label = native_interop::wide_str("亮色");
        let dark_label = native_interop::wide_str("暗色");
        let _ = AppendMenuW(
            theme_menu,
            if widget_theme == WidgetTheme::Light {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            },
            IDM_THEME_LIGHT as usize,
            PCWSTR::from_raw(light_label.as_ptr()),
        );
        let _ = AppendMenuW(
            theme_menu,
            if widget_theme == WidgetTheme::Dark {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            },
            IDM_THEME_DARK as usize,
            PCWSTR::from_raw(dark_label.as_ptr()),
        );
        let theme_label = native_interop::wide_str("主题");
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            theme_menu.0 as usize,
            PCWSTR::from_raw(theme_label.as_ptr()),
        );

        let color_menu = CreatePopupMenu().unwrap();
        let mono_label = native_interop::wide_str("黑白");
        let antd_label = native_interop::wide_str("彩色");
        let _ = AppendMenuW(
            color_menu,
            if progress_color_mode == ProgressColorMode::Mono {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            },
            IDM_COLOR_MONO as usize,
            PCWSTR::from_raw(mono_label.as_ptr()),
        );
        let _ = AppendMenuW(
            color_menu,
            if progress_color_mode == ProgressColorMode::Antd {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            },
            IDM_COLOR_ANTD as usize,
            PCWSTR::from_raw(antd_label.as_ptr()),
        );
        let color_label = native_interop::wide_str("颜色");
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            color_menu.0 as usize,
            PCWSTR::from_raw(color_label.as_ptr()),
        );

        // Settings submenu
        let settings_menu = CreatePopupMenu().unwrap();

        let startup_str = native_interop::wide_str(strings.start_with_windows);
        let startup_flags = if is_startup_enabled() {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            settings_menu,
            startup_flags,
            IDM_START_WITH_WINDOWS as usize,
            PCWSTR::from_raw(startup_str.as_ptr()),
        );

        let reset_pos_str = native_interop::wide_str(strings.reset_position);
        let _ = AppendMenuW(
            settings_menu,
            MENU_ITEM_FLAGS(0),
            IDM_RESET_POSITION as usize,
            PCWSTR::from_raw(reset_pos_str.as_ptr()),
        );

        let language_menu = CreatePopupMenu().unwrap();
        let system_label = native_interop::wide_str(strings.system_default);
        let system_flags = if language_override.is_none() {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            language_menu,
            system_flags,
            IDM_LANG_SYSTEM as usize,
            PCWSTR::from_raw(system_label.as_ptr()),
        );

        for language in LanguageId::ALL {
            let id = match language {
                LanguageId::English => IDM_LANG_ENGLISH,
                LanguageId::Dutch => IDM_LANG_DUTCH,
                LanguageId::Spanish => IDM_LANG_SPANISH,
                LanguageId::French => IDM_LANG_FRENCH,
                LanguageId::German => IDM_LANG_GERMAN,
                LanguageId::Japanese => IDM_LANG_JAPANESE,
                LanguageId::Korean => IDM_LANG_KOREAN,
                LanguageId::SimplifiedChinese => IDM_LANG_SIMPLIFIED_CHINESE,
                LanguageId::TraditionalChinese => IDM_LANG_TRADITIONAL_CHINESE,
                LanguageId::Russian => IDM_LANG_RUSSIAN,
                LanguageId::PortugueseBrazil => IDM_LANG_PORTUGUESE_BRAZIL,
            };
            let label_str = native_interop::wide_str(language.native_name());
            let flags = if language_override == Some(language) {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                language_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label_str.as_ptr()),
            );
        }

        let language_label = native_interop::wide_str(strings.language);
        let _ = AppendMenuW(
            settings_menu,
            MF_POPUP,
            language_menu.0 as usize,
            PCWSTR::from_raw(language_label.as_ptr()),
        );

        let _ = AppendMenuW(settings_menu, MF_SEPARATOR, 0, PCWSTR::null());

        let version_label =
            version_action_label(strings, language, install_channel, &update_status);
        let version_str = native_interop::wide_str(&version_label);
        let version_flags = if matches!(
            update_status,
            UpdateStatus::Checking | UpdateStatus::Applying
        ) {
            MF_GRAYED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            settings_menu,
            version_flags,
            IDM_VERSION_ACTION as usize,
            PCWSTR::from_raw(version_str.as_ptr()),
        );

        let settings_label = native_interop::wide_str(strings.settings);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            settings_menu.0 as usize,
            PCWSTR::from_raw(settings_label.as_ptr()),
        );

        let widget_label = native_interop::wide_str(strings.show_widget);
        let widget_flags = if widget_visible {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            menu,
            widget_flags,
            tray_icon::IDM_TOGGLE_WIDGET as usize,
            PCWSTR::from_raw(widget_label.as_ptr()),
        );

        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

        let exit_str = native_interop::wide_str(strings.exit);
        let _ = AppendMenuW(
            menu,
            MENU_ITEM_FLAGS(0),
            2,
            PCWSTR::from_raw(exit_str.as_ptr()),
        );

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, None);
        let _ = DestroyMenu(menu);
    }
}

/// Paint for non-embedded fallback (normal WM_PAINT path)
fn paint(hdc: HDC, hwnd: HWND) {
    let (
        is_dark,
        strings,
        session_pct,
        session_text,
        weekly_pct,
        weekly_text,
        codex_session_pct,
        codex_session_text,
        codex_weekly_pct,
        codex_weekly_text,
        show_claude_code,
        show_codex,
        placement_mode,
        widget_theme,
        progress_color_mode,
        edge_collapse,
        edge_expanded,
    ) = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => (
                s.is_dark,
                s.language.strings(),
                s.session_percent,
                s.session_text.clone(),
                s.weekly_percent,
                s.weekly_text.clone(),
                s.codex_session_percent,
                s.codex_session_text.clone(),
                s.codex_weekly_percent,
                s.codex_weekly_text.clone(),
                s.show_claude_code,
                s.show_codex,
                s.placement_mode,
                s.widget_theme,
                s.progress_color_mode,
                s.edge_collapse,
                s.edge_expanded,
            ),
            None => return,
        }
    };

    let accent = claude_accent_color();
    let codex_accent = codex_accent_color(is_dark);
    let track = if is_dark {
        Color::from_hex("#444444")
    } else {
        Color::from_hex("#AAAAAA")
    };
    let effective_dark = if placement_mode == PlacementMode::Floating {
        widget_theme == WidgetTheme::Dark
    } else {
        is_dark
    };
    let text_color = if effective_dark {
        if placement_mode == PlacementMode::Floating {
            Color::from_hex("#C8D0DA")
        } else {
            Color::from_hex("#888888")
        }
    } else {
        Color::from_hex("#404040")
    };
    let bg_color = if placement_mode == PlacementMode::Floating {
        if widget_theme == WidgetTheme::Dark {
            Color::from_hex("#15171C")
        } else {
            Color::from_hex("#FFFFFF")
        }
    } else if is_dark {
        Color::from_hex("#1C1C1C")
    } else {
        Color::from_hex("#F3F3F3")
    };

    unsafe {
        let mut client_rect = RECT::default();
        let _ = GetClientRect(hwnd, &mut client_rect);
        let width = client_rect.right - client_rect.left;
        let height = client_rect.bottom - client_rect.top;

        if width <= 0 || height <= 0 {
            return;
        }

        let mem_dc = CreateCompatibleDC(hdc);
        let mem_bmp = CreateCompatibleBitmap(hdc, width, height);
        let old_bmp = SelectObject(mem_dc, mem_bmp);

        paint_content(
            mem_dc,
            width,
            height,
            is_dark,
            &bg_color,
            &text_color,
            &accent,
            &track,
            strings,
            session_pct,
            &session_text,
            weekly_pct,
            &weekly_text,
            codex_session_pct,
            &codex_session_text,
            codex_weekly_pct,
            &codex_weekly_text,
            show_claude_code,
            show_codex,
            placement_mode,
            progress_color_mode,
            if edge_expanded {
                EdgeCollapse::None
            } else {
                edge_collapse
            },
            &codex_accent,
            false,
            0.0,
            0,
        );

        let _ = BitBlt(hdc, 0, 0, width, height, mem_dc, 0, 0, SRCCOPY);

        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(mem_bmp);
        let _ = DeleteDC(mem_dc);
    }
}

fn draw_row(
    hdc: HDC,
    x: i32,
    y: i32,
    is_dark: bool,
    text_color: &Color,
    label: &str,
    claude_percent: f64,
    claude_text: &str,
    codex_percent: f64,
    codex_text: &str,
    show_claude_code: bool,
    show_codex: bool,
    claude_accent: &Color,
    codex_accent: &Color,
    track: &Color,
    progress_color_mode: ProgressColorMode,
) {
    let seg_h = sc(SEGMENT_H);
    let active_models = active_model_count(show_claude_code, show_codex);
    let segment_count = row_bar_segment_count(active_models);
    let use_model_text_colors = show_claude_code && show_codex;
    let claude_value_color = if use_model_text_colors {
        claude_usage_text_color(is_dark)
    } else {
        *text_color
    };
    let codex_value_color = if use_model_text_colors {
        codex_usage_text_color(is_dark)
    } else {
        *text_color
    };

    unsafe {
        let _ = SetTextColor(hdc, COLORREF(text_color.to_colorref()));
        let mut label_wide: Vec<u16> = label.encode_utf16().collect();
        let mut label_rect = RECT {
            left: x,
            top: y,
            right: x + sc(LABEL_WIDTH),
            bottom: y + seg_h,
        };
        let _ = DrawTextW(
            hdc,
            &mut label_wide,
            &mut label_rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );

        let mut model_x = x + sc(LABEL_WIDTH) + sc(LABEL_RIGHT_MARGIN);
        if show_claude_code {
            draw_usage_bar(
                hdc,
                model_x,
                y,
                segment_count,
                claude_percent,
                claude_text,
                claude_accent,
                track,
                &claude_value_color,
                progress_color_mode,
            );
            model_x += model_usage_width(segment_count) + sc(MODEL_RIGHT_MARGIN);
        }
        if show_codex {
            draw_usage_bar(
                hdc,
                model_x,
                y,
                segment_count,
                codex_percent,
                codex_text,
                codex_accent,
                track,
                &codex_value_color,
                progress_color_mode,
            );
        }
    }
}

fn model_usage_width(segment_count: i32) -> i32 {
    (sc(SEGMENT_W) + sc(SEGMENT_GAP)) * segment_count - sc(SEGMENT_GAP)
        + sc(BAR_RIGHT_MARGIN)
        + sc(TEXT_WIDTH)
}

fn draw_usage_bar(
    hdc: HDC,
    bar_x: i32,
    y: i32,
    segment_count: i32,
    percent: f64,
    text: &str,
    accent: &Color,
    track: &Color,
    text_color: &Color,
    progress_color_mode: ProgressColorMode,
) {
    let seg_w = sc(SEGMENT_W);
    let seg_h = sc(SEGMENT_H);
    let seg_gap = sc(SEGMENT_GAP);
    let corner_r = sc(CORNER_RADIUS);

    unsafe {
        let percent_clamped = percent.clamp(0.0, 100.0);
        let fill_color = progress_fill_color(percent_clamped, accent, progress_color_mode);
        let segment_percent = 100.0 / segment_count as f64;

        for i in 0..segment_count {
            let seg_x = bar_x + i * (seg_w + seg_gap);
            let seg_start = (i as f64) * segment_percent;
            let seg_end = seg_start + segment_percent;

            let seg_rect = RECT {
                left: seg_x,
                top: y,
                right: seg_x + seg_w,
                bottom: y + seg_h,
            };

            if percent_clamped >= seg_end {
                draw_rounded_rect(hdc, &seg_rect, &fill_color, corner_r);
            } else if percent_clamped <= seg_start {
                draw_rounded_rect(hdc, &seg_rect, track, corner_r);
            } else {
                draw_rounded_rect(hdc, &seg_rect, track, corner_r);
                let fraction = (percent_clamped - seg_start) / segment_percent;
                let fill_width = (seg_w as f64 * fraction) as i32;
                if fill_width > 0 {
                    let fill_rect = RECT {
                        left: seg_x,
                        top: y,
                        right: seg_x + fill_width,
                        bottom: y + seg_h,
                    };
                    let rgn = CreateRoundRectRgn(
                        seg_rect.left,
                        seg_rect.top,
                        seg_rect.right + 1,
                        seg_rect.bottom + 1,
                        corner_r * 2,
                        corner_r * 2,
                    );
                    let _ = SelectClipRgn(hdc, rgn);
                    let brush = CreateSolidBrush(COLORREF(fill_color.to_colorref()));
                    FillRect(hdc, &fill_rect, brush);
                    let _ = DeleteObject(brush);
                    let _ = SelectClipRgn(hdc, HRGN::default());
                    let _ = DeleteObject(rgn);
                }
            }
        }

        let text_x = bar_x + segment_count * (seg_w + seg_gap) - seg_gap + sc(BAR_RIGHT_MARGIN);
        let mut text_wide: Vec<u16> = text.encode_utf16().collect();
        let mut text_rect = RECT {
            left: text_x,
            top: y,
            right: text_x + sc(TEXT_WIDTH),
            bottom: y + seg_h,
        };
        let _ = SetTextColor(hdc, COLORREF(text_color.to_colorref()));
        let _ = DrawTextW(
            hdc,
            &mut text_wide,
            &mut text_rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
    }
}

fn progress_fill_color(percent: f64, fallback: &Color, mode: ProgressColorMode) -> Color {
    match mode {
        ProgressColorMode::Mono => *fallback,
        ProgressColorMode::Antd if percent < 50.0 => Color::from_hex("#389E0D"),
        ProgressColorMode::Antd if percent < 80.0 => Color::from_hex("#D48806"),
        ProgressColorMode::Antd => Color::from_hex("#CF1322"),
    }
}

fn draw_rounded_rect(hdc: HDC, rect: &RECT, color: &Color, radius: i32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(color.to_colorref()));
        let rgn = CreateRoundRectRgn(
            rect.left,
            rect.top,
            rect.right + 1,
            rect.bottom + 1,
            radius * 2,
            radius * 2,
        );
        let _ = FillRgn(hdc, rgn, brush);
        let _ = DeleteObject(rgn);
        let _ = DeleteObject(brush);
    }
}

fn draw_rounded_frame(hdc: HDC, rect: &RECT, color: &Color, radius: i32) {
    unsafe {
        let pen = CreatePen(PS_SOLID, sc(1), COLORREF(color.to_colorref()));
        let old_pen = SelectObject(hdc, pen);
        let brush = GetStockObject(NULL_BRUSH);
        let old_brush = SelectObject(hdc, brush);
        let _ = RoundRect(
            hdc,
            rect.left,
            rect.top,
            rect.right,
            rect.bottom,
            radius * 2,
            radius * 2,
        );
        SelectObject(hdc, old_brush);
        SelectObject(hdc, old_pen);
        let _ = DeleteObject(pen);
    }
}
