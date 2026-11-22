use std::mem::size_of;

use windows::{
    Win32::{
        Foundation::{LPARAM, POINT, RECT},
        Graphics::Gdi::{
            EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONULL,
            MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint,
        },
        System::Com::CoTaskMemFree,
        System::Registry::{
            HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_SET_VALUE, REG_DWORD,
            REG_OPTION_NON_VOLATILE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW,
            RegGetValueW, RegSetValueExW,
        },
        UI::{
            HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
            Shell::{FOLDERID_Downloads, KF_FLAG_DEFAULT, SHGetKnownFolderPath},
            WindowsAndMessaging::{SPI_GETCLIENTAREAANIMATION, SystemParametersInfoW},
        },
    },
    core::{BOOL, PCWSTR, w},
};

/// Returns `true` if Windows uses the light theme for the taskbar and shell.
///
/// Assumes the dark theme on read failure (the Windows 11 shell default).
#[must_use]
pub fn uses_light_theme() -> bool {
    personalize(w!("SystemUsesLightTheme")).is_some_and(|value| value != 0)
}

/// Returns `true` while Windows draws see-through surfaces: "Transparency
/// effects" is on and energy saver is off (it turns them off). Otherwise the
/// system backdrop brush stops blurring and shows up black, so glass has to
/// be drawn solid.
///
/// On Windows 10 there is no backdrop brush for desktop windows at all
/// (`DWMWA_USE_HOSTBACKDROPBRUSH` arrived with Windows 11), so it is always
/// `false` there.
#[must_use]
pub fn transparency_enabled() -> bool {
    if !is_windows_11() {
        return false;
    }
    // Missing means never changed, and the default is on.
    !energy_saver_on() && personalize(w!("EnableTransparency")) != Some(0)
}

/// Suppresses or restores native Windows toast notification banners.
///
/// Setting `ToastEnabled` to 0 prevents Windows from showing its native toast
/// popups so Muelle's macOS-style glass notification banners can take precedence.
pub fn set_windows_toasts_enabled(enabled: bool) {
    let value = u32::from(enabled).to_le_bytes();
    // SAFETY: Updates the user's push notification toast setting safely.
    unsafe {
        let mut key = HKEY::default();
        if RegCreateKeyExW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\PushNotifications"),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
        .is_ok()
        {
            let _ = RegSetValueExW(key, w!("ToastEnabled"), None, REG_DWORD, Some(&value));
            let _ = RegCloseKey(key);
        }
    }
}

/// `true` while energy saver (battery saver on Windows 10) is on.
#[must_use]
pub fn energy_saver_on() -> bool {
    if let Some(on) = crate::power::notified_saver_state() {
        return on;
    }
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    const SAVER_ON: u8 = 1;
    let mut status = SYSTEM_POWER_STATUS::default();
    // SAFETY: `status` is a valid structure the call fills in.
    unsafe { GetSystemPowerStatus(&mut status) }.is_ok() && status.SystemStatusFlag & SAVER_ON != 0
}

/// The input language of the window in front as two capital letters
/// ("ES", "EN"), or `None` with a single language installed: the Windows
/// taskbar only shows its language indicator when there is a choice.
#[must_use]
pub fn input_language() -> Option<String> {
    use windows::Win32::{
        Globalization::LCIDToLocaleName, UI::Input::KeyboardAndMouse::GetKeyboardLayoutList,
    };
    // SAFETY: Read-only queries; `name` is a valid buffer for the locale name.
    unsafe {
        if GetKeyboardLayoutList(None) < 2 {
            return None;
        }
        // Each thread has its own layout: the one that matters is in front.
        let language = (crate::keyboard::Context::current()?.layout as usize & 0xFFFF) as u32;
        let mut name = [0u16; 85];
        let len = usize::try_from(LCIDToLocaleName(language, Some(&mut name), 0)).ok()?;
        let tag = String::from_utf16_lossy(name.get(..len.checked_sub(1)?)?);
        let code = tag.split('-').next()?.to_uppercase();
        (!code.is_empty()).then_some(code)
    }
}

/// `true` on Windows 11 (build 22000 or later), `false` on Windows 10.
/// Read once: the version does not change while the dock runs.
#[must_use]
pub fn is_windows_11() -> bool {
    static WINDOWS_11: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *WINDOWS_11.get_or_init(|| windows_build().is_none_or(|build| build >= 22000))
}

/// The Windows build number, e.g. 26100. Windows reports "10.0" for both
/// Windows 10 and 11; only the build tells them apart.
fn windows_build() -> Option<u32> {
    let mut buffer = [0u16; 16];
    let mut size = size_of_val(&buffer) as u32;
    // SAFETY: `buffer` and `size` describe a valid buffer for the string.
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion"),
            w!("CurrentBuildNumber"),
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut size),
        )
    }
    .ok()
    .ok()?;
    let len = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..len]).parse().ok()
}

/// A DWORD from the user's personalization settings.
/// The accent color chosen in Settings → Personalization → Colors, as
/// red, green and blue.
#[must_use]
pub fn accent_color() -> Option<[u8; 3]> {
    let mut value = 0u32;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: `value` and `size` describe a valid 4-byte buffer.
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\DWM"),
            w!("AccentColor"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut value).cast()),
            Some(&mut size),
        )
    }
    .ok()
    .ok()?;
    // Stored as 0xAABBGGRR.
    let [r, g, b, _] = value.to_le_bytes();
    Some([r, g, b])
}

fn personalize(name: PCWSTR) -> Option<u32> {
    let mut value = 0u32;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: `value` and `size` describe a valid 4-byte buffer.
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            name,
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut value).cast()),
            Some(&mut size),
        )
    }
    .is_ok()
    .then_some(value)
}

/// Monitor bounds, work area, and scale in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Monitor {
    /// Full-screen bounds.
    pub screen: RECT,
    pub work_left: i32,
    pub work_top: i32,
    pub work_right: i32,
    pub work_bottom: i32,
    /// 1.0 = 96 DPI (100%).
    pub scale: f32,
}

/// Primary monitor. `None` only if Windows cannot describe it.
#[must_use]
pub fn primary_monitor() -> Option<Monitor> {
    // SAFETY: The origin is on the primary monitor in virtual screen coordinates.
    monitor_info(unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) })
}

/// The monitor containing a screen point, with no fallback for gaps or disconnected displays.
#[must_use]
pub fn monitor_at(point: POINT) -> Option<Monitor> {
    // SAFETY: Read-only monitor lookup.
    monitor_info(unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONULL) })
}

/// Physical bounds of the connected displays, including negative coordinates.
#[must_use]
pub fn monitor_rects() -> Vec<RECT> {
    unsafe extern "system" fn append(_: HMONITOR, _: HDC, rect: *mut RECT, data: LPARAM) -> BOOL {
        // SAFETY: EnumDisplayMonitors runs synchronously and supplies the
        // current monitor bounds. The caller owns the vector for that call.
        unsafe {
            (&mut *(data.0 as *mut Vec<RECT>)).push(*rect);
        }
        BOOL(1)
    }
    let mut screens = Vec::new();
    // SAFETY: The callback's vector lives through the synchronous enumeration.
    if unsafe {
        !EnumDisplayMonitors(
            None,
            None,
            Some(append),
            LPARAM((&raw mut screens) as isize),
        )
        .as_bool()
    } {
        screens.clear();
    }
    screens
}

fn monitor_info(handle: HMONITOR) -> Option<Monitor> {
    // SAFETY: Read-only queries using correctly sized structures.
    unsafe {
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(handle, &mut info).as_bool() {
            return None;
        }
        let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
        // Fall back to 96 DPI.
        let _ = GetDpiForMonitor(handle, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
        Some(Monitor {
            screen: info.rcMonitor,
            work_left: info.rcWork.left,
            work_top: info.rcWork.top,
            work_right: info.rcWork.right,
            work_bottom: info.rcWork.bottom,
            scale: dpi_x as f32 / 96.0,
        })
    }
}

/// Path to the user's Downloads folder, which may be on another drive.
#[must_use]
pub fn downloads_folder() -> Option<String> {
    // SAFETY: The returned string is copied and freed with the allocator that created it.
    unsafe {
        let raw = SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_DEFAULT, None).ok()?;
        let path = raw.to_string().ok();
        CoTaskMemFree(Some(raw.as_ptr().cast()));
        path
    }
}

/// The languages Muelle's text comes in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiLanguage {
    Spanish,
    English,
    French,
}

/// The Windows display language, or English when Muelle has no text in it.
#[must_use]
pub fn ui_language() -> UiLanguage {
    const LANG_SPANISH: u16 = 0x0A;
    const LANG_FRENCH: u16 = 0x0C;
    // SAFETY: Read-only query.
    let language = unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() };
    match language & 0x3FF {
        LANG_SPANISH => UiLanguage::Spanish,
        LANG_FRENCH => UiLanguage::French,
        _ => UiLanguage::English,
    }
}

/// `false` if the user turned off "Animation effects" in Windows settings.
#[must_use]
pub fn animations_enabled() -> bool {
    let mut enabled = BOOL::from(true);
    // SAFETY: `enabled` is a valid BOOL the call writes to.
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some((&raw mut enabled).cast()),
            Default::default(),
        );
    }
    enabled.as_bool()
}

/// Enables or disables the DWM transitions (minimize, maximize, close
/// animations) of one window. It works across processes.
pub fn set_window_transitions(hwnd: isize, enabled: bool) {
    let disabled = BOOL::from(!enabled);
    // SAFETY: The value outlives the call; a window that has closed makes it fail harmlessly.
    unsafe {
        let _ = windows::Win32::Graphics::Dwm::DwmSetWindowAttribute(
            windows::Win32::Foundation::HWND(hwnd as *mut _),
            windows::Win32::Graphics::Dwm::DWMWA_TRANSITIONS_FORCEDISABLED,
            (&raw const disabled).cast(),
            size_of::<BOOL>() as u32,
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Battery {
    /// 0..=100.
    pub percent: u8,
    pub charging: bool,
    pub saver: bool,
}

/// Battery state, or `None` on a computer without a battery.
#[must_use]
pub fn battery() -> Option<Battery> {
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    const NO_BATTERY: u8 = 128;
    const UNKNOWN: u8 = 255;
    let mut status = SYSTEM_POWER_STATUS::default();
    // SAFETY: `status` is a valid structure the call fills in.
    unsafe { GetSystemPowerStatus(&mut status) }.ok()?;
    let saver = energy_saver_on();
    (status.BatteryFlag & NO_BATTERY == 0
        && status.BatteryFlag != UNKNOWN
        && status.BatteryLifePercent <= 100)
        .then_some(Battery {
            percent: status.BatteryLifePercent,
            charging: status.ACLineStatus == 1,
            saver,
        })
}

/// Current date and time in the regional format of the user, e.g. "vie 13 nov  19:42".
#[must_use]
pub fn clock_text() -> String {
    use windows::Win32::Globalization::{GetDateFormatEx, GetTimeFormatEx, TIME_NOSECONDS};
    let mut date = [0u16; 64];
    let mut time = [0u16; 32];
    // SAFETY: The buffers live for the calls, which return the length written
    // including the terminator (0 on failure).
    let (date_len, time_len) = unsafe {
        (
            GetDateFormatEx(
                None,
                Default::default(),
                None,
                w!("ddd d MMM"),
                Some(&mut date),
                None,
            ),
            GetTimeFormatEx(None, TIME_NOSECONDS, None, None, Some(&mut time)),
        )
    };
    let text =
        |buffer: &[u16], len: i32| String::from_utf16_lossy(&buffer[..(len - 1).max(0) as usize]);
    format!("{}  {}", text(&date, date_len), text(&time, time_len))
}
