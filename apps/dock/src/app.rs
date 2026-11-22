//! Dock windows and lifecycle.
//!
//! There are two windows:
//! - the **visual** window draws everything and passes through clicks; it is
//!   larger than the dock to fit the shadow, magnified icons, and labels;
//! - the invisible **input** window covers only the area where the dock receives
//!   mouse input. It expands while the cursor is over the dock (hysteresis).
//!
//! While the dock is running, the Windows taskbar is hidden and the dock is
//! registered as an AppBar, reserving a strip along the bottom of the screen.

use std::{
    cell::RefCell,
    collections::HashMap,
    env,
    sync::{
        OnceLock,
        atomic::{AtomicIsize, AtomicU8, Ordering},
    },
    thread,
    time::Instant,
};

use anyhow::{Context, Result};
use muelle_config::{Config, DockDensity, DockGroup, DockPosition};
use muelle_core::{
    attention::Attention,
    bounce,
    frame::{DockEdge, Frame as DockFrame},
    layout::{ItemKind, Layout, Metrics, Rect, layout},
    magnify::{Magnifier, magnify},
    roster::{Group, Roster},
    spring::Spring,
};
use muelle_shell::{
    appbar::{ABN_FULLSCREENAPP, AppBar, Edge},
    end_task::PreparedEndTask,
    files,
    icons::IconImage,
    jumplist, launcher,
    pins::{self, app_exists, taskbar_pins, taskbar_pins_if_readable},
    power,
    recycle_bin::{self, BinWatch},
    status,
    system::{
        Monitor, animations_enabled, downloads_folder, monitor_at, monitor_rects, primary_monitor,
        set_window_transitions, transparency_enabled, uses_light_theme,
    },
    taskbar,
    tracker::{self, TrackedWindow, WindowTracker, normalize_id},
};
use windows::{
    UI::Composition::{CompositionSurfaceBrush, Desktop::DesktopWindowTarget},
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Dwm::{DWMWA_USE_HOSTBACKDROPBRUSH, DwmFlush, DwmSetWindowAttribute},
        System::{
            Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize},
            Console::{CTRL_C_EVENT, SetConsoleCtrlHandler},
            LibraryLoader::GetModuleHandleW,
            Ole::{
                DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE,
                IDropTarget, OleInitialize, OleUninitialize, RegisterDragDrop, RevokeDragDrop,
            },
            WinRT::Composition::ICompositorDesktopInterop,
        },
        UI::{
            Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
            Input::KeyboardAndMouse::{
                GetAsyncKeyState, GetCapture, GetKeyState, ReleaseCapture, SetCapture, TME_LEAVE,
                TRACKMOUSEEVENT, TrackMouseEvent, VIRTUAL_KEY, VK_CONTROL, VK_DOWN, VK_END,
                VK_ESCAPE, VK_HOME, VK_LBUTTON, VK_LEFT, VK_LWIN, VK_MBUTTON, VK_MENU, VK_RBUTTON,
                VK_RIGHT, VK_RWIN, VK_SHIFT, VK_TAB, VK_UP,
            },
            WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DispatchMessageW, EVENT_OBJECT_CLOAKED,
                EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_UNCLOAKED, EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART, FindWindowW, GetCursorPos,
                GetMessageW, GetSystemMetrics, HMENU, HWND_TOPMOST, IDC_ARROW, KillTimer,
                LoadCursorW, MA_ACTIVATE, MA_NOACTIVATE, MSG, OBJID_WINDOW, PM_REMOVE,
                PeekMessageW, PostMessageW, PostQuitMessage, RegisterClassW,
                RegisterShellHookWindow, RegisterWindowMessageW, SM_CXDRAG, SM_SWAPBUTTON,
                SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_SHOWWINDOW, SetTimer, SetWindowPos,
                WINDOW_EX_STYLE, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_APP,
                WM_CAPTURECHANGED, WM_CLOSE, WM_COPYDATA, WM_DESTROY, WM_DISPLAYCHANGE,
                WM_DPICHANGED, WM_INITMENUPOPUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN,
                WM_MBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_QUIT, WM_RBUTTONUP,
                WM_SETTINGCHANGE, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
                WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
                WS_POPUP,
            },
        },
    },
    core::{BOOL, Interface, PCWSTR, w},
};

use crate::{
    assets::{Visuals, fallback_name},
    drop::DropTarget,
    edge_watch::{self, EdgeWatch, Reveal, Target as EdgeTarget},
    flight::{Flight, Snapshot},
    folder_menu,
    gfx::{Gfx, LabelBrush},
    group_view::{DragGhost, GroupItem, GroupView, Placement, Released},
    mascot,
    menu::{self, MenuItem},
    menubar::{MenuBar, MenuBarWidgets},
    multi_window_view::{self, MultiWindowAction, MultiWindowView},
    notification_banner,
    scene::{Drag, Frame, Scene, SceneInput, SlotContent, Theme},
    shell_work::{self, Action as ShellAction},
    strings::{self, text},
    thumbnails::Thumbnails,
};

/// These constants are not named in the `windows` crate.
const WM_MOUSELEAVE: u32 = 0x02A3;
const HSHELL_WINDOWACTIVATED: usize = 4;
const HSHELL_RUDEAPPACTIVATED: usize = 0x8004;
const HSHELL_FLASH: usize = 0x8006;
const HSHELL_WINDOWDESTROYED: usize = 2;
/// A window's title or icon changed.
const HSHELL_REDRAW: usize = 6;
/// The keyboard language changed.
const HSHELL_LANGUAGE: usize = 8;
/// `wParam` of WM_SETTINGCHANGE when the work area changes. The dock's own
/// reservation causes this, so handling it would create a loop.
const SPI_SETWORKAREA: usize = 0x002F;
const WM_POWERBROADCAST: u32 = 0x0218;
const PBT_APMRESUMEAUTOMATIC: usize = 0x0012;
/// Sent when the charger is plugged in or out, and when energy saver turns
/// on or off.
const PBT_APMPOWERSTATUSCHANGE: usize = 0x000A;
const PBT_POWERSETTINGCHANGE: usize = 0x8013;

const WINDOW_CLASS: PCWSTR = muelle_shell::dock_link::DOCK_WINDOW_CLASS;
const APPBAR_CALLBACK: u32 = WM_APP + 1;
const TASKBAR_TAKEN: u32 = WM_APP + 2;
const BIN_CHANGED: u32 = WM_APP + 3;
/// Result of checking the recycle bin in the background (`wParam` = 1 if non-empty).
const BIN_STATE: u32 = WM_APP + 4;
/// Posted (not sent) when mouse capture is lost, so it is handled outside the
/// call that released it.
const DRAG_CANCELLED: u32 = WM_APP + 5;
/// Runs a file drop after OLE's `Drop` returns, so the drag source is not
/// blocked while files are copied or an app starts.
const DROP_RUN: u32 = WM_APP + 6;
/// Posted when apps are pinned to or unpinned from the Windows taskbar.
const PINS_CHANGED: u32 = WM_APP + 7;
const ASSETS_READY: u32 = WM_APP + 8;
/// The guard also checks graphics health while the scene is otherwise idle.
pub(crate) const HEALTH_CHECK: u32 = WM_APP + 9;
const TASKBAR_PREFS_CHANGED: u32 = WM_APP + 10;

/// Magnification range, in icons on either side of the cursor.
const RANGE_IN_ICONS: f32 = 3.0;
/// Speed of magnification entry and exit (about 200 ms).
const HOVER_OMEGA: f32 = 28.0;
/// Bounce height as a fraction of the icon size.
const BOUNCE_HEIGHT: f32 = 0.5;
const LAUNCH_BOUNCE_LIMIT: f32 = 10.0;
const ATTENTION_HOPS: f32 = 3.0;

// Dimensions are in logical pixels and are multiplied by the display scale.
/// Gap between the dock and the bottom screen edge, and between the dock and
/// maximized windows.
const FLOAT_MARGIN: f32 = 8.0;
const SHADOW_ROOM: f32 = 44.0;
const LABEL_ROOM: f32 = 60.0;
/// Widest room kept beside a side dock for an app's name, in logical pixels.
const SIDE_LABEL_MAX: f32 = 320.0;
/// Hysteresis distances: the effect starts at the first distance and ends at the second.
const ENTER_HALO: f32 = 5.0;
const LEAVE_HALO: f32 = 15.0;

/// Windows sends settings changes in bursts; coalesce them into one recomposition.
const REFRESH_TIMER: usize = 1;
const REFRESH_DELAY_MS: u32 = 120;
/// Window notifications also arrive in bursts when an app opens.
const SYNC_TIMER: usize = 2;
const SYNC_DELAY_MS: u32 = 40;
/// Minimized windows whose snapshot is kept to fly them back out of the dock.
/// Each costs a quarter of the window's pixels; older ones restore without
/// the genie.
const MAX_SNAPSHOTS: usize = 4;
/// After Explorer restarts or the system resumes, the new taskbar needs time
/// to apply its initial state before it can be taken over again.
const TASKBAR_TIMER: usize = 3;
const TASKBAR_DELAY_MS: u32 = 600;
const TASKBAR_MAX_ATTEMPTS: u8 = 50;
const TRIM_TIMER: usize = 4;
const TRIM_DELAY_MS: u32 = 3000;
const BIN_TIMER: usize = 5;
const BIN_DELAY_MS: u32 = 300;
const HIDE_TIMER: usize = 6;
const HIDE_DELAY_MS: u32 = 300;
/// Explorer writes a pin change in a few steps; read it once they settle.
const PINS_TIMER: usize = 7;
const PINS_DELAY_MS: u32 = 400;
/// Holding a drag over an open app brings its window to the front after this
/// long, so the drop can land in it: the Windows taskbar's drag-hover, and
/// macOS's spring-loaded Dock icons. Passing over icons does not.
const SPRING_TIMER: usize = 8;
const SPRING_DELAY_MS: u32 = 600;
/// While the Alt+Tab switcher is open, checks how long it stays: Windows
/// reports when it comes to the front, but not reliably when it closes.
/// A duration as timer milliseconds; Windows caps timers at about 24 days.
fn millis(duration: std::time::Duration) -> u32 {
    duration.as_millis().min(0x7FFF_FFFF) as u32
}

/// The background update runs once a day: a single timer, aimed at the
/// moment the next look is due (see `schedule_update`).
const UPDATE_TIMER: usize = 12;
const PREVIEW_TIMER: usize = 13;
/// While the crab is out (a few seconds): is its act over, is the pointer on it.
const MASCOT_TIMER: usize = 14;
const MASCOT_POLL_MS: u32 = 50;
const EDGE_TIMER: usize = 15;
/// The charger act at most this often, however many times it is plugged.
const CHARGING_AGAIN: std::time::Duration = std::time::Duration::from_secs(10 * 60);
const PREVIEW_DWELL_MS: u32 = 420;
const UPDATE_EVERY: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
/// After the dock starts, the look waits this long, so it never slows signing in.
const UPDATE_AFTER_START: std::time::Duration = std::time::Duration::from_secs(2 * 60);
/// While a full-screen app (a game, a presentation) is in front, try later.
const UPDATE_RETRY: std::time::Duration = std::time::Duration::from_secs(60 * 60);
const SWITCHER_TIMER: usize = 9;
/// Short enough to see every Tab press (a key stays down 50-100 ms).
const SWITCHER_POLL_MS: u32 = 16;
/// How long the switcher may take to show after coming to the front.
const SWITCHER_APPEAR: std::time::Duration = std::time::Duration::from_millis(400);
/// How soon after closing the switcher's window coming back means nothing.
const SWITCHER_ECHO: std::time::Duration = std::time::Duration::from_millis(150);
/// Recheck full-screen state while hidden in case Windows drops the exit event.
const FULLSCREEN_TIMER: usize = 10;
const FULLSCREEN_RECHECK_MS: u32 = 3_000;
/// Shell results arriving together cause one scene rebuild.
const ASSETS_TIMER: usize = 11;
const ASSETS_DELAY_MS: u32 = 16;
/// Non-activating windows cannot rely on capture outside their bounds.
/// Armed only while dragging or holding a draggable icon.
const DRAG_TIMER: usize = 16;
const DRAG_POLL_MS: u32 = 16;

/// Screen-edge strip that reveals a hidden dock, in physical pixels.
const REVEAL_STRIP: i32 = 2;
/// An icon released this far above the magnified dock, in icon heights, leaves it.
const REMOVE_DISTANCE: f32 = 0.8;
const REORDER_DURATION: std::time::Duration = std::time::Duration::from_millis(140);
/// A dragged app joins the icon under it (in a group) when released over
/// its middle, past this fraction of its width from either side; over its
/// sides, it moves between icons as usual.
const MERGE_SIDE: f32 = 0.25;

/// Longest hover label of a group, which lists its apps.
const GROUP_LABEL_CHARS: usize = 48;

const START_ID: &str = "muelle:start";
const WINDOW_ID_PREFIX: &str = "muelle:window:";
const WINDOW_TITLE_CHARS: usize = 40;
/// Fraction of a thumbnail slot covered by the app badge (see `scene`).
const BADGE_CORNER: f32 = crate::scene::BADGE_SIZE * crate::scene::BADGE_INSIDE;
const CMD_CLOSE_WINDOW: u32 = 8;
const CMD_SETTINGS: u32 = 9;

/// Context menu commands. Window commands start at `CMD_WINDOW`.
const CMD_OPEN: u32 = 1;
const CMD_TOGGLE_PIN: u32 = 2;
const CMD_REVEAL: u32 = 3;
const CMD_QUIT_APP: u32 = 4;
const CMD_EMPTY_BIN: u32 = 5;
const CMD_QUIT_DOCK: u32 = 6;
const CMD_TOGGLE_HIDING: u32 = 7;
const CMD_RUN_AS_ADMIN: u32 = 11;
const CMD_SHOW_DESKTOP: u32 = 12;
const CMD_TASK_MANAGER: u32 = 13;
const CMD_TASKBAR_SETTINGS: u32 = 14;
const CMD_UNGROUP: u32 = 15;
const CMD_REMOVE_FOLDER: u32 = 16;
const CMD_FORGET_RECENT: u32 = 17;
const CMD_END_TASK: u32 = 18;
const CMD_HIDE_FIXED: u32 = 19;
const CMD_WINDOW: u32 = 100;
/// Jump list entries start here, after any window command.
const CMD_JUMP: u32 = 1000;
const MENU_WINDOWS: usize = 12;
const MENU_TITLE_CHARS: usize = 60;

static SHELL_HOOK: OnceLock<u32> = OnceLock::new();
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();
/// Broadcast by the settings app after it saves the configuration.
static CONFIG_CHANGED: OnceLock<u32> = OnceLock::new();
/// Dock visual window, used to close it from the Ctrl+C handler running on
/// another thread.
static MAIN_WINDOW: AtomicIsize = AtomicIsize::new(0);
static DROP_WINDOW: AtomicIsize = AtomicIsize::new(0);
static MINIMIZE_HOOK: AtomicIsize = AtomicIsize::new(0);
static CLOAK_HOOK: AtomicIsize = AtomicIsize::new(0);
static FOREGROUND_HOOK: AtomicIsize = AtomicIsize::new(0);
static LOCATION_HOOK: AtomicIsize = AtomicIsize::new(0);
/// The window in front before the Alt+Tab switcher opened.
static LAST_FRONT: AtomicIsize = AtomicIsize::new(0);

/// One drive scan at a time; changes during a scan request one more pass.
struct BinQueryGate(AtomicU8);

impl BinQueryGate {
    const RUNNING: u8 = 1;
    const DIRTY: u8 = 2;

    const fn new() -> Self {
        Self(AtomicU8::new(0))
    }

    fn request(&self) -> bool {
        if self.0.fetch_or(Self::DIRTY, Ordering::AcqRel) & Self::RUNNING != 0 {
            return false;
        }
        self.0
            .compare_exchange(
                Self::DIRTY,
                Self::RUNNING | Self::DIRTY,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn begin_pass(&self) {
        self.0.fetch_and(!Self::DIRTY, Ordering::AcqRel);
    }

    fn needs_another_pass(&self) -> bool {
        self.0
            .compare_exchange(Self::RUNNING, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    }
}

static BIN_QUERY: BinQueryGate = BinQueryGate::new();

#[derive(Clone, PartialEq)]
struct Entry {
    kind: ItemKind,
    id: Option<String>,
    role: Role,
    running: bool,
    /// A group's apps, in order; empty for anything else. Part of the
    /// scene's key: the group's tile shows them.
    members: Vec<String>,
}

fn app_has_attention(attention: &Attention, windows: &[TrackedWindow], id: &str) -> bool {
    windows
        .iter()
        .any(|window| window.app_id == id && attention.contains(window.hwnd))
}

fn entry_has_attention(attention: &Attention, windows: &[TrackedWindow], entry: &Entry) -> bool {
    match entry.role {
        Role::App => entry
            .id
            .as_deref()
            .is_some_and(|id| app_has_attention(attention, windows, id)),
        Role::Group => entry
            .members
            .iter()
            .any(|id| app_has_attention(attention, windows, id)),
        Role::Window => entry
            .id
            .as_deref()
            .and_then(window_of)
            .is_some_and(|hwnd| attention.contains(hwnd)),
        _ => false,
    }
}

fn active_attention_window(windows: &[TrackedWindow], foreground: isize) -> isize {
    // Windows may briefly report its last minimized window as foreground.
    if windows
        .iter()
        .any(|window| window.hwnd == foreground && !window.minimized)
    {
        foreground
    } else {
        0
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    App,
    /// Pinned apps gathered in one icon; a click unfolds them.
    Group,
    Start,
    /// A minimized window, shown as a live thumbnail.
    Window,
    Folder,
    /// The shortcuts folder: a click opens its contents as a menu.
    Shortcuts,
    Trash,
}

fn window_id(hwnd: isize) -> String {
    format!("{WINDOW_ID_PREFIX}{hwnd}")
}

fn window_of(id: &str) -> Option<isize> {
    id.strip_prefix(WINDOW_ID_PREFIX)?.parse().ok()
}

/// Work deferred until the app state is released. These calls may open a modal
/// loop (a menu, shell error dialog, or recycle bin confirmation) that dispatches
/// messages back to the dock.
pub(crate) enum Outside {
    Open(String),
    OpenStart,
    Activate(Vec<isize>),
    Minimize(Vec<isize>),
    EndTask(PreparedEndTask),
    Reveal(String),
    EmptyBin,
    OpenWith(String, Vec<String>),
    Recycle(Vec<String>),
    Transfer {
        folder: String,
        files: Vec<String>,
        copy: bool,
    },
    Menu(MenuRequest),
    /// The shortcuts folder's contents, as a menu beside its icon.
    FolderMenu {
        folder: String,
        anchor: POINT,
        placement: menu::Placement,
        dark: bool,
    },
    InputMenu {
        gfx: Gfx,
        anchor: POINT,
        dark: bool,
    },
    QuitDock,
    OpenSettings,
    OpenAsAdmin(String),
    /// Unpin from the Windows taskbar too (see `pins::unpin_from_taskbar`).
    UnpinFromTaskbar(String),
    /// An entry of the jump list of the app `program`.
    OpenJump {
        target: jumplist::Target,
        program: String,
    },
    ShowDesktop,
}

pub(crate) struct MenuRequest {
    gfx: Gfx,
    target: MenuTarget,
    items: Vec<MenuItem>,
    anchor: POINT,
    placement: menu::Placement,
    dark: bool,
}

/// Preview positions follow discovery order, independently of activation's
/// z-order. Only open tracked windows are retained; a reopened window is new.
#[derive(Default)]
struct PreviewOrder(Vec<isize>);

impl PreviewOrder {
    fn update(&mut self, windows: &[TrackedWindow]) {
        self.0
            .retain(|hwnd| windows.iter().any(|window| window.hwnd == *hwnd));
        // EnumWindows returns topmost first. Append a newly discovered batch
        // back-to-front so its frontmost (usually newest) window goes last.
        for window in windows.iter().rev() {
            if !self.0.contains(&window.hwnd) {
                self.0.push(window.hwnd);
            }
        }
    }

    fn windows_of(&self, windows: &[TrackedWindow], app: &str) -> Vec<isize> {
        self.0
            .iter()
            .copied()
            .filter(|hwnd| {
                windows
                    .iter()
                    .any(|window| window.hwnd == *hwnd && window.app_id == app)
            })
            .collect()
    }
}

enum MenuTarget {
    Entry {
        id: String,
        role: Role,
        windows: Vec<isize>,
        end_task: Option<PreparedEndTask>,
        /// What each jump list command opens, from `CMD_JUMP` on.
        jump: Vec<jumplist::Target>,
    },
    Dock,
}

struct Asset {
    icon: Option<CompositionSurfaceBrush>,
    label: Option<LabelBrush>,
    /// A group's apps when its tile was drawn; empty for anything else.
    drawn_for: Vec<String>,
}

struct Press {
    id: String,
    at: POINT,
    draggable: bool,
}

struct Dragging {
    id: String,
    point: POINT,
    removing: bool,
    /// The pinned app or group it would join if released now.
    merge: Option<String>,
    /// Dragged out of an open group (see `start_group_drag`).
    from_group: bool,
}

/// What dropping the dragged files at a given place would do.
enum DropAction {
    Nothing,
    OpenWith(String),
    Recycle,
    Transfer { folder: String, copy: bool },
    Pin { id: String, before: Option<String> },
}

impl DropAction {
    fn effect(&self) -> DROPEFFECT {
        match self {
            Self::Nothing => DROPEFFECT_NONE,
            Self::OpenWith(_) => DROPEFFECT_COPY,
            Self::Recycle => DROPEFFECT_MOVE,
            Self::Transfer { copy, .. } => {
                if *copy {
                    DROPEFFECT_COPY
                } else {
                    DROPEFFECT_MOVE
                }
            }
            Self::Pin { .. } => DROPEFFECT_LINK,
        }
    }
}

struct Bounce {
    start: Instant,
    /// Seconds after `start` when bouncing ends. `None` means until the app opens a window.
    until: Option<f32>,
}

/// State that determines the scene; recomposition is unnecessary if it is unchanged.
#[derive(PartialEq)]
struct StageKey {
    screen: RECT,
    /// The screen edge the dock sits on.
    edge: DockEdge,
    /// The menu bar's strip at the top of this screen: a side dock is
    /// centred in the height below it.
    top_inset: i32,
    scale: f32,
    light: bool,
    glass: bool,
    bin_full: bool,
    auto_hide: bool,
    icon_size: u32,
    density: DockDensity,
    max_scale: f32,
    indicators: bool,
    entries: Vec<Entry>,
}

/// Where the switcher's selection lands once the windows marked `false` in
/// `alive` close: it stays at the same place in the list (the next window
/// takes the closed one's place), or on the new last one. `None` if none is
/// left.
fn selection_after_closing(index: usize, alive: &[bool]) -> Option<usize> {
    let left = alive.iter().filter(|&&on| on).count();
    if left == 0 {
        return None;
    }
    let closed_before = alive[..index.min(alive.len())]
        .iter()
        .filter(|&&on| !on)
        .count();
    Some((index - closed_before).min(left - 1))
}

fn group_drop_contains(rect: &Rect, x: f32, y: f32) -> bool {
    let side = rect.width * MERGE_SIDE;
    x >= rect.x + side && x <= rect.x + rect.width - side && y >= rect.y && y < rect.y + rect.height
}

pub(crate) fn primary_button_down() -> bool {
    // GetAsyncKeyState reports physical buttons; WM_LBUTTON* is logical.
    // Respect Windows' left-handed mouse configuration.
    // SAFETY: Read-only system metrics and button state.
    unsafe {
        let key = if GetSystemMetrics(SM_SWAPBUTTON) != 0 {
            VK_RBUTTON
        } else {
            VK_LBUTTON
        };
        GetAsyncKeyState(i32::from(key.0)) < 0
    }
}

fn group_is_foreground(windows: &[isize], foreground: isize) -> bool {
    if !windows.contains(&foreground) {
        return false;
    }
    // The last app can still be reported as foreground after minimizing.
    // Decide from its current native state, not the tracker snapshot or the
    // foreground handle alone, or the next click only minimizes it again.
    let hwnd = HWND(foreground as *mut _);
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(hwnd).as_bool()
            && !windows::Win32::UI::WindowsAndMessaging::IsIconic(hwnd).as_bool()
    }
}

fn power_act(
    before: Option<(Option<bool>, bool)>,
    now: (Option<bool>, bool),
    charging_allowed: bool,
) -> Option<mascot::Act> {
    let before = before?;
    // Unplugging can also enable energy saver. Keep the charger reaction
    // instead of replacing it with the less specific saver-only moment.
    if now.0 == Some(false) && before.0 == Some(true) {
        Some(mascot::Act::Unplugged)
    } else if now.1 && !before.1 {
        Some(mascot::Act::Sleepy)
    } else if now.0 == Some(true) && before.0 == Some(false) && charging_allowed {
        Some(mascot::Act::Charging)
    } else {
        None
    }
}

/// Where the Alt+Tab selection is believed to be.
struct SwitcherFollow {
    /// The windows the switcher lists, in its order.
    order: Vec<isize>,
    index: usize,
    tab_down: bool,
}

struct Stage {
    key: StageKey,
    window_zone: RECT,
    scene: Scene,
    reorder: Option<ReorderMotion>,
    layout: Layout,
    magnifier: Magnifier,
    icon: f32,
    gap: f32,
    scale: f32,
    /// The resting dock on the screen: maps dock coordinates (along, across)
    /// to screen pixels and back, whatever edge it is on.
    frame: DockFrame,
    /// The same, inside the visual window.
    window_frame: DockFrame,
    rest_zone: RECT,
    hover_zone: RECT,
    /// Thin strip at the screen edge that reveals the dock when auto-hide is on.
    edge_zone: RECT,
    /// How far the dock slides toward its screen edge to hide completely.
    slide_distance: f32,
}

impl Stage {
    /// Dock coordinates (along, across) of a screen point. Across is
    /// negative away from the screen edge.
    fn to_dock(&self, point: POINT) -> (f32, f32) {
        self.frame.to_dock(point.x as f32, point.y as f32)
    }

    /// The screen box of a box in dock coordinates.
    fn screen_rect(&self, rect: Rect) -> RECT {
        let rect = self.frame.rect(rect);
        RECT {
            left: rect.x.round() as i32,
            top: rect.y.round() as i32,
            right: (rect.x + rect.width).round() as i32,
            bottom: (rect.y + rect.height).round() as i32,
        }
    }

    /// The screen point `gap` pixels off the box `rect` (dock coordinates)
    /// on its side away from the screen edge, centred: where menus and
    /// popups for an icon hang.
    fn beside(&self, rect: Rect, gap: i32) -> POINT {
        let r = self.frame.rect(rect);
        let (middle_x, middle_y) = (
            (r.x + r.width / 2.0).round() as i32,
            (r.y + r.height / 2.0).round() as i32,
        );
        match self.frame.edge {
            DockEdge::Bottom => POINT {
                x: middle_x,
                y: r.y.round() as i32 - gap,
            },
            DockEdge::Left => POINT {
                x: (r.x + r.width).round() as i32 + gap,
                y: middle_y,
            },
            DockEdge::Right => POINT {
                x: r.x.round() as i32 - gap,
                y: middle_y,
            },
        }
    }

    /// Which way a menu or popup for an icon opens: away from the edge.
    fn menu_placement(&self) -> menu::Placement {
        match self.frame.edge {
            DockEdge::Bottom => menu::Placement::Above,
            DockEdge::Left => menu::Placement::Right,
            DockEdge::Right => menu::Placement::Left,
        }
    }

    /// Where the dock starts along the screen axis it runs on.
    fn along_origin(&self) -> f32 {
        if self.frame.edge.is_vertical() {
            self.frame.origin.1
        } else {
            self.frame.origin.0
        }
    }

    /// Whether `point` is within the span of the reveal strip (the pointer
    /// came out of it toward the dock rather than past its end).
    fn spans_reveal(&self, point: POINT) -> bool {
        let zone = self.edge_zone;
        if self.frame.edge.is_vertical() {
            point.y >= zone.top && point.y < zone.bottom
        } else {
            point.x >= zone.left && point.x < zone.right
        }
    }
}

struct ReorderMotion {
    started: Instant,
    offsets: Vec<f32>,
}

pub(crate) struct App {
    visual: HWND,
    input: HWND,
    gfx: Gfx,
    target: DesktopWindowTarget,
    appbar: Option<AppBar>,
    taskbar_worker: Option<thread::JoinHandle<()>>,
    taskbar_retry: bool,
    taskbar_attempts: u8,
    fullscreen: bool,
    roster: Roster,
    tracker: WindowTracker,
    windows: Vec<TrackedWindow>,
    preview_order: PreviewOrder,
    /// Where each window was last seen on screen while not maximized, for
    /// the genie of snapped windows (see `tracker::rect_before_minimize`).
    last_rects: HashMap<isize, RECT>,
    entries: Vec<Entry>,
    icons: Visuals,
    downloads: Option<String>,
    bin_full: bool,
    _bin_watch: Option<BinWatch>,
    /// Energy saver on or off: glass follows (see `system::transparency_enabled`).
    _saver_watch: power::SaverWatch,
    /// Hears pins and unpins on the Windows taskbar.
    pin_watch: Option<muelle_shell::watch::RegistryWatch>,
    badge_watch: Option<muelle_shell::watch::RegistryWatch>,
    attention: Attention,
    attention_enabled: bool,
    assets: HashMap<String, Asset>,
    /// Scale, theme and icon pixel size the cached assets were drawn for.
    /// Scale, theme, icon size and dock edge the assets were drawn for
    /// (labels point at their icon, so they depend on the edge).
    assets_for: (f32, bool, u32, DockEdge),
    placeholder: Option<CompositionSurfaceBrush>,
    remove_label: Option<LabelBrush>,
    config: Config,
    /// 0 = shown, 1 = hidden (auto-hide).
    slide: Spring,
    press: Option<Press>,
    drag: Option<Dragging>,
    /// Files being dragged over the dock from another app.
    incoming: Vec<String>,
    /// What dropping `incoming` between icons would pin.
    incoming_pin: Option<String>,
    /// Something (files, text, an image) is being dragged over the dock.
    drag_over: bool,
    /// The entry a drag is resting on, which springs open on `SPRING_TIMER`.
    spring: Option<String>,
    /// The Alt+Tab switcher is open; an auto-hidden dock shows meanwhile.
    switching: bool,
    switch_started: Instant,
    switch_ended: Option<Instant>,
    /// When the dock started (the update waits a little after it).
    launched: Instant,
    /// When this dock last started a background update.
    update_tried: Option<Instant>,
    /// The selection in the switcher, as far as it can be followed.
    follow: Option<SwitcherFollow>,
    /// The magnifier rests on the icon selected in Alt+Tab, not the pointer.
    pointed: bool,
    pending_drop: Option<Outside>,
    bounces: HashMap<String, Bounce>,
    notifications: muelle_core::notifications::NotificationQueue,
    /// Minimized windows, in the order they were minimized.
    minimized: Vec<isize>,
    thumbnails: Thumbnails,
    /// Genie animations in progress.
    flights: Vec<Flight>,
    /// What the windows minimized with the genie looked like, oldest first.
    snapshots: Vec<(isize, Snapshot)>,
    menubar: Option<MenuBar>,
    edge_watch: Option<EdgeWatch>,
    /// Screen point identifying the last display chosen by a bottom-edge dwell.
    dock_display: Option<POINT>,
    /// Display names already looked up, by app id.
    names: HashMap<String, String>,
    /// Windows whose system transitions are turned off (see `calm_windows`).
    calmed: Vec<isize>,
    /// Windows already minimized when the dock starts get no animation.
    started: bool,
    stage: Option<Stage>,
    hovering: bool,
    hovered: Option<usize>,
    preview_candidate: Option<String>,
    preview: Option<MultiWindowView>,
    /// A group's apps, unfolded above it after a click.
    group_view: Option<GroupView>,
    /// The group whose view a press on the dock closed: releasing on that
    /// group then leaves it closed rather than opening it again.
    group_was_open: Option<String>,
    /// The icon following the pointer while an app is dragged out of a group.
    ghost: Option<DragGhost>,
    menu_open: bool,
    /// The crab, while it is out (see `perform`).
    mascot: Option<mascot::Mascot>,
    /// Charging and energy saver at the last power change.
    power_seen: Option<(Option<bool>, bool)>,
    /// When the charger act last played.
    charging_shown: Option<Instant>,
    /// Horizontal cursor position relative to the resting dock.
    cursor: f32,
    /// 0 = resting, 1 = full magnification.
    amount: Spring,
    last_frame: Instant,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn explorer_id() -> String {
    normalize_id(&format!(
        "{}\\explorer.exe",
        env::var("WINDIR").unwrap_or_default()
    ))
}

/// Saved settings or, on first launch, the defaults with the apps pinned to
/// the Windows taskbar.
fn load_config() -> Config {
    let path = muelle_config::default_path();
    match muelle_config::load(&path) {
        Ok(Some(config)) => config,
        Ok(None) => {
            let pins = taskbar_pins();
            let config = Config {
                pinned: pins.clone(),
                taskbar_pins: Some(pins),
                ..Config::default()
            };
            match muelle_config::update(&path, |saved| Ok(saved.unwrap_or_else(|| config.clone())))
            {
                Ok(saved) => saved,
                Err(error) => {
                    eprintln!("muelle: {error}");
                    config
                }
            }
        }
        Err(error) => {
            // Do not overwrite it; the user may want to repair the file.
            eprintln!("muelle: {error}; se usan los valores por defecto");
            Config::default()
        }
    }
}

/// Apply only Explorer's built-in pin, preserving the order of all other apps.
fn apply_explorer_pin(roster: &mut Roster, pinned: bool) {
    let explorer = explorer_id();
    if pinned {
        let before = roster.pinned().iter().find(|id| **id != explorer).cloned();
        roster.pin_before(&explorer, before.as_deref());
    } else {
        roster.unpin(&explorer);
        roster.forget_recent(&explorer);
    }
}

fn roster_from(config: &Config) -> Roster {
    let explorer = explorer_id();
    let mut pinned = config.pinned.clone();
    pinned.retain(|id| *id != explorer);
    let groups = config
        .groups
        .iter()
        .map(|group| Group {
            id: group.id.clone(),
            apps: group
                .apps
                .iter()
                .filter(|id| **id != explorer)
                .cloned()
                .collect(),
        })
        .collect();
    let mut roster = Roster::new(pinned, config.recent.clone()).with_groups(groups);
    apply_explorer_pin(&mut roster, config.pin_explorer);
    roster
}

fn save_settings(roster: &Roster, config: &Config) {
    if let Err(error) = save_dock_config(roster, config, false) {
        eprintln!("muelle: {error}");
    }
}

fn save_dock_config(roster: &Roster, config: &Config, toggle_hide: bool) -> Result<Config> {
    let saved = muelle_config::update(&muelle_config::default_path(), |saved| {
        let mut saved = saved.unwrap_or_else(|| config.clone());
        saved.pinned = roster.pinned().to_vec();
        saved.groups = roster
            .groups()
            .iter()
            .map(|group| DockGroup {
                id: group.id.clone(),
                apps: group.apps.clone(),
            })
            .collect();
        saved.recent = roster.recent().to_vec();
        saved.taskbar_pins.clone_from(&config.taskbar_pins);
        if toggle_hide {
            saved.auto_hide = !saved.auto_hide;
        }
        Ok(saved)
    })?;
    Ok(saved)
}

/// Takes the shortcuts folder out of the dock. Only that preference changes
/// on disk: Settings owns the others and may have changed them since.
fn forget_shortcuts_folder(config: &Config) -> Result<()> {
    muelle_config::update(&muelle_config::default_path(), |saved| {
        let mut saved = saved.unwrap_or_else(|| config.clone());
        saved.shortcuts_folder = None;
        Ok(saved)
    })?;
    Ok(())
}

fn fixed_item_preference(role: Role) -> Option<&'static str> {
    match role {
        Role::Start => Some("showStart"),
        Role::Folder => Some("showDownloads"),
        Role::Trash => Some("showTrash"),
        _ => None,
    }
}

/// Hides only the shortcut. No folder, file or Windows pin is removed.
fn hide_fixed_item(path: &std::path::Path, role: Role) -> Result<Config> {
    let key = fixed_item_preference(role).context("el elemento no es un acceso fijo")?;
    let patch = serde_json::Map::from_iter([(key.to_owned(), serde_json::Value::Bool(false))]);
    Ok(muelle_config::update_preferences(path, patch)?)
}

fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        let cut: String = text.chars().take(max - 1).collect();
        format!("{}…", cut.trim_end())
    }
}

fn place(hwnd: HWND, zone: &RECT, visible: bool) -> Result<()> {
    let show = if visible {
        SWP_SHOWWINDOW
    } else {
        SWP_HIDEWINDOW
    };
    // SAFETY: `hwnd` is one of our live windows.
    unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            zone.left,
            zone.top,
            zone.right - zone.left,
            zone.bottom - zone.top,
            SWP_NOACTIVATE | show,
        )?;
    }
    Ok(())
}

pub fn request_quit() {
    // SAFETY: Searches for the window and posts a message with no associated data.
    unsafe {
        if let Ok(hwnd) = FindWindowW(WINDOW_CLASS, None) {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

unsafe extern "system" fn on_console_ctrl(event: u32) -> BOOL {
    let hwnd = MAIN_WINDOW.load(Ordering::Relaxed);
    if event == CTRL_C_EVENT && hwnd != 0 {
        // SAFETY: Only a message is sent; if the window no longer exists, the call has no effect.
        unsafe {
            let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        return true.into();
    }
    false.into()
}

impl App {
    /// List of items: apps, recents, Downloads, and recycle bin, separated into
    /// sections as in macOS.
    fn current_entries(&self) -> Vec<Entry> {
        let separator = || Entry {
            kind: ItemKind::Separator,
            id: None,
            role: Role::Folder,
            running: false,
            members: Vec::new(),
        };
        let icon = |id: &str, role: Role, running: bool| Entry {
            kind: ItemKind::Icon,
            id: Some(id.to_owned()),
            role,
            running,
            members: Vec::new(),
        };

        let mut entries: Vec<Entry> = self
            .roster
            .apps()
            .iter()
            .map(|app| match self.roster.group(&app.id) {
                Some(group) => Entry {
                    members: group.apps.clone(),
                    ..icon(&app.id, Role::Group, app.running)
                },
                None => icon(&app.id, Role::App, app.running),
            })
            .collect();
        if self.config.show_start {
            entries.insert(0, icon(START_ID, Role::Start, false));
        }
        if self.config.show_recents && !self.roster.recent().is_empty() {
            entries.push(separator());
            entries.extend(
                self.roster
                    .recent()
                    .iter()
                    .map(|id| icon(id, Role::App, false)),
            );
        }
        entries.push(separator());
        // Unless "show minimized windows" is on, minimized windows hide in
        // their app's icon instead of getting a thumbnail here.
        if self.config.show_minimized {
            entries.extend(
                self.minimized
                    .iter()
                    .map(|&hwnd| icon(&window_id(hwnd), Role::Window, false)),
            );
        }
        if let Some(folder) = &self.config.shortcuts_folder {
            entries.push(icon(folder, Role::Shortcuts, false));
        }
        if self.config.show_downloads
            && let Some(downloads) = &self.downloads
        {
            entries.push(icon(downloads, Role::Folder, false));
        }
        if self.config.show_trash {
            entries.push(icon(recycle_bin::PARSING_NAME, Role::Trash, false));
        }
        // Hiding fixed items can leave a section empty. Never leave loose dividers.
        let mut previous_separator = true;
        entries.retain(|entry| {
            let separator = entry.kind == ItemKind::Separator;
            let keep = !separator || !previous_separator;
            previous_separator = separator;
            keep
        });
        while entries
            .last()
            .is_some_and(|entry| entry.kind == ItemKind::Separator)
        {
            entries.pop();
        }
        entries
    }

    /// `wanted` is the pixel side icons are drawn at: their magnified size.
    fn prepare_assets(&mut self, wanted: u32, scale: f32, light: bool, edge: DockEdge) {
        if self.assets_for != (scale, light, wanted, edge) {
            self.assets.clear();
            self.remove_label = None;
            self.placeholder = None;
            self.assets_for = (scale, light, wanted, edge);
        }
        if self.remove_label.is_none() {
            self.remove_label = self.gfx.label_brush(text().remove, scale, light, edge).ok();
        }
        if self.placeholder.is_none() {
            self.placeholder = self.gfx.placeholder_brush(wanted, light).ok();
        }
        for entry in &self.entries {
            let Some(id) = entry.id.as_ref() else {
                continue;
            };
            if entry.role == Role::Group {
                if self
                    .assets
                    .get(id)
                    .is_some_and(|asset| asset.drawn_for == entry.members)
                {
                    continue;
                }
                // Every app is asked for, not only the four on the tile: the
                // unfolded group shows them all.
                for member in &entry.members {
                    let _ = self.icons.get(member, "");
                }
                let images: Vec<Option<&IconImage>> = entry
                    .members
                    .iter()
                    .take(4)
                    .map(|member| self.icons.peek(member, "").map(|visual| &visual.icon))
                    .collect();
                let icon = self
                    .gfx
                    .group_brush(wanted, &images, entry.members.len(), light)
                    .inspect_err(|error| eprintln!("muelle: icono del grupo `{id}`: {error:#}"))
                    .ok()
                    .or_else(|| self.placeholder.clone());
                let names: Vec<String> = entry
                    .members
                    .iter()
                    .map(|member| {
                        self.icons
                            .peek(member, "")
                            .map_or_else(|| fallback_name(member), |visual| visual.name.clone())
                    })
                    .collect();
                let label = self
                    .gfx
                    .label_brush(
                        &shorten(&names.join(", "), GROUP_LABEL_CHARS),
                        scale,
                        light,
                        edge,
                    )
                    .inspect_err(|error| eprintln!("muelle: etiqueta del grupo `{id}`: {error:#}"))
                    .ok();
                self.assets.insert(
                    id.clone(),
                    Asset {
                        icon,
                        label,
                        drawn_for: entry.members.clone(),
                    },
                );
                continue;
            }
            let variant = if id == recycle_bin::PARSING_NAME {
                if self.bin_full {
                    recycle_bin::FULL
                } else {
                    recycle_bin::EMPTY
                }
            } else {
                ""
            };
            let visual = if id == START_ID || window_of(id).is_some() {
                None
            } else {
                self.icons.get(id, variant)
            };
            if self.assets.contains_key(id) {
                continue;
            }
            if let Some(hwnd) = window_of(id) {
                let title = shorten(&tracker::title(hwnd), WINDOW_TITLE_CHARS);
                let label = self.gfx.label_brush(&title, scale, light, edge).ok();
                self.assets.insert(
                    id.clone(),
                    Asset {
                        icon: None,
                        label,
                        drawn_for: Vec::new(),
                    },
                );
                continue;
            }
            if id == START_ID {
                let icon = self.gfx.start_brush(wanted).ok();
                let label = self.gfx.label_brush(text().start, scale, light, edge).ok();
                self.assets.insert(
                    id.clone(),
                    Asset {
                        icon,
                        label,
                        drawn_for: Vec::new(),
                    },
                );
                continue;
            }
            // A missing icon should not prevent the rest of the dock from rendering.
            let icon = visual
                .and_then(|visual| {
                    let side = wanted.min(visual.icon.width);
                    self.gfx
                        .icon_brush(&visual.icon, side)
                        .inspect_err(|error| eprintln!("muelle: icono de `{id}`: {error:#}"))
                        .ok()
                })
                .or_else(|| self.placeholder.clone());
            let name = visual.map_or_else(|| fallback_name(id), |visual| visual.name.clone());
            let label = self
                .gfx
                .label_brush(&name, scale, light, edge)
                .inspect_err(|error| eprintln!("muelle: etiqueta de `{id}`: {error:#}"))
                .ok();
            self.assets.insert(
                id.clone(),
                Asset {
                    icon,
                    label,
                    drawn_for: Vec::new(),
                },
            );
        }
        let entries = &self.entries;
        self.assets
            .retain(|id, _| entries.iter().any(|entry| entry.id.as_ref() == Some(id)));
        let windows = &self.windows;
        let explorer = explorer_id();
        // The apps in groups keep their icons: their group's tile and its
        // unfolded view draw them.
        self.icons.retain(|id| {
            id == explorer
                || entries.iter().any(|entry| {
                    entry.id.as_deref() == Some(id) || entry.members.iter().any(|app| app == id)
                })
                || windows.iter().any(|window| window.app_id == id)
        });
    }

    fn assets_ready(&mut self) -> Result<()> {
        let loaded = self.icons.collect();
        if loaded.is_empty() {
            return Ok(());
        }
        for id in &loaded {
            self.assets.remove(id);
            self.names.remove(id);
        }
        // A group's tile and label show its apps' icons and names.
        let groups: Vec<String> = self
            .entries
            .iter()
            .filter(|entry| entry.members.iter().any(|app| loaded.contains(app)))
            .filter_map(|entry| entry.id.clone())
            .collect();
        for id in &groups {
            self.assets.remove(id);
        }
        self.stage = None;
        self.refresh()?;
        self.update_menu_bar_app()
    }

    /// Recalculates sizes, positions, and scene for the current state. Does
    /// nothing if the scene inputs are unchanged, since many Windows notifications
    /// do not affect the dock.
    fn refresh(&mut self) -> Result<()> {
        if let Some(bar) = &mut self.menubar
            && let Err(error) = bar.refresh(&self.gfx)
        {
            crate::recovery::graphics_error(&error);
            muelle_shell::log::line(&format!("actualización de la barra superior: {error:#}"));
        }
        let monitor = self
            .dock_monitor()
            .context("no se pudo consultar el monitor del dock")?;
        let edge = self.position();
        let key = StageKey {
            screen: monitor.screen,
            edge,
            top_inset: self.menu_bar_inset(monitor.screen, edge),
            scale: monitor.scale,
            light: uses_light_theme(),
            // A backdrop brush made while Windows had transparency off stays
            // black after it comes back, so this rebuilds the dock both ways.
            glass: transparency_enabled(),
            bin_full: self.bin_full,
            auto_hide: self.config.auto_hide,
            icon_size: self.config.icon_size,
            density: self.config.dock_density,
            max_scale: self.config.max_scale(),
            indicators: self.config.show_indicators,
            entries: self.current_entries(),
        };
        if self.stage.as_ref().is_some_and(|stage| stage.key == key) {
            return Ok(());
        }
        self.close_preview();
        // An unfolded group shows its apps as they were: it closes if they changed.
        if let Some(view) = &self.group_view
            && self
                .roster
                .group(&view.group_id)
                .is_none_or(|group| group.apps.as_slice() != view.apps())
        {
            self.close_group_view();
        }

        let scale = monitor.scale;
        let max_scale = key.max_scale;
        let (gap, padding) = key.density.spacing();
        let mut metrics = Metrics {
            icon: key.icon_size as f32,
            gap,
            padding,
            ..Metrics::DEFAULT
        }
        .scaled(scale);
        let theme = Theme::for_system(key.light, key.glass);
        let px = |logical: f32| (logical * scale).round();
        let screen = monitor.screen;
        let screen_width = (screen.right - screen.left) as f32;
        // On a side, the dock runs down the height left under the menu bar.
        let span_start = (screen.top + key.top_inset) as f32;
        let length_room = if edge.is_vertical() {
            screen.bottom as f32 - span_start
        } else {
            screen_width
        };
        let kinds: Vec<ItemKind> = key.entries.iter().map(|entry| entry.kind).collect();
        let mut dock = layout(&kinds, &metrics);
        let available = (length_room - 2.0 * px(FLOAT_MARGIN)).max(1.0);
        // A side dock often fills the height. Magnified, it spreads by up to
        // half its growth past each end, which would put the first icon
        // under the menu bar and the last off screen: room is kept for it.
        // It scales with the icons, so one ratio fits both exactly.
        let spread = if edge.is_vertical() {
            Magnifier {
                range: RANGE_IN_ICONS * (metrics.icon + metrics.gap),
                max_scale,
            }
            .max_growth()
        } else {
            0.0
        };
        if dock.width + spread > available {
            // Keep the roster reachable on a smaller display without
            // changing the user's saved icon size.
            metrics = metrics.scaled(available / (dock.width + spread));
            dock = layout(&kinds, &metrics);
        }
        dock.width = dock.width.round();
        dock.height = dock.height.round();

        self.entries = key.entries.clone();
        self.prepare_assets(
            (metrics.icon * max_scale).round().max(1.0) as u32,
            scale,
            theme.light,
            edge,
        );

        let magnifier = Magnifier {
            range: RANGE_IN_ICONS * (metrics.icon + metrics.gap),
            max_scale,
        };

        let side_growth = (magnifier.max_growth() / 2.0).ceil();
        let rise = (metrics.icon * (max_scale - 1.0)).ceil();

        // Position relative to the screen, not the work area; the work area
        // already excludes the strip reserved by the dock.
        let margin = px(FLOAT_MARGIN);
        let centred = span_start + ((length_room - dock.width) / 2.0).round();
        let (dock_x, dock_y) = match edge {
            DockEdge::Bottom => (
                screen.left as f32 + ((screen_width - dock.width) / 2.0).round(),
                screen.bottom as f32 - margin - dock.height,
            ),
            DockEdge::Left => (screen.left as f32 + margin, centred),
            DockEdge::Right => (screen.right as f32 - margin - dock.height, centred),
        };
        let frame = DockFrame::new(edge, (dock_x, dock_y), dock.height);
        // A hidden dock leaves the whole screen to maximized windows.
        let strip = if self.config.auto_hide {
            0
        } else {
            (dock.height + 2.0 * margin) as i32
        };
        // The resting dock's box on screen: across the edge it is as deep as
        // the layout is tall.
        let (box_width, box_height) = if edge.is_vertical() {
            (dock.height, dock.width)
        } else {
            (dock.width, dock.height)
        };
        let (dock_right, dock_bottom) = (dock_x + box_width, dock_y + box_height);
        // Room around the dock, in its own terms: before and after it along
        // the edge, inward (where icons grow and labels show) and outward.
        // Outward never passes the screen edge: a display beyond it must not
        // receive the sliding dock or its mouse input while it slides.
        let zone = move |before: f32, inward: f32, after: f32, outward: f32| match edge {
            DockEdge::Bottom => RECT {
                left: (dock_x - before) as i32,
                top: (dock_y - inward) as i32,
                right: (dock_right + after) as i32,
                bottom: ((dock_bottom + outward) as i32).min(screen.bottom),
            },
            DockEdge::Left => RECT {
                left: ((dock_x - outward) as i32).max(screen.left),
                top: (dock_y - before) as i32,
                right: (dock_right + inward) as i32,
                bottom: (dock_bottom + after) as i32,
            },
            DockEdge::Right => RECT {
                left: (dock_x - inward) as i32,
                top: (dock_y - before) as i32,
                right: ((dock_right + outward) as i32).min(screen.right),
                bottom: (dock_bottom + after) as i32,
            },
        };

        let room = px(SHADOW_ROOM);
        // Labels show inward of the icon: above it on the bottom, beside it
        // on a side, where they need their full width.
        let label_room = if edge.is_vertical() {
            let widest = self
                .assets
                .values()
                .filter_map(|asset| asset.label.as_ref())
                .chain(&self.remove_label)
                .map(|label| label.width)
                .fold(0.0_f32, f32::max);
            px(12.0) + widest.min(px(SIDE_LABEL_MAX))
        } else {
            px(LABEL_ROOM)
        };
        let (room_x, room_top) = (room + side_growth, rise + label_room);
        let window_zone = zone(room_x, room_top, room_x, room);
        let window_frame = DockFrame::new(
            edge,
            (
                dock_x - window_zone.left as f32,
                dock_y - window_zone.top as f32,
            ),
            dock.height,
        );
        let contents: Vec<SlotContent<'_>> = self
            .entries
            .iter()
            .map(|entry| {
                let asset = entry.id.as_ref().and_then(|id| self.assets.get(id));
                let badge = entry
                    .id
                    .as_deref()
                    .and_then(window_of)
                    .and_then(|hwnd| self.windows.iter().find(|window| window.hwnd == hwnd))
                    .and_then(|window| self.assets.get(&window.app_id))
                    .and_then(|asset| asset.icon.as_ref());
                SlotContent {
                    icon: asset.and_then(|asset| asset.icon.as_ref()),
                    label: asset.and_then(|asset| asset.label.as_ref()),
                    running: entry.running && self.config.show_indicators,
                    badge,
                }
            })
            .collect();
        let scene = Scene::build(
            &self.gfx,
            &SceneInput {
                layout: &dock,
                contents: &contents,
                theme,
                scale,
                frame: window_frame,
                remove_label: self.remove_label.as_ref(),
            },
        )?;
        let reorder = self.stage.as_ref().and_then(|previous| {
            // Do not interpolate between monitors or on a theme-only rebuild.
            if self.fullscreen
                || previous.key.screen != key.screen
                || previous.key.edge != key.edge
                || previous.scale != scale
                || previous
                    .key
                    .entries
                    .iter()
                    .map(|entry| &entry.id)
                    .eq(key.entries.iter().map(|entry| &entry.id))
                || !animations_enabled()
            {
                return None;
            }
            let state = magnify(&dock, self.cursor, self.amount.value, &magnifier);
            let offsets: Vec<f32> = key
                .entries
                .iter()
                .zip(&state.rects)
                .map(|(entry, rect)| {
                    let Some(id) = entry.id.as_deref() else {
                        return 0.0;
                    };
                    if self
                        .drag
                        .as_ref()
                        .is_some_and(|drag| drag.id == id && !drag.from_group)
                    {
                        return 0.0;
                    }
                    let Some(index) = previous
                        .key
                        .entries
                        .iter()
                        .position(|entry| entry.id.as_deref() == Some(id))
                    else {
                        return 0.0;
                    };
                    // Start from the currently drawn icon, including any earlier
                    // move still in progress when the user crosses another slot.
                    let along = if edge.is_vertical() { dock_y } else { dock_x };
                    previous.scene.slot_along(index).map_or(0.0, |drawn| {
                        previous.along_origin() + drawn - along - rect.x
                    })
                })
                .collect();
            offsets
                .iter()
                .any(|offset| offset.abs() >= 0.5)
                .then(|| ReorderMotion {
                    started: Instant::now(),
                    offsets,
                })
        });
        self.target.SetRoot(&scene.root)?;

        let (enter, leave) = (px(ENTER_HALO), px(LEAVE_HALO));
        let stage = Stage {
            key,
            window_zone,
            scene,
            reorder,
            magnifier,
            icon: metrics.icon,
            gap: metrics.gap,
            scale,
            frame,
            window_frame,
            rest_zone: zone(enter, enter, enter, enter),
            hover_zone: zone(
                side_growth + leave,
                rise + leave,
                side_growth + leave,
                leave,
            ),
            // The thin strip at the screen edge that reveals a hidden dock,
            // as long as the dock (plus how far its icons spread).
            edge_zone: match edge {
                DockEdge::Bottom => RECT {
                    left: (dock_x - side_growth) as i32,
                    top: screen.bottom - REVEAL_STRIP,
                    right: (dock_right + side_growth) as i32,
                    bottom: screen.bottom,
                },
                DockEdge::Left => RECT {
                    left: screen.left,
                    top: (dock_y - side_growth) as i32,
                    right: screen.left + REVEAL_STRIP,
                    bottom: (dock_bottom + side_growth) as i32,
                },
                DockEdge::Right => RECT {
                    left: screen.right - REVEAL_STRIP,
                    top: (dock_y - side_growth) as i32,
                    right: screen.right,
                    bottom: (dock_bottom + side_growth) as i32,
                },
            },
            slide_distance: dock.height + px(FLOAT_MARGIN) + room,
            layout: dock,
        };
        self.stage = Some(stage);
        self.sync_badge_visuals()?;
        self.place_windows()?;
        self.schedule_trim();
        self.render()?;
        // Last: reserving space is a synchronous call to Explorer (40-60 ms
        // measured), and the dock is already on screen meanwhile.
        if let Some(appbar) = &mut self.appbar {
            // Auto-hide reserves zero height. Moving the dock must never
            // change the destination monitor's work area.
            let reservation = if self.config.auto_hide {
                primary_monitor().map_or(screen, |primary| primary.screen)
            } else {
                screen
            };
            let side = match edge {
                DockEdge::Bottom => Edge::Bottom,
                DockEdge::Left => Edge::Left,
                DockEdge::Right => Edge::Right,
            };
            appbar.reserve(side, reservation, strip);
        }
        Ok(())
    }

    fn place_windows(&self) -> Result<()> {
        let Some(stage) = &self.stage else {
            return Ok(());
        };
        let visible = !self.fullscreen;
        place(self.visual, &stage.window_zone, visible)?;
        // Preserve the expanded input area if the cursor is already over the
        // dock (for example, after launching an app with a dock click).
        let input_zone = if self.hovering || self.drag.is_some() {
            &stage.hover_zone
        } else if self.config.auto_hide && self.slide.target == 1.0 {
            &stage.edge_zone
        } else {
            &stage.rest_zone
        };
        place(self.input, input_zone, visible)
    }

    fn schedule_trim(&self) {
        // SAFETY: `self.input` is this live window, and this call runs on its UI thread.
        unsafe { SetTimer(Some(self.input), TRIM_TIMER, TRIM_DELAY_MS, None) };
    }

    fn trim_if_idle(&self) {
        if !self.hovering && !self.is_animating() {
            power::trim_memory();
        }
    }

    fn render(&mut self) -> Result<()> {
        let Some(stage) = &mut self.stage else {
            return Ok(());
        };
        let mut state = magnify(
            &stage.layout,
            self.cursor,
            self.amount.value,
            &stage.magnifier,
        );
        let now = Instant::now();
        if let Some(motion) = &stage.reorder {
            let elapsed = now.saturating_duration_since(motion.started);
            if elapsed >= REORDER_DURATION {
                stage.reorder = None;
            } else {
                let remaining = 1.0 - elapsed.as_secs_f32() / REORDER_DURATION.as_secs_f32();
                let ease = remaining * remaining;
                for (rect, offset) in state.rects.iter_mut().zip(&motion.offsets) {
                    rect.x += offset * ease;
                }
            }
        }
        self.hovered = (self.hovering || self.pointed)
            .then(|| state.icon_at(&stage.layout, self.cursor, stage.gap))
            .flatten();

        let lift_of = |id: &str| {
            self.bounces.get(id).map_or(0.0, |bounce| {
                let elapsed = now.duration_since(bounce.start).as_secs_f32();
                bounce::lift(elapsed, stage.icon * BOUNCE_HEIGHT)
            })
        };
        // A group bounces for any of its apps (opening, asking for attention).
        let lifts: Vec<f32> = self
            .entries
            .iter()
            .map(|entry| match entry.role {
                Role::Group => entry
                    .members
                    .iter()
                    .map(|app| lift_of(app.as_str()))
                    .fold(0.0, f32::max),
                _ => entry.id.as_deref().map_or(0.0, lift_of),
            })
            .collect();

        let drag = self.drag.as_ref().and_then(|drag| {
            Some(Drag {
                index: self
                    .entries
                    .iter()
                    .position(|entry| entry.id.as_deref() == Some(drag.id.as_str()))?,
                center: stage.to_dock(drag.point),
                removing: drag.removing,
                ghost: drag.from_group,
            })
        });
        let target = self
            .drag
            .as_ref()
            .and_then(|drag| drag.merge.as_deref())
            .and_then(|merge| {
                self.entries
                    .iter()
                    .position(|entry| entry.id.as_deref() == Some(merge))
            });
        if drag.is_some() {
            self.hovered = None;
        }
        // The open group's name would cover its unfolded apps.
        let open_group = self.group_view.as_ref().and_then(|view| {
            self.entries
                .iter()
                .position(|entry| entry.id.as_deref() == Some(view.group_id.as_str()))
        });
        let at_rest = self.amount.value == 0.0
            && self.bounces.is_empty()
            && self.slide.is_settled()
            && stage.reorder.is_none()
            && drag.is_none();
        let slide = self.slide.value * stage.slide_distance;
        stage.scene.apply(&Frame {
            state: &state,
            hovered: self.hovered.filter(|&index| {
                self.preview.is_none() && !self.menu_open && open_group != Some(index)
            }),
            lifts: &lifts,
            drag,
            target,
            slide,
            snap: at_rest,
        })?;

        let visible = !self.fullscreen;
        for (entry, rect) in self.entries.iter().zip(&state.rects) {
            if let Some(hwnd) = entry.id.as_deref().and_then(window_of) {
                let box_ = stage.window_frame.rect(*rect);
                let (slide_x, slide_y) = stage.window_frame.outward(slide);
                let slot = (box_.x + slide_x, box_.y + slide_y, box_.width, box_.height);
                let has_badge = self
                    .windows
                    .iter()
                    .find(|window| window.hwnd == hwnd)
                    .and_then(|window| self.assets.get(&window.app_id))
                    .is_some_and(|asset| asset.icon.is_some());
                let corner = if has_badge { BADGE_CORNER } else { 0.0 };
                let opacity = match self.flights.iter().find(|flight| flight.hwnd == hwnd) {
                    _ if !visible => 0,
                    Some(flight) => flight.landing_opacity(now),
                    None => 255,
                };
                self.thumbnails.place(hwnd, slot, corner, opacity);
            }
        }
        Ok(())
    }

    /// Screen rectangle, at rest and with the window's aspect ratio, where a
    /// minimized window lives in the dock: its app's icon, or its own
    /// thumbnail slot with "show minimized windows". The genie flies into it
    /// and back out of it.
    fn minimized_slot(&self, hwnd: isize, aspect: f32) -> Option<RECT> {
        let stage = self.stage.as_ref()?;
        let id = if !self.config.show_minimized {
            self.windows
                .iter()
                .find(|window| window.hwnd == hwnd)?
                .app_id
                .clone()
        } else {
            window_id(hwnd)
        };
        // An app in a group lives in its group's icon.
        let index = self
            .entries
            .iter()
            .position(|entry| entry.id.as_deref() == Some(id.as_str()))
            .or_else(|| {
                self.entries
                    .iter()
                    .position(|entry| entry.members.contains(&id))
            })?;
        let rect = stage.frame.rect(stage.layout.slots.get(index)?.rect);
        let (width, height) = if aspect >= 1.0 {
            (rect.width, rect.width / aspect)
        } else {
            (rect.height * aspect, rect.height)
        };
        let left = rect.x + (rect.width - width) / 2.0;
        let top = rect.y + (rect.height - height) / 2.0;
        Some(RECT {
            left: left as i32,
            top: top as i32,
            right: (left + width) as i32,
            bottom: (top + height) as i32,
        })
    }

    /// A window starts minimizing: put it in the dock right away and fly it there.
    fn on_minimize_start(&mut self, hwnd: isize) -> Result<()> {
        if self.fullscreen
            || self.minimized.contains(&hwnd)
            || !self.windows.iter().any(|window| window.hwnd == hwnd)
        {
            return Ok(());
        }
        // The window has already vanished (its system transition is off):
        // the stand-in goes up before anything else, or the desktop shows.
        self.show_stand_ins(&[hwnd]);
        self.minimized.push(hwnd);
        self.sync_thumbnails();
        self.refresh()?;
        self.aim_flights();
        Ok(())
    }

    /// Live thumbnails exist only for minimized windows shown in the dock:
    /// none while they hide in their app's icon, so DWM draws nothing extra.
    fn sync_thumbnails(&mut self) {
        let shown: &[isize] = if self.config.show_minimized {
            &self.minimized
        } else {
            &[]
        };
        self.thumbnails.sync(shown);
    }

    /// Turns off the system's transitions for every tracked window. Windows
    /// starts its own minimize animation before the dock hears about the
    /// minimize, and it cannot be cancelled afterwards, so it has to be off in
    /// advance for the genie to be the only animation. The cost: those windows
    /// also lose the system's maximize and close animations while the dock runs.
    fn calm_windows(&mut self) {
        if !self.config.genie || !animations_enabled() {
            return;
        }
        let windows = &self.windows;
        self.calmed
            .retain(|hwnd| windows.iter().any(|window| window.hwnd == *hwnd));
        for window in windows {
            if !self.calmed.contains(&window.hwnd) {
                set_window_transitions(window.hwnd, false);
                self.calmed.push(window.hwnd);
            }
        }
    }

    fn restore_transitions(&mut self) {
        for hwnd in self.calmed.drain(..) {
            set_window_transitions(hwnd, true);
        }
    }

    /// Brings windows to the front. A minimized one flies out of the dock if
    /// it flew in; otherwise it gets its system transitions back first, so it
    /// returns with the system's restore animation (the next `sync` turns
    /// them off again).
    pub(crate) fn activating(&mut self, windows: Vec<isize>) -> Option<Outside> {
        if self.start_restore_flight(&windows) {
            // `tick` restores the window when the genie reaches full size.
            return None;
        }
        for hwnd in &windows {
            if self.minimized.contains(hwnd) {
                set_window_transitions(*hwnd, true);
                self.calmed.retain(|calmed| calmed != hwnd);
            }
        }
        Some(Outside::Activate(windows))
    }

    /// Flies out of the dock the window that activating `windows` would
    /// restore. `false` if the genie does not apply and the caller must
    /// restore it directly.
    fn start_restore_flight(&mut self, windows: &[isize]) -> bool {
        if !self.config.genie || !animations_enabled() || !self.flights.is_empty() {
            return false;
        }
        // Activation restores a window only when all of them are minimized,
        // and then only the first.
        let Some(&hwnd) = windows.first() else {
            return false;
        };
        // The tracker can lag behind a rapid restore/minimize. Never play an
        // old snapshot over a window that is already visible again.
        if !windows.iter().all(|window| unsafe {
            windows::Win32::UI::WindowsAndMessaging::IsIconic(HWND(*window as *mut _)).as_bool()
        }) {
            return false;
        }
        let Some(position) = self.snapshots.iter().position(|(owner, _)| *owner == hwnd) else {
            return false;
        };
        let Some(screen) = self.stage.as_ref().map(|stage| stage.key.screen) else {
            return false;
        };
        let flight = self.snapshots.get(position).and_then(|(_, snapshot)| {
            let slot = self.minimized_slot(hwnd, snapshot.aspect())?;
            Flight::restore(
                &self.gfx,
                hwnd,
                snapshot,
                slot,
                screen,
                self.config.genie_speed.seconds(),
                self.position(),
            )
        });
        let Some(flight) = flight else {
            return false;
        };
        if !self.config.show_minimized {
            flight.tuck_under(self.visual);
        }
        self.snapshots.remove(position);
        self.flights.push(flight);
        self.last_frame = Instant::now();
        true
    }

    fn start_flights(&mut self, windows: &[isize]) {
        self.show_stand_ins(windows);
        self.aim_flights();
    }

    /// Gives each new minimize flight its slot, now that the dock shows it.
    fn aim_flights(&mut self) {
        let slots: Vec<(usize, Option<RECT>)> = self
            .flights
            .iter()
            .enumerate()
            .filter(|(_, flight)| flight.needs_aim())
            .map(|(index, flight)| (index, self.minimized_slot(flight.hwnd, flight.aspect())))
            .collect();
        for (index, slot) in slots {
            if let (Some(slot), Some(flight)) = (slot, self.flights.get_mut(index)) {
                flight.aim(&self.gfx, slot);
                if !self.config.show_minimized {
                    flight.tuck_under(self.visual);
                }
            }
        }
    }

    fn show_stand_ins(&mut self, windows: &[isize]) {
        // Respect "Animation effects" being turned off in Windows settings.
        // One genie at a time: minimizing everything at once (Win+D) would
        // otherwise take a screen snapshot per window.
        if !self.config.genie || !animations_enabled() || !self.flights.is_empty() {
            return;
        }
        let Some(screen) = self.stage.as_ref().map(|stage| stage.key.screen) else {
            return;
        };
        for &hwnd in windows {
            let last_seen = self.last_rects.get(&hwnd).copied();
            let Some((from, frame)) = tracker::rect_before_minimize(hwnd, last_seen) else {
                continue;
            };
            if let Some(flight) = Flight::start(
                &self.gfx,
                hwnd,
                from,
                frame,
                screen,
                self.config.genie_speed.seconds(),
                self.position(),
            ) {
                self.flights.push(flight);
                self.last_frame = Instant::now();
                break;
            }
        }
    }

    fn on_mouse_move(&mut self) -> Result<()> {
        // A player or browser can enter full screen without changing focus.
        // Do not let a stale reveal-strip message raise the dock above it.
        if !self.hovering {
            self.update_fullscreen()?;
        }
        if self.fullscreen {
            return Ok(());
        }
        let Some(stage) = &self.stage else {
            return Ok(());
        };
        let mut point = POINT::default();
        // SAFETY: `point` remains alive for the duration of the call.
        unsafe { GetCursorPos(&mut point)? };

        let (along, _) = stage.to_dock(point);
        if self.update_drag(point) {
            self.cursor = along;
            return self.drag_to(point);
        }
        let Some(stage) = &self.stage else {
            return Ok(());
        };

        // Keep the active-area boundary aligned with the icons as they grow.
        let rise = stage.icon * (stage.magnifier.max_scale - 1.0) * self.amount.value;
        let (along, across) = stage.to_dock(point);
        if self.hovering && across < -rise - LEAVE_HALO * stage.scale {
            return self.on_mouse_leave();
        }

        self.cursor = along;
        if !self.hovering {
            self.hovering = true;
            self.amount.target = 1.0;
            self.slide.target = 0.0;
            self.last_frame = Instant::now();
            // SAFETY: Timer on our own window; killing a timer that is not set is harmless.
            unsafe {
                let _ = KillTimer(Some(self.input), HIDE_TIMER);
            }
            self.place_windows()?;
        }
        // Every move, not only on entering: a menu or a drag that captures
        // the mouse cancels the request, and without it the dock would
        // never hear the pointer leave and stay magnified.
        self.track_leave()?;
        self.render()?;
        self.aim_preview();
        Ok(())
    }

    fn preview_target(&self) -> Option<(&str, usize)> {
        if !self.hovering
            || self.menu_open
            || self.press.is_some()
            || self.drag.is_some()
            || self.drag_over
            || self.fullscreen
            || self.switching
            || self.group_view.is_some()
        {
            return None;
        }
        let index = self.hovered?;
        let entry = self.entries.get(index)?;
        let id = entry.id.as_deref()?;
        let has_windows = match entry.role {
            Role::App if entry.running => self
                .windows
                .iter()
                .filter(|window| window.app_id == id)
                .nth(0)
                .is_some(),
            Role::Window => {
                window_of(id).is_some_and(|hwnd| self.windows.iter().any(|w| w.hwnd == hwnd))
            }
            _ => false,
        };
        has_windows.then_some((id, index))
    }

    fn aim_preview(&mut self) {
        let target = self.preview_target().map(|(id, _)| id);
        if self
            .preview
            .as_ref()
            .is_some_and(|picker| target.is_none_or(|id| id == picker.app_id))
            || target == self.preview_candidate.as_deref() && self.preview.is_none()
        {
            return;
        }
        let candidate = target.map(str::to_owned);
        self.close_preview();
        if candidate.is_some() {
            // SAFETY: The timer belongs to our UI window.
            unsafe { SetTimer(Some(self.input), PREVIEW_TIMER, PREVIEW_DWELL_MS, None) };
            self.preview_candidate = candidate;
        }
    }

    fn close_preview(&mut self) {
        if self.preview_candidate.take().is_none() && self.preview.is_none() {
            return;
        }
        // SAFETY: The timer belongs to our UI window.
        unsafe {
            let _ = KillTimer(Some(self.input), PREVIEW_TIMER);
        }
        if self.preview.take().is_some() && self.config.auto_hide && !self.hovering {
            unsafe { SetTimer(Some(self.input), HIDE_TIMER, HIDE_DELAY_MS, None) };
        }
    }

    fn preview_tick(&mut self) -> Result<()> {
        let mut point = POINT::default();
        // SAFETY: Read-only pointer and window queries on the UI thread.
        if unsafe { GetCursorPos(&mut point) }.is_err() {
            self.close_preview();
            return Ok(());
        }
        if self.preview.is_some() {
            return Ok(());
        }
        // SAFETY: End the one-shot dwell before creating the preview.
        unsafe {
            let _ = KillTimer(Some(self.input), PREVIEW_TIMER);
        }
        let Some(candidate) = self.preview_candidate.take() else {
            return Ok(());
        };
        let Some((id, index)) = self.preview_target() else {
            return Ok(());
        };
        if id != candidate
            || unsafe { windows::Win32::UI::WindowsAndMessaging::WindowFromPoint(point) }
                != self.input
        {
            return Ok(());
        }
        let Some(stage) = &self.stage else {
            return Ok(());
        };
        let state = magnify(
            &stage.layout,
            self.cursor,
            self.amount.value,
            &stage.magnifier,
        );
        let rect = &state.rects[index];
        let anchor = stage.screen_rect(*rect);
        let windows = if let Some(hwnd) = window_of(&candidate) {
            vec![hwnd]
        } else {
            self.preview_order.windows_of(&self.windows, &candidate)
        };
        if windows.is_empty() {
            return Ok(());
        }
        self.preview = Some(MultiWindowView::show(
            &self.gfx,
            candidate,
            windows,
            &multi_window_view::Anchor {
                icon: anchor,
                screen: stage.key.screen,
                edge: stage.frame.edge,
            },
            stage.scale,
            Theme::for_system(stage.key.light, stage.key.glass),
            self.config.show_preview_close_button,
        )?);
        // The view owns its animation and dismissal timer while it is open.
        self.render()?;
        Ok(())
    }

    fn track_leave(&self) -> Result<()> {
        let mut track = TRACKMOUSEEVENT {
            cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: self.input,
            dwHoverTime: 0,
        };
        // SAFETY: `track` is a valid structure with its size initialized.
        unsafe { TrackMouseEvent(&mut track)? };
        Ok(())
    }

    /// A menu closed: it held the mouse, so the dock heard nothing of where
    /// the pointer went meanwhile. Settle the magnifier on where it is now.
    fn after_menu(&mut self) -> Result<()> {
        self.menu_open = false;
        let mut point = POINT::default();
        // SAFETY: Read-only queries.
        let over = unsafe {
            GetCursorPos(&mut point).is_ok()
                && windows::Win32::UI::WindowsAndMessaging::WindowFromPoint(point) == self.input
        };
        if over {
            self.on_mouse_move()
        } else {
            self.on_mouse_leave()
        }
    }

    fn on_mouse_leave(&mut self) -> Result<()> {
        if !self.hovering || self.drag.is_some() || self.drag_over || self.menu_open {
            return Ok(());
        }
        self.hovering = false;
        if self.preview.is_none() {
            self.close_preview();
        }
        self.amount.target = 0.0;
        self.last_frame = Instant::now();
        if self.config.auto_hide {
            // SAFETY: Timer on our own window.
            unsafe { SetTimer(Some(self.input), HIDE_TIMER, HIDE_DELAY_MS, None) };
        }
        self.place_windows()?;
        self.render()
    }

    /// Alt+Tab (or Task View) opened: an auto-hidden dock comes up to show
    /// the open apps until the switcher closes. It cannot follow the
    /// selection: Windows 11's switcher reports it to no one (no accessible
    /// focus, selection or name on its items; checked with MSAA and UIA).
    fn task_switcher_opened(&mut self) -> Result<()> {
        // Only Alt+Tab (and Ctrl+Alt+Tab). Task View shares the switcher's
        // window, and on Windows 11 its virtual desktops sit along the
        // bottom edge, where the dock would cover them.
        // SAFETY: Read-only keyboard state query.
        let alt = unsafe { GetAsyncKeyState(i32::from(VK_MENU.0)) < 0 };
        // After it closes, Windows sometimes hands the focus back to the
        // switcher's hidden window for a moment: not a new Alt+Tab.
        let echo = self
            .switch_ended
            .is_some_and(|ended| ended.elapsed() < SWITCHER_ECHO)
            && tracker::task_switcher_state() != Some(true);
        if self.switching || !alt || echo {
            return Ok(());
        }
        self.switching = true;
        self.close_preview();
        self.close_group_view();
        self.flights.clear();
        self.switch_started = Instant::now();
        // SAFETY: Timers on our own window.
        unsafe {
            SetTimer(Some(self.input), SWITCHER_TIMER, SWITCHER_POLL_MS, None);
            let _ = KillTimer(Some(self.input), HIDE_TIMER);
        }
        if self.config.auto_hide && !self.fullscreen {
            self.slide.target = 0.0;
            self.last_frame = Instant::now();
            self.place_windows()?;
            self.render()?;
        }
        if let Err(error) = self.start_following() {
            eprintln!("muelle: Alt+Tab: {error:#}");
        }
        Ok(())
    }

    fn check_task_switcher(&mut self) -> Result<()> {
        let open = match tracker::task_switcher_state() {
            Some(shown) => shown || self.switch_started.elapsed() < SWITCHER_APPEAR,
            None => false,
        };
        if open {
            return self.follow_selection();
        }
        self.switching = false;
        self.switch_ended = Some(Instant::now());
        // SAFETY: Our own timer.
        unsafe {
            let _ = KillTimer(Some(self.input), SWITCHER_TIMER);
        }
        self.stop_following()?;
        self.hide_if_unused()
    }

    /// Starts following the switcher's selection, when it can be known:
    /// opened with Alt+Tab, and listing exactly this desktop's windows.
    ///
    /// Windows does not say which item is selected, so it is worked out the
    /// way the switcher moves: it lists the windows from the most recently
    /// used, starts on the second (the last with Alt+Shift+Tab), and each Tab
    /// moves one ahead, Shift+Tab one back.
    fn start_following(&mut self) -> Result<()> {
        // SAFETY: Read-only keyboard state queries.
        let (alt, win, shift) = unsafe {
            (
                GetAsyncKeyState(i32::from(VK_MENU.0)) < 0,
                GetAsyncKeyState(i32::from(VK_LWIN.0)) < 0
                    || GetAsyncKeyState(i32::from(VK_RWIN.0)) < 0,
                GetAsyncKeyState(i32::from(VK_SHIFT.0)) < 0,
            )
        };
        if !alt || win {
            return Ok(());
        }
        // Topmost first, which is the order of last use.
        let order: Vec<isize> = self
            .tracker
            .list()
            .iter()
            .map(|window| window.hwnd)
            .collect();
        let edge_open = self
            .windows
            .iter()
            .any(|window| window.app_id.ends_with("\\msedge.exe"));
        if order.len() < 2 || status::alt_tab_lists_more_than_windows(edge_open) {
            return Ok(());
        }
        // Opened from the desktop or another unlisted window, the switcher
        // starts on the first window rather than the second.
        let from_listed = order.first() == Some(&LAST_FRONT.load(Ordering::Relaxed));
        let index = match (shift, from_listed) {
            (true, _) => order.len() - 1,
            (false, true) => 1,
            (false, false) => 0,
        };
        self.follow = Some(SwitcherFollow {
            order,
            index,
            // The Tab that opened the switcher is still down: not a new press.
            tab_down: true,
        });
        self.point_at_selection()
    }

    /// Moves the selection with each Tab press. Arrow keys can change it in
    /// ways that cannot be followed: the highlight then goes away rather
    /// than point at the wrong app.
    fn follow_selection(&mut self) -> Result<()> {
        let Some(follow) = &mut self.follow else {
            return Ok(());
        };
        let down = |key: VIRTUAL_KEY| {
            // SAFETY: Read-only keyboard state query.
            unsafe { GetAsyncKeyState(i32::from(key.0)) < 0 }
        };
        // Only these move the selection in ways that cannot be followed. A
        // click on an item switches to it and closes the switcher; a click
        // anywhere else, or moving the pointer, leaves the selection alone.
        let lost = [VK_LEFT, VK_RIGHT, VK_UP, VK_DOWN, VK_HOME, VK_END]
            .into_iter()
            .any(down);
        if lost {
            return self.stop_following();
        }
        // A window closed from the switcher (its "x", or Delete) leaves the
        // list, and the switcher keeps the selection on the same place.
        let before = follow.order.len();
        let index = follow.index;
        let alive: Vec<bool> = follow
            .order
            .iter()
            .map(|&hwnd| {
                let window = HWND(hwnd as *mut _);
                // SAFETY: Read-only queries; a closed window simply fails them.
                unsafe {
                    windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(window)).as_bool()
                        && windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(window)
                            .as_bool()
                }
            })
            .collect();
        let mut position = 0;
        follow.order.retain(|_| {
            position += 1;
            alive[position - 1]
        });
        let closed = follow.order.len() != before;
        let Some(selected) = selection_after_closing(index, &alive) else {
            return self.stop_following();
        };
        follow.index = selected;
        let tab = down(VK_TAB);
        let pressed = tab && !follow.tab_down;
        follow.tab_down = tab;
        if !pressed {
            return if closed {
                self.point_at_selection()
            } else {
                Ok(())
            };
        }
        let count = follow.order.len();
        follow.index = if down(VK_SHIFT) {
            (follow.index + count - 1) % count
        } else {
            (follow.index + 1) % count
        };
        self.point_at_selection()
    }

    /// Magnifies the dock icon of the selected window, as if the pointer
    /// rested on it, and shows its name.
    fn point_at_selection(&mut self) -> Result<()> {
        let Some(follow) = &self.follow else {
            return Ok(());
        };
        let selected = follow.order[follow.index];
        let window_entry = window_id(selected);
        let app = self
            .windows
            .iter()
            .find(|window| window.hwnd == selected)
            .map(|window| window.app_id.clone());
        let index = self
            .entries
            .iter()
            .position(|entry| entry.id.as_deref() == Some(window_entry.as_str()))
            .or_else(|| {
                let app = app?;
                self.entries
                    .iter()
                    .position(|entry| entry.role == Role::App && entry.id.as_ref() == Some(&app))
                    .or_else(|| {
                        self.entries
                            .iter()
                            .position(|entry| entry.members.contains(&app))
                    })
            });
        let (Some(index), Some(stage)) = (index, &self.stage) else {
            return self.stop_following();
        };
        let Some(slot) = stage.layout.slots.get(index) else {
            return self.stop_following();
        };
        if self.hovering {
            return Ok(());
        }
        self.cursor = slot.rect.x + slot.rect.width / 2.0;
        self.pointed = true;
        self.amount.target = 1.0;
        self.last_frame = Instant::now();
        self.render()
    }

    fn stop_following(&mut self) -> Result<()> {
        self.follow = None;
        if !self.pointed {
            return Ok(());
        }
        self.pointed = false;
        if !self.hovering {
            self.amount.target = 0.0;
            self.last_frame = Instant::now();
        }
        self.render()
    }

    /// Aims the update timer at the next look: a day after the last one,
    /// which survives restarts, and never sooner than two minutes after the
    /// dock starts. So the dock wakes for it once a day, not on a schedule.
    fn schedule_update(&self) {
        // SAFETY: Timers on our own window; setting one again replaces it.
        unsafe {
            if !self.config.check_updates || !self.config.auto_update {
                let _ = KillTimer(Some(self.input), UPDATE_TIMER);
                return;
            }
            // Also counted from the last try in this session: if the date
            // could not be saved, the registry would call it due again at
            // once, and the dock would start one update after another.
            let since_try = self
                .update_tried
                .map_or(std::time::Duration::ZERO, |tried| {
                    UPDATE_EVERY.saturating_sub(tried.elapsed())
                });
            let wait = muelle_shell::updates::due_in(UPDATE_EVERY)
                .max(UPDATE_AFTER_START.saturating_sub(self.launched.elapsed()))
                .max(since_try);
            SetTimer(Some(self.input), UPDATE_TIMER, millis(wait), None);
        }
    }

    /// Starts the background update: Settings without a window, which does
    /// the network part and exits when there is nothing to install. The
    /// dock itself never touches the network.
    fn update_now(&mut self) {
        // SAFETY: Our own timer.
        unsafe {
            let _ = KillTimer(Some(self.input), UPDATE_TIMER);
        }
        if !self.config.check_updates || !self.config.auto_update {
            return;
        }
        // Installing restarts the dock: not in the middle of a game.
        if self.fullscreen {
            // SAFETY: Our own timer.
            unsafe { SetTimer(Some(self.input), UPDATE_TIMER, millis(UPDATE_RETRY), None) };
            return;
        }
        // Recorded first: whatever happens, the next look is tomorrow.
        muelle_shell::updates::mark_checked();
        self.update_tried = Some(Instant::now());
        self.schedule_update();
        let settings = env::current_exe()
            .ok()
            .map(|exe| exe.with_file_name("muelle-settings.exe"))
            .filter(|path| path.exists());
        if let Some(settings) = settings
            && let Err(error) = std::process::Command::new(settings).arg("--update").spawn()
        {
            eprintln!("muelle: actualización: {error}");
        }
    }

    /// The crab plays `act` (see `mascot`), unless it is out already, it is
    /// turned off, or something runs full screen.
    fn perform(&mut self, act: mascot::Act, place: mascot::Place) {
        if !self.can_perform() {
            return;
        }
        let scale = self.stage.as_ref().map_or(1.0, |stage| stage.scale);
        let Some(mascot) = mascot::Mascot::play(&self.gfx, act, place, scale) else {
            return;
        };
        self.mascot = Some(mascot);
        // SAFETY: A timer on our own window, killed when the act ends.
        unsafe {
            if SetTimer(Some(self.input), MASCOT_TIMER, MASCOT_POLL_MS, None) == 0 {
                self.stop_mascot();
                return;
            }
        }
        if matches!(place, mascot::Place::Along { .. }) && self.config.auto_hide {
            // Reveal only after the mascot and its cleanup timer exist.
            unsafe {
                let _ = KillTimer(Some(self.input), HIDE_TIMER);
            }
            self.slide.target = 0.0;
            self.last_frame = Instant::now();
            if let Err(error) = self.place_windows().and_then(|()| self.render()) {
                muelle_shell::log::line(&format!("mostrar el dock para la mascota: {error:#}"));
                self.stop_mascot();
            }
        }
        if self.mascot.is_some() && act == mascot::Act::Charging {
            self.charging_shown = Some(Instant::now());
        }
    }

    fn can_perform(&self) -> bool {
        self.config.mascot && self.mascot.is_none() && !self.fullscreen && animations_enabled()
    }

    fn stop_mascot(&mut self) {
        let active = self.mascot.take().is_some();
        // SAFETY: These timers belong to this UI window. Always clear a stale
        // mascot timer, and resume the usual dock hiding delay after an act.
        unsafe {
            let _ = KillTimer(Some(self.input), MASCOT_TIMER);
            if active && self.config.auto_hide {
                SetTimer(Some(self.input), HIDE_TIMER, HIDE_DELAY_MS, None);
            }
        }
    }

    /// The dock's inner rim, for the crab to walk on. An auto-hidden
    /// dock comes up for the act and goes down after it, as the macOS Dock
    /// does when something asks for attention.
    fn dock_edge(&self) -> Option<mascot::Place> {
        let stage = self.stage.as_ref()?;
        let margin = (stage.icon / 2.0) as i32;
        let frame = stage.frame;
        // The rim is the dock's side away from the screen edge (dock
        // coordinate across = 0), and the walk runs the dock's length.
        let start = frame.point(0.0, 0.0);
        let end = frame.point(stage.layout.width, 0.0);
        let (from, to, rim) = if frame.edge.is_vertical() {
            (start.1, end.1, start.0)
        } else {
            (start.0, end.0, start.1)
        };
        Some(mascot::Place::Along {
            left: from as i32 - margin,
            right: to as i32 + margin,
            top: rim as i32,
            edge: frame.edge,
        })
    }

    fn mascot_tick(&mut self) -> Result<()> {
        if !self.config.mascot
            || self.fullscreen
            || !animations_enabled()
            || self.mascot.as_ref().is_none_or(mascot::Mascot::finished)
        {
            self.stop_mascot();
            return Ok(());
        }
        let mut point = POINT::default();
        // SAFETY: A valid output for the pointer position.
        if unsafe { GetCursorPos(&mut point) }.is_ok()
            && let Some(mascot) = &mut self.mascot
        {
            mascot.poke(&self.gfx, point);
        }
        Ok(())
    }

    /// A moment found by the menu bar (an AI plan's figure): the crab comes
    /// out under that provider.
    fn take_bar_cue(&mut self) {
        let cue = self.menubar.as_mut().and_then(MenuBar::take_cue);
        if let Some((act, point)) = cue {
            self.perform(
                act,
                mascot::Place::Under {
                    x: point.x,
                    top: point.y,
                },
            );
        }
    }

    /// The charger plugged in or out, or energy saver turned on: the crab walks
    /// along the dock. Only on the change itself, never at startup.
    fn power_moment(&mut self) {
        let now = (
            muelle_shell::system::battery().map(|battery| battery.charging),
            muelle_shell::system::energy_saver_on(),
        );
        let before = self.power_seen.replace(now);
        let charging_allowed = self
            .charging_shown
            .is_none_or(|shown| shown.elapsed() >= CHARGING_AGAIN);
        let act = power_act(before, now, charging_allowed);
        if let Some(act) = act {
            if matches!(act, mascot::Act::Charging | mascot::Act::Unplugged)
                && self.config.mascot
                && !self.fullscreen
                && animations_enabled()
            {
                // A disconnect must still show if it happens during the
                // charging reaction; keep only one scene and cleanup timer.
                self.stop_mascot();
            }
            if self.can_perform()
                && let Some(place) = self.dock_edge()
            {
                self.perform(act, place);
            }
        }
    }

    fn hide_if_unused(&mut self) -> Result<()> {
        if !self.config.auto_hide
            || self.mascot.is_some()
            || self.menu_open
            || self.drag_over
            || self.hovering
            || self.drag.is_some()
            || self.switching
            || self.preview.is_some()
            || self.group_view.is_some()
        {
            return Ok(());
        }
        self.slide.target = 1.0;
        self.last_frame = Instant::now();
        self.place_windows()
    }

    /// Only running or pinned apps, and groups, can be dragged. Explorer's
    /// built-in pin is managed by Settings; the other fixed items stay put too.
    fn is_draggable(&self, entry: &Entry) -> bool {
        matches!(entry.role, Role::App | Role::Group)
            && entry.id.as_ref().is_some_and(|id| {
                *id != explorer_id() && (self.roster.is_pinned(id) || self.roster.is_running(id))
            })
    }

    fn on_press(&mut self) -> Result<()> {
        self.close_preview();
        // Any press on the dock closes an unfolded group; on the group
        // itself, `on_click` then leaves it closed.
        self.group_was_open = self.group_view.as_ref().map(|view| view.group_id.clone());
        self.close_group_view();
        let Some(at) = self.pointer_at_click()? else {
            return Ok(());
        };
        let Some(entry) = self.hovered.and_then(|index| self.entries.get(index)) else {
            return Ok(());
        };
        let draggable = self.is_draggable(entry);
        self.press = entry.id.clone().map(|id| Press { id, at, draggable });
        if draggable {
            // Capture is best effort: this non-activating window also polls
            // until release, including when the pointer leaves the dock.
            // SAFETY: Both calls target our window on its owning thread.
            unsafe {
                SetCapture(self.input);
                SetTimer(Some(self.input), DRAG_TIMER, DRAG_POLL_MS, None);
            }
        }
        Ok(())
    }

    /// Button messages can arrive before a coalesced WM_MOUSEMOVE. Resolve
    /// the actual pointer again so a quick click never uses an older icon.
    fn pointer_at_click(&mut self) -> Result<Option<POINT>> {
        let mut point = POINT::default();
        // SAFETY: Read-only queries with a live point buffer.
        unsafe {
            GetCursorPos(&mut point)?;
            if windows::Win32::UI::WindowsAndMessaging::WindowFromPoint(point) != self.input {
                return Ok(None);
            }
        }
        self.hover_at(point)?;
        self.track_leave()?;
        Ok(Some(point))
    }

    fn activate_for_pointer(&self) -> bool {
        let front = tracker::foreground_window();
        // Ordinary app clicks stay non-activating so clicking the active app
        // still minimizes it. Shell flyouts are not tracked application
        // windows: accepting this real click deactivates them before launch.
        self.stage.is_some()
            && front != 0
            && front != self.input.0 as isize
            && !tracker::is_desktop(HWND(front as *mut _))
            && !self.windows.iter().any(|window| window.hwnd == front)
    }

    /// Starts a drag once the cursor moves past the system drag threshold.
    /// Returns `true` while a drag is in progress.
    fn update_drag(&mut self, point: POINT) -> bool {
        if self.drag.is_none()
            && let Some(press) = self.press.as_ref().filter(|press| press.draggable)
            && primary_button_down()
        {
            // SAFETY: Read-only query.
            let threshold = unsafe { GetSystemMetrics(SM_CXDRAG) }.max(1);
            if (point.x - press.at.x).abs() > threshold || (point.y - press.at.y).abs() > threshold
            {
                self.drag = Some(Dragging {
                    id: press.id.clone(),
                    point,
                    removing: false,
                    merge: None,
                    from_group: false,
                });
            }
        }
        self.drag.is_some()
    }

    /// Moves the dragged icon and reorders apps live.
    /// Held over the middle of another pinned app or a group, the dragged
    /// app would join it in a group instead (see `MERGE_SIDE`).
    fn drag_to(&mut self, point: POINT) -> Result<()> {
        let (Some(stage), Some(drag)) = (&self.stage, &mut self.drag) else {
            return Ok(());
        };
        drag.point = point;
        if let Some(ghost) = &self.ghost {
            ghost.move_to(point);
        }
        let id = drag.id.clone();
        let pinned = self.roster.is_pinned(&id);
        let group = self.roster.is_group(&id);
        // How far the pointer is from the dock, away from the screen edge.
        let (_, across) = stage.to_dock(point);
        let above = across < -stage.icon * (stage.magnifier.max_scale - 1.0 + REMOVE_DISTANCE);
        // A group never leaves the dock this way (it can be ungrouped from
        // its menu), and an app taken out of a group stays in the dock,
        // where it was last placed, wherever it is released.
        drag.removing = pinned && above && !group && !drag.from_group;
        drag.merge = None;
        if drag.removing || (above && drag.from_group) {
            return self.render();
        }

        let state = magnify(
            &stage.layout,
            self.cursor,
            self.amount.value,
            &stage.magnifier,
        );
        let explorer = explorer_id();
        let rise = stage.icon * (stage.magnifier.max_scale - 1.0);
        let in_dock = across >= -rise;
        if !group && in_dock {
            drag.merge = self
                .entries
                .iter()
                .zip(&state.rects)
                .find(|(entry, rect)| {
                    matches!(entry.role, Role::App | Role::Group)
                        && entry.id.as_ref().is_some_and(|other| {
                            *other != id && *other != explorer && self.roster.is_pinned(other)
                        })
                        && group_drop_contains(rect, self.cursor, across)
                })
                .and_then(|(entry, _)| entry.id.clone());
            if drag.merge.is_some() {
                return self.render();
            }
        }

        let mut before = None;
        let mut pinned_end = f32::MIN;
        let mut running_before = None;
        let mut running_end = f32::MIN;
        for (entry, rect) in self.entries.iter().zip(&state.rects) {
            let Some(other) = entry.id.as_deref() else {
                continue;
            };
            if !matches!(entry.role, Role::App | Role::Group) {
                continue;
            }
            let ahead = rect.x + rect.width / 2.0 > self.cursor;
            if self.roster.is_pinned(other) {
                if other != id {
                    pinned_end = rect.x + rect.width;
                    if other != explorer && ahead && before.is_none() {
                        before = Some(other);
                    }
                }
            } else if entry.running {
                running_end = rect.x + rect.width;
                if other != id && ahead && running_before.is_none() {
                    running_before = Some(other);
                }
            }
        }
        if !pinned {
            if !in_dock {
                return self.render();
            }
            // Moving among open apps keeps the icon unpinned. Explorer still
            // marks the end of the pinned section when it is the only pin.
            if before.is_none() && self.cursor > pinned_end + stage.gap {
                if self.cursor <= running_end + stage.gap
                    && self.roster.move_running_before(&id, running_before)
                {
                    return self.refresh();
                }
                return self.render();
            }
        }

        let list = self.roster.pinned();
        let current = list
            .iter()
            .position(|pinned| *pinned == id)
            .map(|index| list.get(index + 1).map(String::as_str));
        if current == Some(before) {
            return self.render();
        }
        self.roster.pin_before(&id, before);
        self.refresh()
    }

    /// Hover driven by a drag from another app: OLE takes the mouse, so the
    /// dock gets no WM_MOUSEMOVE meanwhile.
    fn hover_at(&mut self, point: POINT) -> Result<()> {
        let Some(stage) = &self.stage else {
            return Ok(());
        };
        self.cursor = stage.to_dock(point).0;
        if !self.hovering {
            self.hovering = true;
            self.amount.target = 1.0;
            self.slide.target = 0.0;
            self.last_frame = Instant::now();
            self.place_windows()?;
        }
        self.render()
    }

    fn drop_action(&self) -> DropAction {
        let Some(stage) = &self.stage else {
            return DropAction::Nothing;
        };
        let state = magnify(
            &stage.layout,
            self.cursor,
            self.amount.value,
            &stage.magnifier,
        );
        let target = state
            .icon_at(&stage.layout, self.cursor, stage.gap)
            .and_then(|index| self.entries.get(index));
        let Some(entry) = target else {
            return self.pin_action(&state);
        };
        let Some(id) = entry.id.clone() else {
            return DropAction::Nothing;
        };
        match entry.role {
            Role::App if id.ends_with(".exe") => DropAction::OpenWith(id),
            Role::Trash => DropAction::Recycle,
            Role::Folder => DropAction::Transfer {
                copy: !self
                    .incoming
                    .iter()
                    .all(|file| files::same_volume(file, &id)),
                folder: id,
            },
            // The user arranges the shortcuts folder in Explorer.
            Role::App | Role::Group | Role::Start | Role::Window | Role::Shortcuts => {
                DropAction::Nothing
            }
        }
    }

    /// A single app or shortcut dropped between icons is pinned there.
    fn pin_action(&self, state: &muelle_core::magnify::Magnified) -> DropAction {
        let Some(id) = self.incoming_pin.clone() else {
            return DropAction::Nothing;
        };
        let explorer = explorer_id();
        let before = self
            .entries
            .iter()
            .zip(&state.rects)
            .find(|(entry, rect)| {
                matches!(entry.role, Role::App | Role::Group)
                    && entry.id.as_ref().is_some_and(|other| {
                        *other != explorer && *other != id && self.roster.is_pinned(other)
                    })
                    && rect.x + rect.width / 2.0 > self.cursor
            })
            .and_then(|(entry, _)| entry.id.clone());
        DropAction::Pin { id, before }
    }

    fn on_drag_enter(&mut self, incoming: Vec<String>, point: POINT) -> Result<DROPEFFECT> {
        self.close_preview();
        // What a single dropped app or shortcut would pin, worked out once:
        // drag-over runs on every mouse move, and reading a shortcut is a
        // COM call and a disk read.
        self.incoming_pin = match incoming.as_slice() {
            [file] if tracker::is_shortcut(file) => pins::shortcut_id(std::path::Path::new(file)),
            [file] => Some(normalize_id(file)).filter(|id| id.ends_with(".exe")),
            _ => None,
        };
        self.incoming = incoming;
        self.drag_over = true;
        self.on_drag_over(point)
    }

    fn on_drag_over(&mut self, point: POINT) -> Result<DROPEFFECT> {
        self.hover_at(point)?;
        self.aim_spring();
        Ok(self.drop_action().effect())
    }

    fn on_drag_leave(&mut self) -> Result<()> {
        self.incoming.clear();
        self.end_drag_over();
        self.on_mouse_leave()
    }

    fn end_drag_over(&mut self) {
        self.drag_over = false;
        if self.spring.take().is_some() {
            // SAFETY: Our own timer on our own window.
            unsafe {
                let _ = KillTimer(Some(self.input), SPRING_TIMER);
            }
        }
    }

    /// The entry under a drag that can spring open: an app with windows, or
    /// a minimized window's thumbnail.
    fn spring_target(&self) -> Option<String> {
        let entry = self.hovered.and_then(|index| self.entries.get(index))?;
        let id = entry.id.clone()?;
        match entry.role {
            Role::App if !self.windows_of(&id).is_empty() => Some(id),
            Role::Window => Some(id),
            _ => None,
        }
    }

    /// Starts the spring delay when the drag moves onto another entry; the
    /// delay restarts on each new entry, so sweeping across the dock opens
    /// nothing.
    fn aim_spring(&mut self) {
        let target = self.spring_target();
        if target == self.spring {
            return;
        }
        // SAFETY: Our own timer on our own window; setting it again restarts it.
        unsafe {
            if target.is_some() {
                SetTimer(Some(self.input), SPRING_TIMER, SPRING_DELAY_MS, None);
            } else {
                let _ = KillTimer(Some(self.input), SPRING_TIMER);
            }
        }
        self.spring = target;
    }

    /// The drag rested on an app: bring its windows to the front, under the
    /// pointer, so the drop lands in the app.
    fn spring_open(&mut self) -> Result<Option<Outside>> {
        // SAFETY: Our own timer on our own window.
        unsafe {
            let _ = KillTimer(Some(self.input), SPRING_TIMER);
        }
        if !self.drag_over || self.spring_target() != self.spring {
            return Ok(None);
        }
        let Some(id) = self.spring.clone() else {
            return Ok(None);
        };
        let windows = match self.hovered.and_then(|index| self.entries.get(index)) {
            Some(entry) if entry.role == Role::Window => window_of(&id).into_iter().collect(),
            _ => self.windows_of(&id),
        };
        if windows.is_empty() || group_is_foreground(&windows, tracker::foreground_window()) {
            return Ok(None);
        }
        Ok(self.activating(windows))
    }

    fn on_drop(&mut self, point: POINT) -> Result<DROPEFFECT> {
        self.end_drag_over();
        self.hover_at(point)?;
        let action = self.drop_action();
        let effect = action.effect();
        let files = std::mem::take(&mut self.incoming);
        self.pending_drop = match action {
            DropAction::Nothing => None,
            DropAction::OpenWith(app) => {
                self.start_bounce(app.clone(), None);
                Some(Outside::OpenWith(app, files))
            }
            DropAction::Recycle => Some(Outside::Recycle(files)),
            DropAction::Transfer { folder, copy } => Some(Outside::Transfer {
                folder,
                files,
                copy,
            }),
            DropAction::Pin { id, before } => {
                // A dropped shortcut that is needed to open the app (it has
                // arguments, or opens a script) is kept as Muelle's own copy,
                // so the pin keeps working if the original is deleted.
                let id = match files.first() {
                    Some(file) if tracker::is_shortcut(file) => {
                        pins::keep_shortcut(std::path::Path::new(file))
                    }
                    _ => Some(id),
                };
                if let Some(id) = id {
                    self.roster.pin_before(&id, before.as_deref());
                    save_settings(&self.roster, &self.config);
                    self.refresh()?;
                }
                None
            }
        };
        if self.pending_drop.is_some() {
            // SAFETY: Posting a message to our own window.
            unsafe {
                let _ = PostMessageW(Some(self.input), DROP_RUN, WPARAM(0), LPARAM(0));
            }
        }
        // The mouse is still over the dock; the next WM_MOUSEMOVE starts a
        // tracked hover again.
        self.on_mouse_leave()?;
        Ok(effect)
    }

    fn on_release(&mut self) -> Result<Option<Outside>> {
        let press = self.press.take();
        // Sample the release position too: the final WM_MOUSEMOVE can be
        // coalesced or delivered to another window outside this dock.
        let positioned = if self.drag.is_some() {
            self.on_mouse_move()
        } else {
            Ok(())
        };
        let drag = self.drag.take();
        self.ghost = None;
        self.stop_drag_input();
        positioned?;
        let Some(drag) = drag else {
            // A late button-up after polling/cancellation must never launch
            // an app, nor should releasing over a different icon.
            if let Some(press) = press {
                return self.on_click(&press.id);
            }
            return Ok(None);
        };
        if let Some(target) = &drag.merge {
            self.roster.group_with(&drag.id, target);
        } else if drag.removing {
            self.roster.unpin(&drag.id);
        }
        save_settings(&self.roster, &self.config);
        self.refresh()?;
        self.leave_if_outside(drag.point)?;
        self.after_group_drag(&drag)?;
        // The order may already match the live drag. Repaint anyway so the
        // icon settles into its slot even if the pointer stops moving.
        self.render()?;
        Ok(None)
    }

    /// Capture can be lost mid-drag (Alt+Tab, a system dialog): keep the order
    /// reached so far.
    fn cancel_drag(&mut self) -> Result<()> {
        self.press = None;
        self.group_was_open = None;
        self.ghost = None;
        self.stop_drag_input();
        let Some(drag) = self.drag.take() else {
            return Ok(());
        };
        save_settings(&self.roster, &self.config);
        self.leave_if_outside(drag.point)?;
        self.after_group_drag(&drag)?;
        self.render()
    }

    fn stop_drag_input(&self) {
        // Never release capture now held by a menu or a different popup.
        // SAFETY: Our timer and the current thread's capture window.
        unsafe {
            let _ = KillTimer(Some(self.input), DRAG_TIMER);
            if GetCapture() == self.input {
                let _ = ReleaseCapture();
            }
        }
    }

    fn poll_drag(&mut self) -> Result<Option<Outside>> {
        if self.press.is_none() && self.drag.is_none() {
            self.stop_drag_input();
            return Ok(None);
        }
        // SAFETY: Read-only physical button state, independent of capture.
        let (down, escape) = unsafe {
            (
                primary_button_down(),
                GetAsyncKeyState(i32::from(VK_ESCAPE.0)) < 0,
            )
        };
        if escape {
            self.cancel_drag()?;
        } else if !down {
            // Button-up may still be queued. Resolve the release once here;
            // on_release consumes the press, so a late message is harmless.
            return self.on_release();
        } else {
            let mut point = POINT::default();
            // SAFETY: Valid output for this query.
            unsafe { GetCursorPos(&mut point)? };
            let previous = self
                .drag
                .as_ref()
                .map(|drag| drag.point)
                .or_else(|| self.press.as_ref().map(|press| press.at));
            if previous != Some(point) {
                self.on_mouse_move()?;
            }
        }
        Ok(None)
    }

    /// A drag out of a group starts away from the dock, so no hover ends
    /// with it: the input area shrinks back now, and an auto-hidden dock
    /// goes down.
    fn after_group_drag(&mut self, drag: &Dragging) -> Result<()> {
        if !drag.from_group || self.hovering {
            return Ok(());
        }
        if self.config.auto_hide {
            // SAFETY: Timer on our own window.
            unsafe { SetTimer(Some(self.input), HIDE_TIMER, HIDE_DELAY_MS, None) };
        }
        self.place_windows()
    }

    fn leave_if_outside(&mut self, point: POINT) -> Result<()> {
        let Some(stage) = &self.stage else {
            return Ok(());
        };
        let zone = stage.hover_zone;
        let inside = point.x >= zone.left
            && point.x < zone.right
            && point.y >= zone.top
            && point.y < zone.bottom;
        if inside {
            Ok(())
        } else {
            self.on_mouse_leave()
        }
    }

    fn windows_of(&self, id: &str) -> Vec<isize> {
        self.windows
            .iter()
            .filter(|window| window.app_id == id)
            .map(|window| window.hwnd)
            .collect()
    }

    /// On icon click, launches a closed app or brings its windows to the
    /// foreground. Clicking the app that is already in front minimizes it, as
    /// the Windows taskbar does; the next click brings it back.
    fn on_click(&mut self, pressed_id: &str) -> Result<Option<Outside>> {
        let group_was_open = self.group_was_open.take();
        if self.pointer_at_click()?.is_none() {
            return Ok(None);
        }
        let Some(entry) = self.hovered.and_then(|index| self.entries.get(index)) else {
            return Ok(None);
        };
        let (Some(id), role) = (entry.id.clone(), entry.role) else {
            return Ok(None);
        };
        if id != pressed_id {
            return Ok(None);
        }

        Ok(match role {
            Role::App => self.click_app(id),
            // A click unfolds the group; another click on it folds it back.
            Role::Group => {
                if group_was_open.as_deref() != Some(id.as_str()) {
                    self.open_group_view(&id)?;
                }
                None
            }
            Role::Start => Some(Outside::OpenStart),
            Role::Window => window_of(&id).and_then(|hwnd| self.activating(vec![hwnd])),
            Role::Shortcuts => self.folder_menu(id)?,
            Role::Folder | Role::Trash => Some(Outside::Open(id)),
        })
    }

    /// The shortcuts folder's menu, above its icon. The label hides while
    /// the menu is open, as for the context menu.
    fn folder_menu(&mut self, folder: String) -> Result<Option<Outside>> {
        let Some(anchor) = self.hovered.and_then(|index| self.anchor_beside(index)) else {
            return Ok(None);
        };
        let placement = self
            .stage
            .as_ref()
            .map_or(menu::Placement::Above, Stage::menu_placement);
        self.close_preview();
        self.close_group_view();
        self.hovered = None;
        self.hovering = false;
        self.render()?;
        Ok(Some(Outside::FolderMenu {
            folder,
            anchor,
            placement,
            dark: !uses_light_theme(),
        }))
    }

    /// Where a menu for the icon at `index` opens: just off it, on the side
    /// away from the screen edge (see `Stage::menu_placement`).
    fn anchor_beside(&self, index: usize) -> Option<POINT> {
        let stage = self.stage.as_ref()?;
        let state = magnify(
            &stage.layout,
            self.cursor,
            self.amount.value,
            &stage.magnifier,
        );
        let rect = state.rects.get(index)?;
        let gap = (stage.gap / 2.0).round() as i32;
        Some(stage.beside(*rect, gap))
    }

    /// What clicking the app `id` does, in the dock or in an unfolded group:
    /// open it, bring its windows to the front, or minimize them if it is the
    /// app in front.
    fn click_app(&mut self, id: String) -> Option<Outside> {
        let windows = self.windows_of(&id);
        // As on the Windows taskbar: Shift+click opens another window of the
        // app, Ctrl+Shift+click opens it as administrator.
        // SAFETY: Read-only keyboard state queries.
        let (shift, ctrl) = unsafe {
            (
                GetKeyState(i32::from(VK_SHIFT.0)) < 0,
                GetKeyState(i32::from(VK_CONTROL.0)) < 0,
            )
        };
        if shift {
            self.start_bounce(id.clone(), None);
            return Some(if ctrl {
                Outside::OpenAsAdmin(id)
            } else {
                Outside::Open(id)
            });
        }
        if windows.is_empty() && !app_exists(&id) {
            // Uninstalled while the dock was running: it leaves the dock
            // instead of failing to open.
            if self.prune_missing_apps() {
                save_settings(&self.roster, &self.config);
                if let Err(error) = self.refresh() {
                    muelle_shell::log::line(&format!("app desinstalada: {error:#}"));
                }
            }
            None
        } else if windows.is_empty() {
            self.start_bounce(id.clone(), None);
            Some(Outside::Open(id))
        } else if group_is_foreground(&windows, tracker::foreground_window()) {
            Some(Outside::Minimize(windows))
        } else {
            self.activating(windows)
        }
    }

    /// Unfolds the group `id` beside its icon, away from the screen edge.
    fn open_group_view(&mut self, id: &str) -> Result<()> {
        self.close_preview();
        self.close_group_view();
        let Some(stage) = &self.stage else {
            return Ok(());
        };
        let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.id.as_deref() == Some(id))
        else {
            return Ok(());
        };
        let state = magnify(
            &stage.layout,
            self.cursor,
            self.amount.value,
            &stage.magnifier,
        );
        let Some(rect) = state.rects.get(index) else {
            return Ok(());
        };
        let anchor = stage.screen_rect(*rect);
        let (icon, scale) = (stage.icon.round(), stage.scale);
        let theme = Theme::for_system(stage.key.light, stage.key.glass);
        // The dock's own display, which a hidden dock can move between.
        let (screen, edge) = (stage.key.screen, stage.frame.edge);
        let items = self.group_items(id, icon as u32);
        self.group_view = Some(GroupView::show(
            &self.gfx,
            id.to_owned(),
            &items,
            &Placement {
                anchor,
                screen,
                edge,
                scale,
                icon,
                theme,
                dock: self.input,
            },
        )?);
        self.sync_badge_visuals()?;
        self.render()
    }

    /// The apps of the group `id`, drawn `side` pixels wide.
    fn group_items(&mut self, id: &str, side: u32) -> Vec<GroupItem> {
        let apps = self
            .roster
            .group(id)
            .map(|group| group.apps.clone())
            .unwrap_or_default();
        let mut items = Vec::with_capacity(apps.len());
        for app in apps {
            let visual = self.icons.get(&app, "");
            let icon = visual
                .and_then(|visual| {
                    self.gfx
                        .icon_brush(&visual.icon, side.min(visual.icon.width))
                        .inspect_err(|error| eprintln!("muelle: icono de `{app}`: {error:#}"))
                        .ok()
                })
                .or_else(|| self.placeholder.clone());
            let name = visual.map_or_else(|| fallback_name(&app), |visual| visual.name.clone());
            items.push(GroupItem {
                running: self.roster.is_running(&app),
                id: app,
                name,
                icon,
            });
        }
        items
    }

    fn close_group_view(&mut self) {
        if self.group_view.take().is_some()
            && self.config.auto_hide
            && !self.hovering
            && self.drag.is_none()
        {
            // SAFETY: Timer on our own window.
            unsafe { SetTimer(Some(self.input), HIDE_TIMER, HIDE_DELAY_MS, None) };
        }
    }

    /// The pointer moved over the unfolded group: past the drag threshold,
    /// the app pressed leaves the group.
    fn group_view_moved(&mut self) -> Result<()> {
        let Some(view) = &mut self.group_view else {
            return Ok(());
        };
        if let Some((id, point)) = view.drag_started()? {
            return self.start_group_drag(id, point);
        }
        view.update()
    }

    /// An app dragged out of an unfolded group: it leaves the group for the
    /// dock, right after the group, and the drag goes on as a dock drag. So
    /// it can be placed among the icons or dropped on another group, and
    /// released anywhere else it simply stays out of the group.
    fn start_group_drag(&mut self, id: String, point: POINT) -> Result<()> {
        // Closing the view also releases the mouse it held.
        self.close_group_view();
        if !self.roster.ungroup(&id) {
            return Ok(());
        }
        let side = self.stage.as_ref().map_or(48.0, |stage| {
            (stage.icon * stage.magnifier.max_scale).round()
        });
        let brush = self
            .icons
            .get(&id, "")
            .and_then(|visual| {
                self.gfx
                    .icon_brush(&visual.icon, (side as u32).min(visual.icon.width))
                    .ok()
            })
            .or_else(|| self.placeholder.clone());
        self.ghost = brush.and_then(|brush| {
            DragGhost::show(&self.gfx, &brush, side, point)
                .inspect_err(|error| eprintln!("muelle: icono arrastrado: {error:#}"))
                .ok()
        });
        self.press = None;
        self.drag = Some(Dragging {
            id,
            point,
            removing: false,
            merge: None,
            from_group: true,
        });
        self.refresh()?;
        if let Some(stage) = &self.stage {
            self.cursor = stage.to_dock(point).0;
        }
        // SAFETY: Capture keeps mouse input while dragging outside the dock.
        unsafe {
            SetCapture(self.input);
            SetTimer(Some(self.input), DRAG_TIMER, DRAG_POLL_MS, None);
        }
        self.drag_to(point)
    }

    fn group_view_released(&mut self) -> Result<Option<Outside>> {
        let Some(view) = &mut self.group_view else {
            return Ok(None);
        };
        Ok(match view.release()? {
            Released::Item(id) => {
                self.close_group_view();
                self.click_app(id)
            }
            Released::Dismiss => {
                self.close_group_view();
                None
            }
            Released::Nothing => None,
        })
    }

    /// Right-click on an app in an unfolded group: that app's dock menu.
    fn group_view_menu(&mut self) -> Result<Option<Outside>> {
        let Some(view) = &self.group_view else {
            return Ok(None);
        };
        let hit = view.item_at_cursor()?;
        self.close_group_view();
        let Some((id, rect)) = hit else {
            return Ok(None);
        };
        let windows = self.windows_of(&id);
        let (items, end_task, jump) = self.entry_menu(&id, Role::App, &windows);
        let gap = self
            .stage
            .as_ref()
            .map_or(0, |stage| (stage.gap / 2.0).round() as i32);
        let anchor = POINT {
            x: (rect.left + rect.right) / 2,
            y: rect.top - gap,
        };
        self.render()?;
        Ok(Some(Outside::Menu(MenuRequest {
            gfx: self.gfx.clone(),
            target: MenuTarget::Entry {
                id,
                role: Role::App,
                windows,
                end_task,
                jump,
            },
            items,
            anchor,
            placement: menu::Placement::Above,
            dark: !uses_light_theme(),
        })))
    }

    /// Middle click on an app opens another window of it, as on the Windows
    /// taskbar.
    fn on_middle_click(&mut self) -> Result<Option<Outside>> {
        self.close_preview();
        self.close_group_view();
        if self.pointer_at_click()?.is_none() {
            return Ok(None);
        }
        let Some(entry) = self.hovered.and_then(|index| self.entries.get(index)) else {
            return Ok(None);
        };
        if entry.role == Role::Window {
            if let Some(id) = &entry.id
                && let Some(hwnd) = window_of(id)
            {
                muelle_shell::tracker::request_close_window(hwnd);
            }
            return Ok(None);
        }
        let (Some(id), Role::App) = (entry.id.clone(), entry.role) else {
            return Ok(None);
        };
        self.start_bounce(id.clone(), None);
        Ok(Some(Outside::Open(id)))
    }

    /// Right-click prepares the menu for the item under the cursor, or the dock
    /// menu when over empty space or a divider.
    fn on_right_click(&mut self) -> Result<Option<Outside>> {
        if self.menu_open {
            return Ok(None);
        }
        self.close_preview();
        self.close_group_view();
        if self.pointer_at_click()?.is_none() {
            return Ok(None);
        }
        let Some(stage) = &self.stage else {
            return Ok(None);
        };
        let state = magnify(
            &stage.layout,
            self.cursor,
            self.amount.value,
            &stage.magnifier,
        );
        let target_entry = self.hovered.and_then(|index| {
            let entry = self.entries.get(index)?;
            Some((entry.id.clone()?, entry.role, *state.rects.get(index)?))
        });
        let gap = (stage.gap / 2.0).round() as i32;
        let stage_placement = stage.menu_placement();

        let (target, items, anchor) = match target_entry {
            Some((id, role, rect)) => {
                let windows = self.windows_of(&id);
                let anchor = stage.beside(rect, gap);
                let (items, end_task, jump) = self.entry_menu(&id, role, &windows);
                (
                    MenuTarget::Entry {
                        id,
                        role,
                        windows,
                        end_task,
                        jump,
                    },
                    items,
                    anchor,
                )
            }
            None => {
                let anchor = stage.beside(
                    Rect {
                        x: self.cursor,
                        y: 0.0,
                        width: 0.0,
                        height: stage.layout.height,
                    },
                    gap,
                );
                let hiding = if self.config.auto_hide {
                    text().turn_hiding_off
                } else {
                    text().turn_hiding_on
                };
                let items = vec![
                    MenuItem::action(CMD_TOGGLE_HIDING, hiding),
                    MenuItem::action(CMD_SHOW_DESKTOP, text().show_desktop),
                    MenuItem::Separator,
                    MenuItem::action(CMD_TASK_MANAGER, text().task_manager),
                    MenuItem::action(CMD_TASKBAR_SETTINGS, text().taskbar_settings),
                    MenuItem::Separator,
                    MenuItem::action(CMD_SETTINGS, text().settings),
                    MenuItem::Separator,
                    MenuItem::action(CMD_QUIT_DOCK, text().quit_dock),
                ];
                (MenuTarget::Dock, items, anchor)
            }
        };

        Ok(Some(Outside::Menu(MenuRequest {
            gfx: self.gfx.clone(),
            target,
            items,
            anchor,
            placement: stage_placement,
            dark: !uses_light_theme(),
        })))
    }

    /// The menu of a dock item, the app's prepared End Task and what its jump
    /// list entries open (see `CMD_JUMP`).
    fn entry_menu(
        &self,
        id: &str,
        role: Role,
        windows: &[isize],
    ) -> (
        Vec<MenuItem>,
        Option<PreparedEndTask>,
        Vec<jumplist::Target>,
    ) {
        let is_file = !id.starts_with("shell:") && !id.starts_with("::");
        let t = text();
        let mut items = Vec::new();
        let mut jump = Vec::new();
        let end_task = if role == Role::App && id != explorer_id() {
            PreparedEndTask::capture(id, windows, &self.windows)
        } else {
            None
        };
        match role {
            Role::App => {
                // The app's jump list first, as on the taskbar.
                let groups = if self.config.jump_lists {
                    jumplist::for_dock_item(id, windows)
                } else {
                    Vec::new()
                };
                for group in groups {
                    items.push(MenuItem::Header(match group.section {
                        jumplist::Section::Pinned => t.jump_pinned.to_owned(),
                        jumplist::Section::Frequent => t.jump_frequent.to_owned(),
                        jumplist::Section::Recent => t.jump_recent.to_owned(),
                        jumplist::Section::Tasks => t.jump_tasks.to_owned(),
                        jumplist::Section::Custom(name) => shorten(&name, MENU_TITLE_CHARS),
                    }));
                    for entry in group.entries {
                        match entry {
                            jumplist::Entry::Item {
                                label,
                                target,
                                icon,
                            } => {
                                items.push(MenuItem::Action {
                                    command: CMD_JUMP + jump.len() as u32,
                                    label: shorten(&label, MENU_TITLE_CHARS).replace('&', "&&"),
                                    checked: false,
                                    enabled: true,
                                    icon,
                                });
                                jump.push(target);
                            }
                            jumplist::Entry::Separator => items.push(MenuItem::Separator),
                        }
                    }
                }
                if !items.is_empty() {
                    items.push(MenuItem::Separator);
                }
                let jump_rows = items.len();
                for (index, &hwnd) in windows.iter().take(MENU_WINDOWS).enumerate() {
                    let title = tracker::title(hwnd);
                    if !title.is_empty() {
                        items.push(MenuItem::action(
                            CMD_WINDOW + index as u32,
                            shorten(&title, MENU_TITLE_CHARS).replace('&', "&&"),
                        ));
                    }
                }
                if items.len() > jump_rows {
                    items.push(MenuItem::Separator);
                }
                let open = if windows.is_empty() {
                    t.open
                } else {
                    t.new_window
                };
                items.push(MenuItem::action(CMD_OPEN, open));
                if id != explorer_id() {
                    items.push(MenuItem::Action {
                        command: CMD_TOGGLE_PIN,
                        label: t.keep_in_dock.into(),
                        checked: self.roster.is_pinned(id),
                        enabled: true,
                        icon: None,
                    });
                }
                // A recent app can be dismissed while recent apps stay on.
                if windows.is_empty() && self.roster.recent().iter().any(|recent| recent == id) {
                    items.push(MenuItem::action(CMD_FORGET_RECENT, t.remove_from_dock));
                }
                if is_file {
                    items.push(MenuItem::action(CMD_RUN_AS_ADMIN, t.run_as_admin));
                    items.push(MenuItem::action(CMD_REVEAL, t.show_in_explorer));
                }
                if !windows.is_empty() {
                    items.push(MenuItem::Separator);
                    items.push(MenuItem::action(CMD_QUIT_APP, t.quit_app));
                    if id != explorer_id() {
                        items.push(MenuItem::Action {
                            command: CMD_END_TASK,
                            label: t.end_task.into(),
                            checked: false,
                            enabled: end_task.is_some(),
                            icon: None,
                        });
                    }
                }
            }
            Role::Group => {
                items.push(MenuItem::action(CMD_OPEN, t.open));
                items.push(MenuItem::Separator);
                items.push(MenuItem::action(CMD_UNGROUP, t.ungroup));
            }
            Role::Start => items.push(MenuItem::action(CMD_OPEN, t.open)),
            Role::Window => {
                items.push(MenuItem::action(CMD_OPEN, t.open));
                items.push(MenuItem::Separator);
                items.push(MenuItem::action(CMD_CLOSE_WINDOW, t.close_window));
            }
            Role::Folder => {
                items.push(MenuItem::action(CMD_OPEN, t.open));
                items.push(MenuItem::action(CMD_REVEAL, t.show_in_explorer));
            }
            // The folder is chosen in Settings, so changing it opens them.
            Role::Shortcuts => {
                items.push(MenuItem::action(CMD_OPEN, t.open));
                items.push(MenuItem::action(CMD_SETTINGS, t.change_folder));
                items.push(MenuItem::Separator);
                items.push(MenuItem::action(CMD_REMOVE_FOLDER, t.remove_from_dock));
            }
            Role::Trash => {
                items.push(MenuItem::action(CMD_OPEN, t.open));
                items.push(MenuItem::Separator);
                items.push(MenuItem::Action {
                    command: CMD_EMPTY_BIN,
                    label: t.empty_bin.into(),
                    checked: false,
                    enabled: self.bin_full,
                    icon: None,
                });
            }
        }
        if fixed_item_preference(role).is_some() {
            items.push(MenuItem::Separator);
            items.push(MenuItem::action(CMD_HIDE_FIXED, t.remove_from_dock));
        }
        (items, end_task, jump)
    }

    fn on_command(&mut self, target: MenuTarget, command: u32) -> Result<Option<Outside>> {
        let MenuTarget::Entry {
            id,
            role,
            windows,
            end_task,
            jump,
        } = target
        else {
            return match command {
                CMD_QUIT_DOCK => Ok(Some(Outside::QuitDock)),
                CMD_TOGGLE_HIDING => self.toggle_hiding().map(|()| None),
                CMD_SHOW_DESKTOP => Ok(Some(Outside::ShowDesktop)),
                CMD_TASK_MANAGER => Ok(Some(Outside::Open("taskmgr.exe".into()))),
                CMD_TASKBAR_SETTINGS => Ok(Some(Outside::Open("ms-settings:taskbar".into()))),
                CMD_SETTINGS => Ok(Some(Outside::OpenSettings)),
                _ => Ok(None),
            };
        };
        Ok(match command {
            CMD_OPEN if role == Role::Group => {
                self.open_group_view(&id)?;
                None
            }
            // The group's apps go back to the dock, in its place.
            CMD_UNGROUP => {
                self.close_group_view();
                self.roster.dissolve(&id);
                save_settings(&self.roster, &self.config);
                self.refresh()?;
                None
            }
            CMD_OPEN if role == Role::Start => Some(Outside::OpenStart),
            CMD_OPEN if role == Role::Window => {
                window_of(&id).and_then(|hwnd| self.activating(vec![hwnd]))
            }
            CMD_CLOSE_WINDOW => {
                launcher::close(&window_of(&id).into_iter().collect::<Vec<_>>());
                None
            }
            CMD_SETTINGS => Some(Outside::OpenSettings),
            CMD_HIDE_FIXED => {
                let saved = hide_fixed_item(&muelle_config::default_path(), role)?;
                self.config.show_start = saved.show_start;
                self.config.show_downloads = saved.show_downloads;
                self.config.show_trash = saved.show_trash;
                self.refresh()?;
                None
            }
            CMD_FORGET_RECENT => {
                if self.roster.forget_recent(&id) {
                    save_settings(&self.roster, &self.config);
                    self.refresh()?;
                }
                None
            }
            CMD_REMOVE_FOLDER => {
                if let Err(error) = forget_shortcuts_folder(&self.config) {
                    eprintln!("muelle: {error}");
                }
                self.config.shortcuts_folder = None;
                self.refresh()?;
                None
            }
            CMD_OPEN => {
                if role == Role::App {
                    self.start_bounce(id.clone(), None);
                }
                Some(Outside::Open(id))
            }
            CMD_TOGGLE_PIN => {
                // Unpinning here also unpins on the Windows taskbar, as there is
                // only one taskbar for the user. Pinning cannot go the other way.
                let on_taskbar = self
                    .config
                    .taskbar_pins
                    .as_ref()
                    .is_some_and(|pins| pins.contains(&id));
                let unpinning = self.roster.is_pinned(&id);
                if unpinning {
                    self.roster.unpin(&id);
                    pins::forget_shortcut(&id);
                } else {
                    self.roster.pin(&id);
                }
                save_settings(&self.roster, &self.config);
                self.refresh()?;
                (unpinning && on_taskbar).then_some(Outside::UnpinFromTaskbar(id))
            }
            CMD_RUN_AS_ADMIN => Some(Outside::OpenAsAdmin(id)),
            CMD_REVEAL => Some(Outside::Reveal(id)),
            CMD_QUIT_APP => {
                launcher::close(&windows);
                None
            }
            CMD_END_TASK => end_task.map(Outside::EndTask),
            CMD_EMPTY_BIN => Some(Outside::EmptyBin),
            command if command >= CMD_JUMP => {
                let target = jump.into_iter().nth((command - CMD_JUMP) as usize);
                target.map(|target| {
                    self.start_bounce(id.clone(), None);
                    Outside::OpenJump {
                        target,
                        program: id,
                    }
                })
            }
            window => {
                let index = window.checked_sub(CMD_WINDOW).map(|index| index as usize);
                index
                    .and_then(|index| windows.get(index))
                    .and_then(|&hwnd| self.activating(vec![hwnd]))
            }
        })
    }

    /// The settings app saved a change: read it and apply what differs.
    fn reload_config(&mut self) -> Result<()> {
        self.close_preview();
        self.close_group_view();
        let Ok(Some(saved)) = muelle_config::load(&muelle_config::default_path()) else {
            return Ok(());
        };
        // Pinned and recent apps belong to the dock; the settings app only
        // changes the rest.
        let config = Config {
            pinned: self.config.pinned.clone(),
            groups: self.config.groups.clone(),
            recent: self.config.recent.clone(),
            taskbar_pins: self.config.taskbar_pins.clone(),
            ..saved
        };
        if config == self.config {
            return Ok(());
        }
        self.stop_mascot();
        if config.language != self.config.language {
            strings::set_language(config.language);
            // Labels are drawn text: redraw them in the new language.
            self.assets_for = (0.0, false, 0, DockEdge::Bottom);
        }
        if !config.genie {
            self.restore_transitions();
            self.snapshots.clear();
        }
        if config.notification_dnd || !config.notifications {
            self.notifications.clear_banners();
            notification_banner::NotificationBannerView::hide();
        }
        if config.auto_hide != self.config.auto_hide {
            self.slide.target = if config.auto_hide { 1.0 } else { 0.0 };
            self.last_frame = Instant::now();
        }
        let moved = config.dock_position != self.config.dock_position;
        apply_explorer_pin(&mut self.roster, config.pin_explorer);
        self.slide.omega = config.dock_auto_hide_speed.omega();
        self.config = config;
        if moved {
            // A hidden dock comes out of its new edge on every display.
            self.apply_edge_settings();
        }
        // Updates may have been turned on or off.
        self.schedule_update();
        self.sync_thumbnails();
        self.apply_menu_bar_setting();
        self.update_menu_bar_app()?;
        self.calm_windows();
        self.refresh()?;
        self.sync_badge_visuals()?;
        self.render()
    }

    /// Adds the apps pinned to the Windows taskbar that the dock does not
    /// have yet, in the taskbar's order, after the dock's own pins. Nothing is
    /// removed or reordered: the dock imports them by itself only the first
    /// time it runs, and pins the user removed since then stay removed until
    /// they ask for this.
    /// Brings pins and unpins made on the Windows taskbar to the dock, as if
    /// it were the taskbar: what was pinned there is pinned here, what was
    /// unpinned there is unpinned here. Apps pinned only in the dock stay.
    /// Windows offers no way to do the reverse (pin to the taskbar).
    fn sync_taskbar_pins(&mut self) -> Result<()> {
        let pruned = self.prune_missing_apps();
        let Some(current) = taskbar_pins_if_readable() else {
            if pruned {
                save_settings(&self.roster, &self.config);
                return self.refresh();
            }
            return Ok(());
        };
        let Some(previous) = self.config.taskbar_pins.replace(current.clone()) else {
            // First look since this feature: keep the dock as the user left it,
            // but add the Store apps pinned in Windows. Versions before 0.1.7
            // could not import them, so their absence was never a choice; on
            // Windows 10 most default pins (Store, Mail) are Store apps.
            for id in current
                .iter()
                .filter(|id| id.starts_with("shell:AppsFolder\\"))
            {
                self.roster.pin(id);
            }
            save_settings(&self.roster, &self.config);
            return self.refresh();
        };
        if previous == current {
            if pruned {
                save_settings(&self.roster, &self.config);
                return self.refresh();
            }
            return Ok(());
        }
        for id in current.iter().filter(|id| !previous.contains(id)) {
            self.roster.pin(id);
        }
        for id in previous.iter().filter(|id| !current.contains(id)) {
            self.roster.unpin(id);
        }
        apply_explorer_pin(&mut self.roster, self.config.pin_explorer);
        save_settings(&self.roster, &self.config);
        self.refresh()
    }

    /// Removes apps that were uninstalled, so no broken icon stays behind, as
    /// the Windows taskbar drops them: pinned apps (in groups too) and recent
    /// apps (issue #25). Returns whether anything changed.
    fn prune_missing_apps(&mut self) -> bool {
        let missing: Vec<String> = self
            .roster
            .pinned_apps()
            .into_iter()
            .filter(|id| !app_exists(id))
            .collect();
        for id in &missing {
            self.roster.unpin(id);
        }
        let gone: Vec<String> = self
            .roster
            .recent()
            .iter()
            .filter(|id| !app_exists(id))
            .cloned()
            .collect();
        for id in &gone {
            self.roster.forget_recent(id);
        }
        !missing.is_empty() || !gone.is_empty()
    }

    fn toggle_hiding(&mut self) -> Result<()> {
        self.config.auto_hide = save_dock_config(&self.roster, &self.config, true)?.auto_hide;
        self.slide.target = if self.config.auto_hide { 1.0 } else { 0.0 };
        self.last_frame = Instant::now();
        self.apply_edge_settings();
        self.refresh()?;
        self.update_fullscreen()
    }

    fn set_bin_full(&mut self, full: bool) -> Result<()> {
        if full == self.bin_full {
            return Ok(());
        }
        self.bin_full = full;
        self.assets.remove(recycle_bin::PARSING_NAME);
        self.refresh()
    }

    /// `until: None` is the bounce of an app being opened, which the user can
    /// turn off; a request for attention always bounces.
    fn start_bounce(&mut self, id: String, until: Option<f32>) {
        if until.is_none() && !self.config.bounce_on_open {
            return;
        }
        self.bounces.entry(id).or_insert(Bounce {
            start: Instant::now(),
            until,
        });
        self.last_frame = Instant::now();
    }

    fn on_attention(&mut self, hwnd: isize) -> Result<()> {
        if !self.windows.iter().any(|window| window.hwnd == hwnd) && self.read_windows() {
            self.sync_badge_visuals()?;
            self.render()?;
        }
        // A minimize/flash can arrive before the debounced tracker refresh.
        // SAFETY: Read-only query; a stale or destroyed HWND returns false.
        let minimized =
            unsafe { windows::Win32::UI::WindowsAndMessaging::IsIconic(HWND(hwnd as *mut _)) }
                .as_bool();
        let foreground = if minimized {
            0
        } else {
            active_attention_window(&self.windows, tracker::foreground_window())
        };
        if hwnd == foreground {
            return Ok(());
        }
        let app_id = self
            .windows
            .iter()
            .find(|window| window.hwnd == hwnd)
            .map(|window| window.app_id.clone());
        if let Some(app_id) = app_id {
            let changed = self.attention_enabled && self.attention.request(hwnd, foreground);
            if (changed || !self.attention_enabled)
                && self.config.notification_bounce
                && animations_enabled()
            {
                self.start_bounce(app_id.clone(), Some(ATTENTION_HOPS * bounce::HOP_SECONDS));
            }
            if changed {
                self.sync_badge_visuals()?;
                self.render()?;
            }
            if (self.config.notification_badges || self.config.notifications)
                && !self.config.muted_apps.contains(&app_id)
            {
                let title = tracker::title(hwnd);
                let app_name = muelle_shell::names::display_name(&app_id);
                let app_name = if app_name.is_empty() {
                    title.clone()
                } else {
                    app_name
                };
                let pushed = self.notifications.push(
                    app_id.clone(),
                    app_name,
                    title,
                    String::new(),
                    None,
                    Some(hwnd),
                    self.config.notification_duration as u64 * 1000,
                    self.config.notification_dnd || !self.config.notifications,
                    false,
                );
                if self.config.notification_badges {
                    let _ = self.sync_badge_visuals().and_then(|()| self.render());
                }
                if pushed.is_some() {
                    self.show_active_banners();
                }
            }
        }
        Ok(())
    }

    fn show_active_banners(&self) {
        if self.fullscreen || self.config.notification_dnd || !self.config.notifications {
            return;
        }
        let pos = match self.config.notification_position {
            muelle_config::NotificationPosition::TopRight => {
                muelle_core::notifications::BannerPosition::TopRight
            }
            muelle_config::NotificationPosition::BottomRight => {
                muelle_core::notifications::BannerPosition::BottomRight
            }
        };
        let work_area = primary_monitor().map_or(
            Rect {
                x: 0.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0,
            },
            |m| Rect {
                x: m.screen.left as f32,
                y: m.screen.top as f32,
                width: (m.screen.right - m.screen.left) as f32,
                height: (m.screen.bottom - m.screen.top) as f32,
            },
        );
        let scale = self.dock_monitor().map_or(1.0, |m| m.scale);
        let theme = Theme::for_system(uses_light_theme(), transparency_enabled());
        let _ = notification_banner::NotificationBannerView::ensure_or_create(
            &self.gfx, pos, work_area, scale, theme,
        );
        let _ =
            notification_banner::NotificationBannerView::sync_queue(&self.gfx, &self.notifications);
    }

    pub fn on_notification(&mut self, payload: muelle_shell::dock_link::NotificationPayload) {
        if !self.config.notifications {
            return;
        }
        let muted = self.config.muted_apps.contains(&payload.app_id);
        if muted {
            return;
        }

        let duration_ms = (payload.duration_secs.max(1) as u64) * 1000;
        let dnd = self.config.notification_dnd;

        let pushed = self.notifications.push(
            payload.app_id.clone(),
            payload.app_name.clone(),
            payload.title.clone(),
            payload.body.clone(),
            payload.icon_path.clone(),
            None,
            duration_ms,
            dnd,
            false,
        );

        if self.config.notification_bounce && animations_enabled() {
            self.start_bounce(
                payload.app_id.clone(),
                Some(ATTENTION_HOPS * bounce::HOP_SECONDS),
            );
        }

        if self.config.notification_badges {
            let _ = self.sync_badge_visuals().and_then(|()| self.render());
        }

        if pushed.is_some() {
            self.show_active_banners();
        }
    }

    pub fn dismiss_notification(&mut self, id: u64) {
        self.notifications.dismiss(id);
        let _ =
            notification_banner::NotificationBannerView::sync_queue(&self.gfx, &self.notifications);
    }

    pub fn tick_notifications(&mut self, delta_ms: u64) {
        let expired = self.notifications.tick(delta_ms);
        if !expired.is_empty() {
            let _ = notification_banner::NotificationBannerView::sync_queue(
                &self.gfx,
                &self.notifications,
            );
        }
    }

    pub fn clear_notification_badge(&mut self, app_id: &str) {
        if self.notifications.badge_count(app_id) > 0 {
            self.notifications.clear_badge(app_id);
            let _ = self.sync_badge_visuals().and_then(|()| self.render());
        }
    }

    fn sync_badge_visuals(&mut self) -> Result<()> {
        if let Some(stage) = &mut self.stage {
            stage.scene.set_notification_counts(
                &self.gfx,
                self.entries.iter().map(|entry| {
                    if self.config.notification_badges {
                        entry
                            .id
                            .as_deref()
                            .map_or(0, |id| self.notifications.badge_count(id))
                    } else {
                        0
                    }
                }),
            )?;
            stage.scene.set_attention(
                &self.gfx,
                self.entries.iter().map(|entry| {
                    self.attention_enabled
                        && entry_has_attention(&self.attention, &self.windows, entry)
                }),
            )?;
        }
        if let Some(view) = &mut self.group_view {
            view.set_attention(&self.gfx, |id| {
                self.attention_enabled && app_has_attention(&self.attention, &self.windows, id)
            })?;
        }
        Ok(())
    }

    fn taskbar_preferences_changed(&mut self) -> Result<()> {
        if let Some(watch) = &mut self.badge_watch {
            watch.arm();
        }
        let enabled = status::taskbar_badges_enabled();
        if self.attention_enabled != enabled {
            self.attention_enabled = enabled;
            self.attention.clear();
            self.sync_badge_visuals()?;
            self.render()?;
        }
        Ok(())
    }

    fn forget_attention(&mut self, hwnd: isize) -> Result<()> {
        if self.attention.acknowledge(hwnd) {
            self.sync_badge_visuals()?;
            self.render()?;
        }
        Ok(())
    }

    /// Refreshes open windows and recomposes the dock if anything changed.
    fn sync(&mut self) -> Result<()> {
        let attention_changed = self.read_windows();
        if self
            .preview
            .as_ref()
            .is_some_and(|view| !view.matches(&self.windows_of(&view.app_id)))
        {
            self.close_preview();
        }
        let windows = &self.windows;
        self.last_rects
            .retain(|hwnd, _| windows.iter().any(|window| window.hwnd == *hwnd));
        let tracked: Vec<isize> = self.windows.iter().map(|window| window.hwnd).collect();
        for hwnd in tracked {
            self.remember_rect(hwnd);
        }
        self.calm_windows();
        let recent_before = self.roster.recent().to_vec();
        let changed = self
            .roster
            .sync(self.windows.iter().map(|window| window.app_id.as_str()));
        if !self.config.pin_explorer {
            self.roster.forget_recent(&explorer_id());
        }
        if self.roster.recent() != recent_before.as_slice() {
            save_settings(&self.roster, &self.config);
        }

        // The launching app has opened a window, so finish its current bounce.
        let now = Instant::now();
        for (id, bounce) in &mut self.bounces {
            if bounce.until.is_none() && self.roster.is_running(id) {
                let elapsed = now.duration_since(bounce.start).as_secs_f32();
                bounce.until = Some(bounce::end_of_hop(elapsed));
            }
        }

        let minimized_before = self.minimized.clone();
        let now_minimized: Vec<isize> = self
            .windows
            .iter()
            .filter(|window| window.minimized)
            .map(|window| window.hwnd)
            .collect();
        // A window still flying in was added at minimize start and may not
        // report as minimized yet.
        let flights = &self.flights;
        self.minimized.retain(|hwnd| {
            now_minimized.contains(hwnd) || flights.iter().any(|flight| flight.hwnd == *hwnd)
        });
        for hwnd in now_minimized {
            if !self.minimized.contains(&hwnd) {
                self.minimized.push(hwnd);
            }
        }
        self.sync_thumbnails();
        let minimized = &self.minimized;
        self.snapshots.retain(|(hwnd, _)| minimized.contains(hwnd));

        if changed || self.minimized != minimized_before {
            self.refresh()?;
        }
        let newly_minimized: Vec<isize> = self
            .minimized
            .iter()
            .copied()
            .filter(|hwnd| !minimized_before.contains(hwnd))
            .collect();
        if self.started && !self.fullscreen {
            self.start_flights(&newly_minimized);
        }
        self.started = true;
        if attention_changed {
            self.sync_badge_visuals()?;
            self.render()?;
        }
        // A foreground-window change may mean an app entered or exited full-screen mode.
        self.update_fullscreen()
    }

    fn tick(&mut self) -> Result<Option<Outside>> {
        let now = Instant::now();
        let flying = self.flights.len();
        let gfx = &self.gfx;
        let snapshots = &mut self.snapshots;
        let mut restore = None;
        let foreground = tracker::foreground_window();
        // The Alt+Tab switcher can briefly have no foreground HWND. Cancel
        // before a deferred restore gets a chance to fight the keyboard.
        let switching = unsafe {
            GetAsyncKeyState(i32::from(VK_MENU.0)) < 0 && GetAsyncKeyState(i32::from(VK_TAB.0)) < 0
        };
        self.flights.retain_mut(|flight| {
            if switching || !flight.is_current(foreground) {
                return false;
            }
            let going = flight.step(gfx, now);
            if flight.take_restore(!going) {
                restore = Some(flight.hwnd);
            }
            if !going && let Some(snapshot) = flight.take_snapshot_for_restore() {
                if snapshots.len() >= MAX_SNAPSHOTS {
                    snapshots.remove(0);
                }
                snapshots.push((flight.hwnd, snapshot));
            }
            going
        });
        if self.flights.len() != flying {
            // Confirm the windows really are minimized, or restored.
            // SAFETY: Timer on our own window.
            unsafe { SetTimer(Some(self.input), SYNC_TIMER, SYNC_DELAY_MS, None) };
        }
        let dt = now.duration_since(self.last_frame).as_secs_f32();
        self.amount.step(dt);
        self.slide.step(dt);
        self.last_frame = now;

        self.bounces.retain(|_, bounce| {
            let elapsed = now.duration_since(bounce.start).as_secs_f32();
            let until = bounce.until.unwrap_or(LAUNCH_BOUNCE_LIMIT);
            elapsed < until
        });
        self.render()?;
        Ok(restore.map(|hwnd| Outside::Activate(vec![hwnd])))
    }

    /// Explorer can block on taking over the taskbar, especially after it
    /// restarts. Coalesce notifications while the worker owns that request.
    fn request_taskbar(&mut self, rebuild: bool) -> Result<()> {
        if self.taskbar_worker.is_some() {
            self.taskbar_retry |= rebuild;
            return Ok(());
        }
        let window = self.visual.0 as isize;
        self.taskbar_attempts = self.taskbar_attempts.saturating_add(1);
        self.taskbar_worker = Some(thread::Builder::new().name("muelle-taskbar".into()).spawn(
            move || {
                let hidden = taskbar::take_over();
                // SAFETY: Only a message with scalar data is sent to our UI.
                unsafe {
                    let _ = PostMessageW(
                        Some(HWND(window as *mut _)),
                        TASKBAR_TAKEN,
                        WPARAM(usize::from(rebuild)),
                        LPARAM(isize::from(hidden)),
                    );
                }
            },
        )?);
        Ok(())
    }

    fn taskbar_taken(&mut self, rebuild: bool, hidden: bool) -> Result<()> {
        // The worker has finished its Explorer call and posted this reply.
        self.taskbar_worker = None;
        if std::mem::take(&mut self.taskbar_retry) {
            return self.request_taskbar(true);
        }
        if !hidden {
            // Explorer is still starting or recreated its taskbar during our
            // request. Retry briefly without blocking the UI or polling forever.
            if self.taskbar_attempts < TASKBAR_MAX_ATTEMPTS {
                unsafe {
                    SetTimer(Some(self.input), TASKBAR_TIMER, TASKBAR_DELAY_MS, None);
                }
            } else {
                muelle_shell::log::line(
                    "barra de Windows pendiente tras 50 intentos; se reintentará al reiniciarse Explorer o reanudar Windows",
                );
            }
            return Ok(());
        }
        self.taskbar_attempts = 0;
        if rebuild {
            self.appbar = None;
            self.appbar = AppBar::register(self.visual, APPBAR_CALLBACK);
            self.menubar = None;
            self.apply_menu_bar_setting();
        }
        self.stage = None;
        self.refresh()
    }

    /// Hides the dock while the foreground window covers the entire screen and
    /// shows it again when it no longer does.
    /// Creates or removes the menu bar to match the settings.
    fn apply_menu_bar_setting(&mut self) {
        let widgets = MenuBarWidgets {
            search: self.config.menu_bar_search,
            task_view: self.config.menu_bar_task_view,
            widgets: self.config.menu_bar_widgets,
            bluetooth: self.config.menu_bar_bluetooth,
            battery_percent: self.config.menu_bar_battery_percent,
            island_style: self.config.menu_bar_island_style,
        };
        if !self.config.menu_bar {
            self.menubar = None;
        } else if self.menubar.is_none() {
            self.menubar = MenuBar::create(
                &self.gfx,
                self.config.ai_usage,
                self.config.menu_bar_music,
                self.config.menu_bar_auto_hide,
                widgets,
            )
            .inspect_err(|error| eprintln!("muelle: barra superior: {error:#}"))
            .ok();
        }
        if let Some(bar) = &mut self.menubar {
            let applied = bar
                .set_auto_hide(self.config.menu_bar_auto_hide)
                .and_then(|()| bar.set_usage_enabled(&self.gfx, self.config.ai_usage))
                .and_then(|()| bar.set_media_enabled(&self.gfx, self.config.menu_bar_music))
                .and_then(|()| bar.set_widgets(&self.gfx, widgets))
                .and_then(|()| {
                    let covered = primary_monitor()
                        .is_some_and(|monitor| tracker::foreground_is_fullscreen(monitor.screen));
                    bar.set_visible(!covered)
                })
                .and_then(|()| {
                    bar.set_accessories_enabled(&self.gfx, self.config.accessory_battery)
                });
            if let Err(error) = applied {
                eprintln!("muelle: barra superior: {error:#}");
            }
        }
        self.apply_edge_settings();
    }

    /// The screen edge chosen in Settings.
    fn position(&self) -> DockEdge {
        match self.config.dock_position {
            DockPosition::Bottom => DockEdge::Bottom,
            DockPosition::Left => DockEdge::Left,
            DockPosition::Right => DockEdge::Right,
        }
    }

    /// How much of `screen`'s top the menu bar keeps, for a dock on a side
    /// edge to start below it. Zero on the bottom edge (not needed there),
    /// when the bar is off or hides itself, and on other displays (the bar
    /// is on the primary one).
    fn menu_bar_inset(&self, screen: RECT, edge: DockEdge) -> i32 {
        if !edge.is_vertical() {
            return 0;
        }
        // From the settings, not the live bar: at startup the dock is placed
        // before the bar exists, and it must not move once the bar appears.
        if !self.config.menu_bar || self.config.menu_bar_auto_hide {
            return 0;
        }
        primary_monitor()
            .filter(|primary| primary.screen == screen)
            .map_or(0, |primary| MenuBar::strip_height(primary.scale))
    }

    fn dock_monitor(&self) -> Option<Monitor> {
        self.dock_display
            .filter(|_| self.config.auto_hide)
            .and_then(monitor_at)
            .or_else(primary_monitor)
    }

    fn apply_edge_settings(&mut self) {
        if !self.config.auto_hide {
            self.dock_display = None;
        }
        let mut screens = if self.config.auto_hide {
            monitor_rects()
        } else {
            Vec::new()
        };
        let watch_dock = screens.len() > 1;
        if !watch_dock {
            // One display already has the dock's own reveal strip.
            screens.clear();
        }
        edge_watch::set_dock_monitors(screens, self.position());
        let enabled = watch_dock || (self.menubar.is_some() && self.config.menu_bar_auto_hide);
        if enabled && self.edge_watch.is_none() {
            self.edge_watch = EdgeWatch::start(self.visual)
                .inspect_err(|error| {
                    muelle_shell::log::line(&format!("bordes de pantalla: {error:#}"))
                })
                .ok();
        } else if !enabled && self.edge_watch.take().is_some() {
            // SAFETY: This timer belongs to our UI thread.
            unsafe {
                let _ = KillTimer(Some(self.visual), EDGE_TIMER);
            }
        }
    }

    fn edge_changed(&mut self) -> Result<()> {
        // Explorer can recreate a secondary taskbar while the dock is running.
        // Check on monitor entry so it is transparent before the edge dwell.
        if self.config.auto_hide
            && self.taskbar_worker.is_none()
            && taskbar::needs_visibility_repair()
        {
            self.request_taskbar(false)?;
        }
        let (target, wait) = edge_watch::pending();
        let on_dock_edge = matches!(target, Some(EdgeTarget::Dock(screen))
            if self.stage.as_ref().is_some_and(|stage| stage.key.screen == screen));
        self.arm_edge_timer(wait);
        if !on_dock_edge && self.config.auto_hide && !self.hovering && self.slide.target == 0.0 {
            // SAFETY: Existing hide timer on the dock input window.
            unsafe {
                SetTimer(Some(self.input), HIDE_TIMER, HIDE_DELAY_MS, None);
            }
        }
        Ok(())
    }

    fn arm_edge_timer(&self, wait: Option<std::time::Duration>) {
        // SAFETY: One-shot timer on the window receiving edge notifications.
        unsafe {
            let _ = KillTimer(Some(self.visual), EDGE_TIMER);
            if let Some(wait) = wait {
                SetTimer(Some(self.visual), EDGE_TIMER, millis(wait).max(1), None);
            }
        }
    }

    fn edge_rested(&mut self) -> Result<()> {
        self.arm_edge_timer(None);
        if self.edge_watch.is_none() {
            return Ok(());
        }
        let mut point = POINT::default();
        // SAFETY: Valid output for this call.
        unsafe { GetCursorPos(&mut point)? };
        match edge_watch::poll(point) {
            Reveal::Wait(wait) => self.arm_edge_timer(Some(wait)),
            Reveal::Ready(EdgeTarget::MenuBar) => {
                if let Some(bar) = &mut self.menubar {
                    bar.edge_rested(&self.gfx, point)?;
                }
            }
            Reveal::Ready(EdgeTarget::Dock(screen)) => self.reveal_dock(point, screen)?,
            Reveal::Idle => self.edge_changed()?,
        }
        Ok(())
    }

    fn reveal_dock(&mut self, point: POINT, screen: RECT) -> Result<()> {
        // SAFETY: Read-only mouse button state.
        let mouse_down = [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON]
            .into_iter()
            .any(|key| unsafe { GetAsyncKeyState(i32::from(key.0)) < 0 });
        if !self.config.auto_hide
            || self.hovering
            || self.menu_open
            || self.press.is_some()
            || self.drag_over
            || self.drag.is_some()
            || self.switching
            || !self.flights.is_empty()
            || mouse_down
            || monitor_at(point).is_none_or(|monitor| monitor.screen != screen)
            || tracker::foreground_is_fullscreen(screen)
        {
            return Ok(());
        }
        let switched = self
            .stage
            .as_ref()
            .is_none_or(|stage| stage.key.screen != screen);
        if switched {
            self.close_preview();
            self.stop_mascot();
            self.dock_display = Some(POINT {
                x: screen.left + (screen.right - screen.left) / 2,
                y: screen.top + (screen.bottom - screen.top) / 2,
            });
            self.amount = Spring::at(0.0, HOVER_OMEGA);
            self.slide = Spring::at(1.0, self.config.dock_auto_hide_speed.omega());
            self.fullscreen = false;
            self.refresh()?;
        }
        if self
            .stage
            .as_ref()
            .is_some_and(|stage| stage.spans_reveal(point))
        {
            return self.on_mouse_move();
        }
        self.slide.target = 0.0;
        self.last_frame = Instant::now();
        // SAFETY: Leaving the edge or entering the dock re-arms the hide
        // delay. Cancel any delay left over from the previous monitor.
        unsafe {
            let _ = KillTimer(Some(self.input), HIDE_TIMER);
        }
        self.place_windows()?;
        self.render()
    }

    /// Shows the name of the app in front, as the macOS menu bar does.
    fn update_menu_bar_app(&mut self) -> Result<()> {
        if self.menubar.is_none() {
            return Ok(());
        }
        let front = tracker::foreground_window();
        let id = self
            .windows
            .iter()
            .find(|window| window.hwnd == front)
            .map_or_else(explorer_id, |window| window.app_id.clone());
        if !self.names.contains_key(&id) {
            let name = self
                .icons
                .get(&id, "")
                .map_or_else(|| fallback_name(&id), |visual| visual.name.clone());
            self.names.insert(id.clone(), name);
        }
        let name = self.names.get(&id).map_or("", String::as_str);
        match &mut self.menubar {
            Some(bar) => bar.set_app_name(&self.gfx, name),
            None => Ok(()),
        }
    }

    /// Notes where `hwnd` is on screen. A maximized window forgets it: its
    /// rectangle then comes from the work area, and a stale one from before
    /// could be taken after it is un-maximized.
    fn remember_rect(&mut self, hwnd: isize) {
        match tracker::screen_rect(hwnd) {
            Some((rect, false)) => {
                self.last_rects.insert(hwnd, rect);
            }
            Some((_, true)) => {
                self.last_rects.remove(&hwnd);
            }
            None => {}
        }
    }

    fn update_fullscreen(&mut self) -> Result<()> {
        // The window in front is where the user works: note where it is now.
        let front = tracker::foreground_window();
        if self.windows.iter().any(|window| window.hwnd == front) {
            self.remember_rect(front);
        }
        let Some(monitor) = primary_monitor() else {
            return Ok(());
        };
        let bar_fullscreen = tracker::foreground_is_fullscreen(monitor.screen);
        let fullscreen = self.dock_monitor().map_or(bar_fullscreen, |dock| {
            if dock.screen == monitor.screen {
                bar_fullscreen
            } else {
                tracker::foreground_is_fullscreen(dock.screen)
            }
        });
        if let Some(bar) = &mut self.menubar {
            // An auxiliary bar failure must not strand a hidden dock.
            if let Err(error) = bar.set_visible(!bar_fullscreen) {
                crate::recovery::graphics_error(&error);
                muelle_shell::log::line(&format!("visibilidad de la barra superior: {error:#}"));
            }
        }
        // Windows sometimes omits the shell event when a full-screen app
        // closes or switches desktops. Poll only while our UI is hidden.
        // SAFETY: `self.input` is a live window on this UI thread.
        unsafe {
            if fullscreen || (bar_fullscreen && self.menubar.is_some()) {
                SetTimer(
                    Some(self.input),
                    FULLSCREEN_TIMER,
                    FULLSCREEN_RECHECK_MS,
                    None,
                );
            }
        }
        if fullscreen != self.fullscreen {
            muelle_shell::log::line(&format!(
                "{} por pantalla completa (al frente: {})",
                if fullscreen { "se oculta" } else { "vuelve" },
                tracker::foreground_class()
            ));
            let previous = self.fullscreen;
            self.fullscreen = fullscreen;
            if fullscreen {
                self.stop_mascot();
                self.close_preview();
                self.close_group_view();
                self.hovering = false;
                self.amount = Spring::at(0.0, HOVER_OMEGA);
                self.press = None;
                self.drag = None;
                self.ghost = None;
                self.stop_drag_input();
            }
            if let Err(error) = self.place_windows() {
                self.fullscreen = previous;
                return Err(error);
            }
        }
        if !fullscreen && (!bar_fullscreen || self.menubar.is_none()) {
            // SAFETY: This UI thread owns the timer.
            unsafe {
                let _ = KillTimer(Some(self.input), FULLSCREEN_TIMER);
            }
        }
        // Updating an app name can fail transiently; it must never prevent
        // the dock or bar from returning after full-screen mode.
        if let Err(error) = self.update_menu_bar_app() {
            eprintln!("muelle: nombre de la barra superior: {error:#}");
        }
        Ok(())
    }

    fn on_foreground_changed(&mut self) -> Result<()> {
        self.close_preview();
        let foreground = tracker::foreground_window();
        if let Some(window) = self.windows.iter().find(|w| w.hwnd == foreground) {
            let app_id = window.app_id.clone();
            self.clear_notification_badge(&app_id);
        }
        self.flights.retain(|flight| flight.is_current(foreground));
        // The app roster can stay identical while its windows change z-order.
        // Refresh their order so the next dock activation selects the actual
        // most recently used window rather than a stale group leader.
        let attention_changed = self.read_windows();
        #[cfg(feature = "recover-browser-z-order")]
        tracker::recover_browser_z_order(&self.windows, foreground);
        self.update_fullscreen()?;
        if attention_changed {
            self.sync_badge_visuals()?;
            self.render()?;
        }
        Ok(())
    }

    fn read_windows(&mut self) -> bool {
        self.windows = self.tracker.list();
        self.preview_order.update(&self.windows);
        let pruned = self
            .attention
            .retain(|hwnd| self.windows.iter().any(|window| window.hwnd == hwnd));
        self.attention.acknowledge(active_attention_window(
            &self.windows,
            tracker::foreground_window(),
        )) || pruned
    }

    fn on_appbar_notice(&mut self, notice: usize) -> Result<()> {
        match notice {
            // Windows notifications may be missing or unreliable, so use them
            // only as a signal to recheck the state.
            ABN_FULLSCREENAPP => self.update_fullscreen(),
            // The dock's own reservation can trigger this. The strip changes
            // only with the screen or dock size, already handled by `refresh`.
            _ => Ok(()),
        }
    }

    fn is_animating(&self) -> bool {
        !self.amount.is_settled()
            || !self.slide.is_settled()
            || self
                .stage
                .as_ref()
                .is_some_and(|stage| stage.reorder.is_some())
            || !self.bounces.is_empty()
            || !self.flights.is_empty()
    }
}

/// Runs `action` on the app and logs failures; a transient failure must not
/// terminate the dock.
pub(crate) fn with_app(what: &str, action: impl FnOnce(&mut App) -> Result<()>) {
    with_app_then(what, |app| action(app).map(|()| None));
}

/// Like `with_app`, but `action` can request work to perform after releasing
/// the app state (see [`Outside`]).
pub(crate) fn with_app_then(what: &str, action: impl FnOnce(&mut App) -> Result<Option<Outside>>) {
    let outside = APP.with(|cell| {
        // Modal calls run outside the borrow, so this should not happen. If it
        // does, discard the message rather than terminating the dock.
        let Ok(mut app) = cell.try_borrow_mut() else {
            eprintln!("muelle: {what}: el dock estaba ocupado; se ignora");
            return None;
        };
        match app.as_mut().map(action) {
            Some(Ok(outside)) => outside,
            Some(Err(error)) => {
                crate::recovery::graphics_error(&error);
                muelle_shell::log::line(&format!("{what}: {error:#}"));
                None
            }
            None => None,
        }
    });
    if let Some(outside) = outside {
        run_outside(outside);
    }
}

unsafe extern "system" fn on_win_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    object: i32,
    child: i32,
    _thread: u32,
    _time: u32,
) {
    if object != OBJID_WINDOW.0 {
        return;
    }
    if event == EVENT_SYSTEM_MINIMIZESTART {
        with_app("minimizar", |app| app.on_minimize_start(hwnd.0 as isize));
    } else if event == EVENT_SYSTEM_MINIMIZEEND {
        // Restored from outside the dock (Alt+Tab, the app itself): no shell
        // hook reports it, and a window the dock still thought minimized kept
        // its thumbnail and skipped the genie on its next minimize.
        with_app("restaurar", App::sync);
    } else if event == EVENT_SYSTEM_FOREGROUND {
        if tracker::is_task_switcher(hwnd) {
            with_app("Alt+Tab", App::task_switcher_opened);
        } else if !tracker::is_switch_transient(hwnd) {
            LAST_FRONT.store(hwnd.0 as isize, Ordering::Relaxed);
            with_app("ventana activa", App::on_foreground_changed);
        }
    } else if event == EVENT_OBJECT_LOCATIONCHANGE {
        // Only the foreground top-level window matters. Ignore child controls,
        // carets and background animations, and coalesce resize bursts using
        // the existing full-screen timer, without enumerating apps or polling.
        if child == 0 && hwnd.0 as isize == tracker::foreground_window() {
            unsafe { SetTimer(Some(main_window()), FULLSCREEN_TIMER, SYNC_DELAY_MS, None) };
        }
    } else if event == EVENT_OBJECT_CLOAKED || event == EVENT_OBJECT_UNCLOAKED {
        // Shows the windows of the virtual desktop in front, as the Windows
        // taskbar does. A desktop switch cloaks many windows at once; the
        // timer turns the burst into one re-read.
        // SAFETY: A timer on our own window.
        unsafe {
            SetTimer(Some(main_window()), SYNC_TIMER, SYNC_DELAY_MS, None);
        }
    }
}

/// Like `with_app`, for callers that need a value back (OLE callbacks).
fn with_app_value<T: Default>(what: &str, action: impl FnOnce(&mut App) -> Result<T>) -> T {
    let mut value = T::default();
    with_app(what, |app| {
        value = action(app)?;
        Ok(())
    });
    value
}

pub fn menu_bar_clicked(x: f32, y: f32) {
    with_app_then("barra superior", |app| {
        if let Some(bar) = &mut app.menubar {
            if let Some(anchor) = bar.input_switcher_anchor(x, y) {
                bar.dismiss_island(&app.gfx)?;
                return Ok(Some(Outside::InputMenu {
                    gfx: app.gfx.clone(),
                    anchor,
                    dark: !uses_light_theme(),
                }));
            }
            bar.click(&app.gfx, x, y)?;
        }
        Ok(None)
    });
}

pub fn menu_bar_volume_press(x: f32, y: f32) {
    with_app("volumen", |app| {
        if let Some(bar) = &mut app.menubar {
            bar.press_volume(&app.gfx, x, y)?;
        }
        Ok(())
    });
}

pub fn menu_bar_scrolled(point: POINT, delta: i16) -> bool {
    with_app_value("desplazar uso de IA", |app| match &mut app.menubar {
        Some(bar) => bar.scroll_usage_at(point, delta),
        None => Ok(false),
    })
}

pub fn menu_bar_volume_release(x: Option<f32>) -> bool {
    with_app_value("volumen", |app| match &mut app.menubar {
        Some(bar) => bar.release_volume(x),
        None => Ok(false),
    })
}

pub fn menu_bar_volume_flush() {
    with_app("volumen", |app| match &mut app.menubar {
        Some(bar) => bar.flush_volume(),
        None => Ok(()),
    });
}

pub fn menu_bar_media_changed() {
    with_app("reproducción", |app| match &mut app.menubar {
        Some(bar) => bar.media_changed(&app.gfx),
        None => Ok(()),
    });
}

pub fn menu_bar_audio_changed() {
    with_app("audio", |app| match &mut app.menubar {
        Some(bar) => bar.audio_changed(&app.gfx),
        None => Ok(()),
    });
}

pub fn menu_bar_accessories_changed() {
    with_app("accesorios", |app| match &mut app.menubar {
        Some(bar) => bar.accessories_changed(&app.gfx),
        None => Ok(()),
    });
}

pub fn menu_bar_accessories_reset() {
    with_app("reanudar accesorios", |app| match &mut app.menubar {
        Some(bar) => bar.restart_accessories(&app.gfx),
        None => Ok(()),
    });
}

pub fn multi_window_view_tick() {
    with_app("vistas de ventanas", |app| {
        if let Some(view) = &mut app.preview
            && !view.tick()?
        {
            app.close_preview();
        }
        Ok(())
    });
}

pub fn multi_window_view_press() {
    with_app("seleccionar ventana", |app| {
        if let Some(view) = &mut app.preview {
            view.press()?;
        }
        Ok(())
    });
}

pub fn multi_window_view_release() {
    with_app_then("abrir ventana", |app| {
        let action = match &mut app.preview {
            Some(view) => view.release()?,
            None => None,
        };
        match action {
            Some(MultiWindowAction::Activate(hwnd)) => {
                app.close_preview();
                // Resolve against the current tracker snapshot: a closed window
                // must never select a different entry by its former index.
                if app.windows.iter().any(|window| window.hwnd == hwnd) {
                    return Ok(app.activating(vec![hwnd]));
                }
            }
            Some(MultiWindowAction::Close(hwnd)) => {
                muelle_shell::tracker::request_close_window(hwnd);
                if let Some(view) = &mut app.preview {
                    let still_open = view.remove_window(&app.gfx, hwnd)?;
                    if !still_open {
                        app.close_preview();
                    }
                }
            }
            None => {}
        }
        Ok(None)
    });
}

pub fn multi_window_view_middle_click() {
    with_app("cerrar ventana por clic central", |app| {
        let target = match &mut app.preview {
            Some(view) => view.middle_click()?,
            None => None,
        };
        if let Some(hwnd) = target {
            muelle_shell::tracker::request_close_window(hwnd);
            if let Some(view) = &mut app.preview {
                let still_open = view.remove_window(&app.gfx, hwnd)?;
                if !still_open {
                    app.close_preview();
                }
            }
        }
        Ok(())
    });
}

pub fn multi_window_view_scroll(previous: bool) {
    with_app("más ventanas", |app| {
        if let Some(view) = &mut app.preview {
            view.scroll(&app.gfx, previous)?;
        }
        Ok(())
    });
}

pub fn multi_window_view_dismiss() {
    with_app("cerrar vistas", |app| {
        app.close_preview();
        Ok(())
    });
}

pub fn group_view_tick() {
    with_app("grupo", |app| {
        if let Some(view) = &mut app.group_view
            && !view.tick()?
        {
            app.close_group_view();
        }
        // Capture from a background popup does not guarantee mouse-move
        // delivery once the pointer leaves it. Poll only its active press.
        if let Some(view) = &app.group_view
            && let Some((id, point)) = view.drag_started()?
        {
            app.start_group_drag(id, point)?;
        }
        Ok(())
    });
}

pub fn group_view_press() {
    with_app("grupo", |app| match &mut app.group_view {
        Some(view) => view.press(),
        None => Ok(()),
    });
}

pub fn group_view_moved() {
    with_app("arrastre desde un grupo", App::group_view_moved);
}

pub fn group_view_release() {
    with_app_then("abrir desde un grupo", App::group_view_released);
}

pub fn group_view_menu() {
    with_app_then("menú de un grupo", App::group_view_menu);
}

pub fn group_view_press_lost() {
    with_app("grupo", |app| {
        if let Some(view) = &mut app.group_view {
            view.cancel_press();
        }
        Ok(())
    });
}

pub fn menu_bar_check_pointer() {
    with_app("barra superior", |app| {
        if app.menu_open {
            Ok(())
        } else {
            match &mut app.menubar {
                Some(bar) => bar.check_pointer(&app.gfx),
                None => Ok(()),
            }
        }
    });
}

pub fn menu_bar_finish_hide(completed_epoch: Option<usize>) {
    with_app("ocultar barra superior", |app| match &mut app.menubar {
        Some(bar) => bar.finish_hide(&app.gfx, completed_epoch),
        None => Ok(()),
    });
}

pub fn menu_bar_pointer_moved(x: f32, y: f32) {
    with_app("barra superior", |app| match &mut app.menubar {
        Some(bar) => bar.pointer_moved(&app.gfx, x, y),
        None => Ok(()),
    });
}

pub fn menu_bar_dismiss_island() {
    with_app("barra superior", |app| match &mut app.menubar {
        Some(bar) => bar.dismiss_island(&app.gfx),
        None => Ok(()),
    });
}

/// Redraws the menu bar from what it reads on each draw (input language,
/// clock), without asking the network or audio again.
pub fn menu_bar_refresh() {
    with_app("barra superior", |app| {
        if let Some(bar) = &mut app.menubar {
            bar.refresh(&app.gfx)?;
        }
        app.take_bar_cue();
        Ok(())
    });
}

pub fn menu_bar_registry_changed() {
    with_app("barra superior", |app| match &mut app.menubar {
        Some(bar) => bar.registry_changed(&app.gfx),
        None => Ok(()),
    });
}

pub fn menu_bar_tick() {
    with_app("barra superior", |app| {
        if let Some(bar) = &mut app.menubar {
            bar.tick(&app.gfx)?;
        }
        app.take_bar_cue();
        Ok(())
    });
}

pub fn drag_enter(files: Vec<String>, x: i32, y: i32) -> DROPEFFECT {
    with_app_value("arrastre de archivos", |app| {
        app.on_drag_enter(files, POINT { x, y })
    })
}

pub fn drag_over(x: i32, y: i32) -> DROPEFFECT {
    with_app_value("arrastre de archivos", |app| {
        app.on_drag_over(POINT { x, y })
    })
}

pub fn drag_leave() {
    with_app("arrastre de archivos", App::on_drag_leave);
}

pub fn drop_files(x: i32, y: i32) -> DROPEFFECT {
    with_app_value("soltar archivos", |app| app.on_drop(POINT { x, y }))
}

/// Checks the recycle bin on a background thread by scanning each drive
/// (measured at 50–75 ms), so the UI is not blocked. The result arrives via
/// `BIN_STATE`.
fn query_bin_in_background() {
    if !BIN_QUERY.request() {
        return;
    }
    let window = MAIN_WINDOW.load(Ordering::Relaxed);
    let spawned = thread::Builder::new()
        .name("muelle-recycle-query".into())
        .spawn(move || {
            power::set_thread_efficiency(true);
            // SAFETY: COM is initialized and released on this thread; the message
            // is harmless if the window no longer exists.
            unsafe {
                let com = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                loop {
                    BIN_QUERY.begin_pass();
                    let full = recycle_bin::has_items();
                    let _ = PostMessageW(
                        Some(HWND(window as *mut _)),
                        BIN_STATE,
                        WPARAM(usize::from(full)),
                        LPARAM(0),
                    );
                    if !BIN_QUERY.needs_another_pass() {
                        break;
                    }
                }
                if com.is_ok() {
                    CoUninitialize();
                }
            }
        });
    if let Err(error) = spawned {
        BIN_QUERY.0.store(0, Ordering::Release);
        eprintln!("muelle: no se pudo consultar la Papelera: {error}");
    }
}

fn main_window() -> HWND {
    HWND(MAIN_WINDOW.load(Ordering::Relaxed) as *mut _)
}

fn run_outside(outside: Outside) {
    let submit = |action| {
        let result = shell_work::dispatch(action);
        if let Err(error) = result {
            eprintln!("muelle: operación no iniciada: {error:#}");
            shell_work::notify_not_started();
        }
    };
    match outside {
        Outside::Open(id) => submit(ShellAction::Open { id }),
        Outside::OpenStart => launcher::open_start_menu(),
        Outside::Activate(windows) => launcher::activate(&windows),
        Outside::Minimize(windows) => launcher::minimize(&windows),
        Outside::Reveal(path) => submit(ShellAction::Reveal { path }),
        Outside::OpenWith(app, files) => submit(ShellAction::OpenWith { app, files }),
        Outside::Recycle(files) => submit(ShellAction::Recycle { files }),
        Outside::Transfer {
            folder,
            files: paths,
            copy,
        } => submit(ShellAction::Transfer {
            files: paths,
            folder,
            copy,
        }),
        Outside::EmptyBin => submit(ShellAction::EmptyBin),
        Outside::QuitDock => request_quit(),
        Outside::OpenAsAdmin(id) => submit(ShellAction::OpenAsAdmin { id }),
        Outside::ShowDesktop => launcher::show_desktop(),
        Outside::UnpinFromTaskbar(id) => submit(ShellAction::Unpin { id }),
        Outside::OpenJump { target, program } => submit(ShellAction::OpenJump { target, program }),
        Outside::OpenSettings => {
            // The settings app is installed next to the dock.
            let settings = env::current_exe()
                .ok()
                .map(|exe| exe.with_file_name("muelle-settings.exe"));
            match settings {
                Some(path) if path.exists() => submit(ShellAction::Open {
                    id: path.to_string_lossy().into_owned(),
                }),
                _ => eprintln!("muelle: no se encontró muelle-settings.exe junto al dock"),
            }
        }
        Outside::InputMenu { gfx, anchor, dark } => {
            use muelle_shell::keyboard;
            let context = keyboard::Context::current();
            let layouts = keyboard::layouts(strings::text().keyboard);
            const SETTINGS: u32 = u32::MAX;
            let mut items: Vec<MenuItem> = layouts
                .iter()
                .enumerate()
                .map(|(index, layout)| MenuItem::Action {
                    command: index as u32 + 1,
                    label: layout.name.replace('&', "&&"),
                    checked: context.is_some_and(|context| context.layout == layout.handle),
                    enabled: context.is_some(),
                    icon: None,
                })
                .collect();
            if !items.is_empty() {
                items.push(MenuItem::Separator);
            }
            items.push(MenuItem::action(
                SETTINGS,
                strings::text().keyboard_settings,
            ));
            let owner = main_window();
            with_app("abrir selector de teclado", |app| {
                app.menu_open = true;
                Ok(())
            });
            let chosen = menu::show_below(&gfx, owner, &items, anchor, dark);
            with_app("cerrar selector de teclado", App::after_menu);
            if chosen == Some(SETTINGS) {
                submit(ShellAction::Open {
                    id: "ms-settings:regionlanguage".into(),
                });
            } else if let (Some(context), Some(layout)) = (
                context,
                chosen
                    .and_then(|command| command.checked_sub(1))
                    .and_then(|index| layouts.get(index as usize)),
            ) && let Err(error) = context.request(layout.handle)
            {
                muelle_shell::log::line(&format!("cambiar teclado: {error}"));
                shell_work::notify_not_started();
            }
            menu_bar_refresh();
        }
        Outside::EndTask(target) => {
            // Captured kernel handles are Send; no HWND or PID is resolved
            // again after the menu. Never wait on an unresponsive UI thread.
            let started = thread::Builder::new()
                .name("muelle-end-task".into())
                .spawn(move || {
                    if let Err(error) = target.terminate() {
                        muelle_shell::log::line(&format!("finalizar tarea: {error}"));
                        shell_work::notify_end_task_failed();
                    }
                });
            if started.is_err() {
                shell_work::notify_end_task_failed();
            }
        }
        Outside::Menu(request) => {
            let owner = HWND(MAIN_WINDOW.load(Ordering::Relaxed) as *mut _);
            with_app("abrir menú", |app| {
                app.menu_open = true;
                app.render()
            });
            let chosen = menu::show(
                &request.gfx,
                owner,
                &request.items,
                request.anchor,
                request.dark,
                request.placement,
            );
            with_app("menú cerrado", App::after_menu);
            if let Some(command) = chosen {
                with_app_then("orden del menú", |app| {
                    app.on_command(request.target, command)
                });
            }
        }
        Outside::FolderMenu {
            folder,
            anchor,
            placement,
            dark,
        } => {
            with_app("abrir carpeta de accesos directos", |app| {
                app.menu_open = true;
                app.render()
            });
            let chosen = folder_menu::show(main_window(), &folder, anchor, dark, placement);
            with_app("menú cerrado", App::after_menu);
            if let Some(path) = chosen {
                submit(ShellAction::Open { id: path });
            }
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        HEALTH_CHECK => with_app("estado del dispositivo gráfico", |app| {
            app.gfx.check_device()
        }),
        WM_MOUSEACTIVATE => {
            let mouse = ((l.0 >> 16) & 0xFFFF) as u32;
            let activate = matches!(mouse, WM_LBUTTONDOWN | WM_MBUTTONDOWN)
                && with_app_value("activación por clic", |app| {
                    Ok(hwnd == app.input && app.activate_for_pointer())
                });
            // This is Windows' actual pointer activation negotiation. Preserve
            // the button message so the same click also opens the chosen app.
            return LRESULT(if activate { MA_ACTIVATE } else { MA_NOACTIVATE } as isize);
        }
        ASSETS_READY => {
            // SAFETY: Coalesce background results on this UI window.
            unsafe { SetTimer(Some(hwnd), ASSETS_TIMER, ASSETS_DELAY_MS, None) };
        }
        WM_TIMER if w.0 == ASSETS_TIMER => {
            // SAFETY: This UI thread owns the one-shot timer.
            unsafe {
                let _ = KillTimer(Some(hwnd), ASSETS_TIMER);
            }
            with_app("iconos del shell", App::assets_ready);
        }
        WM_MOUSEMOVE => with_app("movimiento del ratón", App::on_mouse_move),
        WM_MOUSELEAVE => with_app("salida del ratón", App::on_mouse_leave),
        WM_LBUTTONDOWN => with_app("pulsación", App::on_press),
        WM_LBUTTONUP => with_app_then("clic", App::on_release),
        WM_MBUTTONUP => with_app_then("clic central", App::on_middle_click),
        WM_CAPTURECHANGED => {
            // SAFETY: Posting a message to our own window.
            unsafe {
                let _ = PostMessageW(Some(hwnd), DRAG_CANCELLED, WPARAM(0), LPARAM(0));
            }
        }
        DRAG_CANCELLED => {
            // ReleaseCapture posts this too. A newer press may already
            // own capture by the time the deferred cancellation runs.
            if unsafe { GetCapture() } != hwnd {
                with_app("arrastre", App::cancel_drag);
            }
        }
        WM_TIMER if w.0 == DRAG_TIMER => with_app_then("arrastre", App::poll_drag),
        DROP_RUN => with_app_then("soltar archivos", |app| Ok(app.pending_drop.take())),
        WM_RBUTTONUP => with_app_then("menú", App::on_right_click),
        BIN_STATE => with_app("papelera", |app| app.set_bin_full(w.0 != 0)),
        TASKBAR_PREFS_CHANGED => {
            with_app("preferencias de badges", App::taskbar_preferences_changed)
        }
        BIN_CHANGED => {
            // SAFETY: `hwnd` is this live window on its owning UI thread.
            unsafe { SetTimer(Some(hwnd), BIN_TIMER, BIN_DELAY_MS, None) };
        }
        WM_SETTINGCHANGE if w.0 == SPI_SETWORKAREA => {}
        WM_SETTINGCHANGE | WM_DISPLAYCHANGE | WM_DPICHANGED => {
            if matches!(msg, WM_DISPLAYCHANGE | WM_DPICHANGED) {
                with_app("pantalla de la mascota", |app| {
                    app.stop_mascot();
                    Ok(())
                });
            }
            // SAFETY: `hwnd` is our window; scheduling the same timer resets it.
            unsafe {
                SetTimer(Some(hwnd), REFRESH_TIMER, REFRESH_DELAY_MS, None);
                if msg == WM_DISPLAYCHANGE && hwnd == main_window() {
                    // Let Explorer settle and create any new secondary taskbars.
                    SetTimer(Some(hwnd), TASKBAR_TIMER, TASKBAR_DELAY_MS, None);
                }
            }
        }
        // On battery or charging, or energy saver turned on or off: glass may
        // have to become solid, or the other way round.
        WM_POWERBROADCAST if w.0 == PBT_APMPOWERSTATUSCHANGE || w.0 == PBT_POWERSETTINGCHANGE => {
            if w.0 == PBT_POWERSETTINGCHANGE {
                // SAFETY: Windows owns the setting payload for this message.
                unsafe {
                    power::power_setting_changed(l);
                }
            }
            // SAFETY: `hwnd` is our window; scheduling the same timer resets it.
            unsafe { SetTimer(Some(hwnd), REFRESH_TIMER, REFRESH_DELAY_MS, None) };
        }
        WM_TIMER if w.0 == BIN_TIMER => {
            // SAFETY: `hwnd` is this live window on its owning UI thread.
            unsafe {
                let _ = KillTimer(Some(hwnd), w.0);
            }
            query_bin_in_background();
        }
        PINS_CHANGED => {
            with_app("apps fijadas", |app| {
                if let Some(watch) = &mut app.pin_watch {
                    watch.arm();
                }
                Ok(())
            });
            // SAFETY: `hwnd` is our window; scheduling the same timer resets it.
            unsafe { SetTimer(Some(hwnd), PINS_TIMER, PINS_DELAY_MS, None) };
        }
        WM_TIMER if w.0 == PINS_TIMER => {
            // SAFETY: `hwnd` is this live window on its owning UI thread.
            unsafe {
                let _ = KillTimer(Some(hwnd), w.0);
            }
            with_app("apps fijadas", App::sync_taskbar_pins);
        }
        WM_TIMER if w.0 == UPDATE_TIMER => {
            with_app("actualización", |app| {
                app.update_now();
                Ok(())
            });
        }
        WM_TIMER if w.0 == SWITCHER_TIMER => {
            with_app("Alt+Tab", App::check_task_switcher);
        }
        WM_TIMER if w.0 == FULLSCREEN_TIMER => {
            with_app("pantalla completa", App::update_fullscreen);
        }
        WM_TIMER if w.0 == SPRING_TIMER => {
            with_app_then("arrastre sobre una app", App::spring_open);
        }
        WM_TIMER if w.0 == MASCOT_TIMER => with_app("cangrejo", App::mascot_tick),
        edge_watch::CHANGED => with_app("borde de pantalla", App::edge_changed),
        WM_TIMER if w.0 == EDGE_TIMER => with_app("mostrar al llegar al borde", App::edge_rested),
        WM_TIMER if w.0 == PREVIEW_TIMER => with_app("selector de ventanas", App::preview_tick),
        WM_TIMER if w.0 == HIDE_TIMER => {
            // SAFETY: Same as above.
            unsafe {
                let _ = KillTimer(Some(hwnd), w.0);
            }
            with_app("ocultación", App::hide_if_unused);
        }
        WM_TIMER if w.0 == TRIM_TIMER => {
            // SAFETY: `hwnd` is this live window on its owning UI thread.
            unsafe {
                let _ = KillTimer(Some(hwnd), w.0);
            }
            with_app("recorte de memoria", |app| {
                app.trim_if_idle();
                Ok(())
            });
        }
        WM_TIMER if [REFRESH_TIMER, SYNC_TIMER, TASKBAR_TIMER].contains(&w.0) => {
            // SAFETY: `hwnd` is this live window on its owning UI thread.
            unsafe {
                let _ = KillTimer(Some(hwnd), w.0);
            }
            match w.0 {
                REFRESH_TIMER => with_app("recomposición del dock", |app| {
                    app.apply_edge_settings();
                    app.refresh()?;
                    app.update_fullscreen()?;
                    app.power_moment();
                    Ok(())
                }),
                SYNC_TIMER => with_app("lectura de ventanas", App::sync),
                _ => with_app("ocultar la barra de Windows", |app| {
                    app.request_taskbar(true)
                }),
            }
        }
        APPBAR_CALLBACK => with_app("aviso de AppBar", |app| app.on_appbar_notice(w.0)),
        // Once the taskbar is set to auto-hide, the dock strip moves to the
        // screen edge, so reserve and reposition it again.
        TASKBAR_TAKEN => with_app("reserva de espacio", |app| {
            app.taskbar_taken(w.0 != 0, l.0 != 0)
        }),
        WM_POWERBROADCAST if w.0 == PBT_APMRESUMEAUTOMATIC => {
            with_app("reanudar reserva de espacio", |app| {
                app.taskbar_attempts = 0;
                Ok(())
            });
            // SAFETY: `hwnd` is this live window on its owning UI thread.
            unsafe { SetTimer(Some(hwnd), TASKBAR_TIMER, TASKBAR_DELAY_MS, None) };
            // Timers do not count the time asleep: aim it again by the clock.
            with_app("actualización", |app| {
                app.schedule_update();
                Ok(())
            });
        }
        _ if CONFIG_CHANGED.get() == Some(&msg) => with_app("ajustes", App::reload_config),
        _ if TASKBAR_CREATED.get() == Some(&msg) => {
            with_app("Explorer reiniciado", |app| {
                app.taskbar_attempts = 0;
                Ok(())
            });
            // SAFETY: `hwnd` is this live window on its owning UI thread.
            unsafe { SetTimer(Some(hwnd), TASKBAR_TIMER, TASKBAR_DELAY_MS, None) };
        }
        // A folder's submenu in the shortcuts folder's menu is about to open.
        WM_INITMENUPOPUP if folder_menu::init_popup(HMENU(w.0 as *mut _)) => {}
        WM_COPYDATA => {
            let cds =
                unsafe { &*(l.0 as *const windows::Win32::System::DataExchange::COPYDATASTRUCT) };
            if cds.dwData == muelle_shell::dock_link::NOTIFY_DATA_TAG && cds.cbData > 0 {
                let bytes = unsafe {
                    std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize)
                };
                if let Some(payload) = std::str::from_utf8(bytes)
                    .ok()
                    .and_then(muelle_shell::dock_link::NotificationPayload::decode)
                {
                    with_app("notificación", |app| {
                        app.on_notification(payload);
                        Ok(())
                    });
                    return LRESULT(1);
                }
            }
        }
        WM_DESTROY => {
            notification_banner::NotificationBannerView::hide();
            muelle_shell::system::set_windows_toasts_enabled(true);
            unsafe { PostQuitMessage(0) };
        }
        _ if SHELL_HOOK.get() == Some(&msg) => match w.0 {
            // Changing the foreground window does not change the app list; it
            // can only enter or exit full-screen mode.
            HSHELL_WINDOWACTIVATED | HSHELL_RUDEAPPACTIVATED => {
                with_app("ventana activa", App::on_foreground_changed);
            }
            // Terminals and browsers retitle their windows many times a minute.
            // The dock shows no titles, so only a window it does not list yet
            // (an app that declares its identity late) needs a new look.
            HSHELL_REDRAW if is_tracked(l.0) => {}
            // The input language indicator of the menu bar.
            HSHELL_LANGUAGE => menu_bar_refresh(),
            code => {
                if code == HSHELL_FLASH {
                    with_app("aviso de atención", |app| app.on_attention(l.0));
                } else if code == HSHELL_WINDOWDESTROYED {
                    with_app("ventana cerrada", |app| app.forget_attention(l.0));
                }
                // SAFETY: `hwnd` is this live window on its owning UI thread.
                unsafe { SetTimer(Some(hwnd), SYNC_TIMER, SYNC_DELAY_MS, None) };
            }
        },
        _ => return unsafe { DefWindowProcW(hwnd, msg, w, l) },
    }
    LRESULT(0)
}

/// `true` if the dock already lists the window `hwnd`.
fn is_tracked(hwnd: isize) -> bool {
    APP.with(|cell| {
        cell.try_borrow().is_ok_and(|app| {
            app.as_ref()
                .is_some_and(|app| app.windows.iter().any(|window| window.hwnd == hwnd))
        })
    })
}

fn is_animating() -> bool {
    APP.with_borrow(|app| app.as_ref().is_some_and(App::is_animating))
}

/// Main loop. Sleeps while idle (0% CPU) and advances one frame per display
/// refresh while an animation is running.
///
/// Outside animations, the thread uses EcoQoS because remaining work (window
/// tracking and recomposition) is not time-sensitive.
fn message_loop() {
    let mut msg = MSG::default();
    let mut was_animating = None;
    unsafe {
        loop {
            let animating = is_animating();
            if was_animating != Some(animating) {
                power::set_thread_efficiency(!animating);
                if was_animating == Some(true) {
                    with_app("recorte de memoria", |app| {
                        app.schedule_trim();
                        Ok(())
                    });
                }
                was_animating = Some(animating);
            }
            if animating {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    if msg.message == WM_QUIT {
                        return;
                    }
                    DispatchMessageW(&msg);
                }
                with_app_then("animación", App::tick);
                // Wait for the next refresh to pace animation without timers.
                let _ = DwmFlush();
            } else {
                let status = GetMessageW(&mut msg, None, 0, 0);
                if status.0 <= 0 {
                    if status.0 < 0 {
                        eprintln!("muelle: error al leer la cola de mensajes de Windows");
                    }
                    return;
                }
                DispatchMessageW(&msg);
            }
        }
    }
}

pub fn run() -> Result<()> {
    let startup = Instant::now();
    muelle_shell::log::line(concat!("inicio, versión ", env!("CARGO_PKG_VERSION")));
    let gfx = Gfx::new()?.with_error_handler(crate::recovery::graphics_error);
    // SAFETY: The thread is already a single-threaded COM apartment (see
    // `main`); OLE adds drag and drop on top of it and is released at the end.
    unsafe { OleInitialize(None)? };

    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WINDOW_CLASS;
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: class,
            ..Default::default()
        });

        let base = WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        let create = |extra: WINDOW_EX_STYLE| {
            CreateWindowExW(
                base | extra,
                class,
                w!("Muelle"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance.into()),
                None,
            )
        };
        let visual = create(WS_EX_LAYERED | WS_EX_TRANSPARENT)?;
        let input = create(WINDOW_EX_STYLE::default())?;

        // Without this, the system backdrop brush appears black. Windows 10
        // does not know the attribute; the dock then draws solid glass
        // (`system::transparency_enabled`), so the error is not fatal.
        let enable = BOOL::from(true);
        let _ = DwmSetWindowAttribute(
            visual,
            DWMWA_USE_HOSTBACKDROPBRUSH,
            (&raw const enable).cast(),
            size_of::<BOOL>() as u32,
        );

        let target = gfx
            .compositor
            .cast::<ICompositorDesktopInterop>()?
            .CreateDesktopWindowTarget(visual, true)?;

        muelle_config::migrate_legacy_folder();
        let config = load_config();
        strings::set_language(config.language);
        let roster = roster_from(&config);
        let auto_hide = config.auto_hide;
        let slide_omega = config.dock_auto_hide_speed.omega();
        let mut app = App {
            visual,
            input,
            gfx,
            target,
            appbar: AppBar::register(visual, APPBAR_CALLBACK),
            taskbar_worker: None,
            taskbar_retry: false,
            taskbar_attempts: 0,
            fullscreen: false,
            roster,
            tracker: WindowTracker::default(),
            windows: Vec::new(),
            preview_order: PreviewOrder::default(),
            last_rects: HashMap::new(),
            entries: Vec::new(),
            icons: Visuals::new(visual, ASSETS_READY)?,
            downloads: downloads_folder(),
            bin_full: false,
            _bin_watch: BinWatch::register(input, BIN_CHANGED),
            _saver_watch: power::SaverWatch::new(input.0 as isize),
            pin_watch: muelle_shell::pins::watch(visual.0 as isize, PINS_CHANGED),
            badge_watch: status::watch_taskbar_settings(visual.0 as isize, TASKBAR_PREFS_CHANGED),
            attention: Attention::default(),
            attention_enabled: status::taskbar_badges_enabled(),
            assets: HashMap::new(),
            assets_for: (0.0, false, 0, DockEdge::Bottom),
            placeholder: None,
            remove_label: None,
            config,
            slide: Spring::at(if auto_hide { 1.0 } else { 0.0 }, slide_omega),
            press: None,
            drag: None,
            incoming: Vec::new(),
            incoming_pin: None,
            drag_over: false,
            spring: None,
            switching: false,
            switch_started: Instant::now(),
            switch_ended: None,
            launched: Instant::now(),
            update_tried: None,
            follow: None,
            pointed: false,
            pending_drop: None,
            bounces: HashMap::new(),
            notifications: muelle_core::notifications::NotificationQueue::new(),
            minimized: Vec::new(),
            thumbnails: Thumbnails::new(visual),
            flights: Vec::new(),
            snapshots: Vec::new(),
            menubar: None,
            edge_watch: None,
            dock_display: None,
            names: HashMap::new(),
            calmed: Vec::new(),
            started: false,
            stage: None,
            hovering: false,
            hovered: None,
            preview_candidate: None,
            preview: None,
            group_view: None,
            group_was_open: None,
            ghost: None,
            menu_open: false,
            mascot: None,
            // Initial registration notifications establish the baseline after
            // the queue starts, without playing a startup charging/saver act.
            power_seen: None,
            charging_shown: None,
            cursor: 0.0,
            amount: Spring::at(0.0, HOVER_OMEGA),
            last_frame: Instant::now(),
        };
        let _ = TASKBAR_CREATED.set(RegisterWindowMessageW(w!("TaskbarCreated")));
        let _ = CONFIG_CHANGED.set(RegisterWindowMessageW(
            muelle_shell::dock_link::CONFIG_CHANGED,
        ));
        MAIN_WINDOW.store(visual.0 as isize, Ordering::Relaxed);
        // The dock inherits the "ignore Ctrl+C" attribute from its parent
        // (some tools enable it), so clear it to ensure Ctrl+C is handled.
        let _ = SetConsoleCtrlHandler(None, false);
        let _ = SetConsoleCtrlHandler(Some(on_console_ctrl), true);

        // Apps uninstalled while Muelle was closed leave the dock.
        if app.prune_missing_apps() {
            save_settings(&app.roster, &app.config);
        }
        // Pins and unpins made on the Windows taskbar while Muelle was closed.
        if let Err(error) = app.sync_taskbar_pins() {
            eprintln!("muelle: apps fijadas de la barra de Windows: {error:#}");
        }
        app.sync()?;
        // `sync` recomposes only when the list changes; run it unconditionally at startup.
        if app.stage.is_none() {
            app.refresh()?;
        }
        // After the dock: creating the bar and reserving its strip take
        // 60-130 ms, which the dock no longer waits for.
        app.apply_menu_bar_setting();
        // Otherwise the bar shows no app name until the user switches windows.
        if let Err(error) = app.update_menu_bar_app() {
            eprintln!("muelle: barra superior: {error:#}");
        }

        // The shell notifies this window when a window is created, closed,
        // activated, or requests attention.
        let _ = SHELL_HOOK.set(RegisterWindowMessageW(w!("SHELLHOOK")));
        RegisterShellHookWindow(input).ok()?;
        // Delivered through this thread's message loop; no code runs in other processes.
        let minimize_hook = SetWinEventHook(
            EVENT_SYSTEM_MINIMIZESTART,
            EVENT_SYSTEM_MINIMIZEEND,
            None,
            Some(on_win_event),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );
        MINIMIZE_HOOK.store(minimize_hook.0 as isize, Ordering::Relaxed);
        // Switching virtual desktops hides (cloaks) the windows of the one
        // left and shows those of the one entered, and no shell hook says so.
        let cloak_hook = SetWinEventHook(
            EVENT_OBJECT_CLOAKED,
            EVENT_OBJECT_UNCLOAKED,
            None,
            Some(on_win_event),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );
        CLOAK_HOOK.store(cloak_hook.0 as isize, Ordering::Relaxed);
        // Alt+Tab: the switcher coming to the front. One event per window
        // switch, so it costs nothing while the user works in one app.
        let foreground_hook = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(on_win_event),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );
        FOREGROUND_HOOK.store(foreground_hook.0 as isize, Ordering::Relaxed);
        let location_hook = SetWinEventHook(
            EVENT_OBJECT_LOCATIONCHANGE,
            EVENT_OBJECT_LOCATIONCHANGE,
            None,
            Some(on_win_event),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );
        LOCATION_HOOK.store(location_hook.0 as isize, Ordering::Relaxed);
        LAST_FRONT.store(tracker::foreground_window(), Ordering::Relaxed);
        let target: IDropTarget = DropTarget.into();
        RegisterDragDrop(input, &target)?;
        DROP_WINDOW.store(input.0 as isize, Ordering::Relaxed);

        app.schedule_update();
        if app.config.notifications {
            muelle_shell::system::set_windows_toasts_enabled(false);
        }
        APP.set(Some(app));
    }

    // Hiding the taskbar is a synchronous Explorer call that takes 50–500 ms
    // depending on load. Run it off the UI thread after the dock is visible.
    query_bin_in_background();
    with_app("ocultar la barra de Windows", |app| {
        app.request_taskbar(false)
    });

    muelle_shell::log::line(&format!(
        "interfaz inicializada en {} ms desde app::run; comienza el bucle de mensajes",
        startup.elapsed().as_millis()
    ));
    message_loop();
    // SAFETY: The drop target was registered on this window in this thread.
    unsafe {
        let _ = RevokeDragDrop(HWND(DROP_WINDOW.load(Ordering::Relaxed) as *mut _));
        for hook in [
            &MINIMIZE_HOOK,
            &CLOAK_HOOK,
            &FOREGROUND_HOOK,
            &LOCATION_HOOK,
        ] {
            let _ = UnhookWinEvent(HWINEVENTHOOK(hook.load(Ordering::Relaxed) as *mut _));
        }
    }
    let taking_over = APP.with_borrow_mut(|app| {
        app.as_mut().and_then(|app| {
            app.restore_transitions();
            app.taskbar_worker.take()
        })
    });
    APP.set(None);
    muelle_shell::system::set_windows_toasts_enabled(true);
    // SAFETY: Balances `OleInitialize` above, on the same thread.
    unsafe { OleUninitialize() };
    // Wait for taskbar takeover to finish before restoring it.
    if let Some(worker) = taking_over {
        let _ = worker.join();
    }
    Ok(())
}

#[cfg(test)]
mod dock_preferences_tests {
    use super::*;

    #[test]
    fn removing_fixed_shortcuts_preserves_folders_pins_and_other_preferences() -> Result<()> {
        struct Fixture(std::path::PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
                let _ = std::fs::remove_file(self.0.with_extension("json.lock"));
            }
        }
        let file = Fixture(env::temp_dir().join(format!(
            "muelle-fixed-shortcuts-{}.json",
            std::process::id()
        )));
        let initial = Config {
            pinned: vec!["editor".into()],
            shortcuts_folder: Some("C:\\Projects".into()),
            notification_dnd: true,
            ..Config::default()
        };
        muelle_config::save(&file.0, &initial)?;
        let downloads = hide_fixed_item(&file.0, Role::Folder)?;
        assert!(!downloads.show_downloads);
        assert!(downloads.show_trash && downloads.show_start);
        hide_fixed_item(&file.0, Role::Trash)?;
        let saved = hide_fixed_item(&file.0, Role::Start)?;
        assert!(!saved.show_downloads && !saved.show_trash && !saved.show_start);
        assert_eq!(saved.pinned, initial.pinned);
        assert_eq!(saved.shortcuts_folder, initial.shortcuts_folder);
        assert!(saved.notification_dnd);
        let before = std::fs::read(&file.0)?;
        assert!(hide_fixed_item(&file.0, Role::App).is_err());
        assert_eq!(std::fs::read(&file.0)?, before);
        Ok(())
    }

    #[test]
    fn explorer_preference_preserves_other_pins_and_follows_open_windows() {
        let explorer = explorer_id();
        let config = Config {
            pinned: vec!["editor".into(), explorer.clone(), "browser".into()],
            pin_explorer: false,
            ..Config::default()
        };
        let mut roster = roster_from(&config);
        assert_eq!(roster.pinned(), ["editor", "browser"]);
        assert!(!roster.apps().iter().any(|app| app.id == explorer));
        roster.sync(["editor", explorer.as_str()]);
        assert!(roster.is_running(&explorer) && !roster.is_pinned(&explorer));
        apply_explorer_pin(&mut roster, true);
        assert_eq!(roster.pinned(), [explorer.as_str(), "editor", "browser"]);
        apply_explorer_pin(&mut roster, false);
        assert!(roster.is_running(&explorer) && !roster.is_pinned(&explorer));
        roster.sync(["editor"]);
        apply_explorer_pin(&mut roster, false);
        assert!(!roster.apps().iter().any(|app| app.id == explorer));
        assert!(!roster.recent().contains(&explorer));
    }

    #[test]
    fn density_changes_dimensions_without_changing_icon_size() {
        let mut widths = Vec::new();
        for density in [
            DockDensity::Compact,
            DockDensity::Normal,
            DockDensity::Spacious,
        ] {
            let (gap, padding) = density.spacing();
            let metrics = Metrics {
                icon: 64.0,
                gap,
                padding,
                ..Metrics::DEFAULT
            };
            let result = layout(&[ItemKind::Icon; 3], &metrics);
            assert!(result.slots.iter().all(|slot| slot.rect.width == 64.0));
            widths.push(result.width);
        }
        assert!(widths.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn auto_hide_presets_keep_the_original_speed_as_default() {
        use muelle_config::DockAutoHideSpeed;
        let progress = |speed: DockAutoHideSpeed| {
            let mut spring = Spring::at(1.0, speed.omega());
            spring.target = 0.0;
            for _ in 0..12 {
                spring.step(1.0 / 120.0);
            }
            spring.value
        };
        assert!(progress(DockAutoHideSpeed::Smooth) > progress(DockAutoHideSpeed::Normal));
        assert!(progress(DockAutoHideSpeed::Normal) > progress(DockAutoHideSpeed::Fast));
        assert_eq!(DockAutoHideSpeed::default().omega(), 24.0);
    }
}

#[cfg(test)]
mod attention_tests {
    use super::*;

    #[test]
    fn attention_reaches_apps_groups_and_minimized_thumbnails_without_clearing_other_windows() {
        let windows = [
            TrackedWindow {
                hwnd: 7,
                app_id: "chat".into(),
                minimized: true,
            },
            TrackedWindow {
                hwnd: 8,
                app_id: "chat".into(),
                minimized: false,
            },
        ];
        let mut attention = Attention::default();
        attention.request(7, 2);
        let app = Entry {
            kind: ItemKind::Icon,
            id: Some("chat".into()),
            role: Role::App,
            running: true,
            members: Vec::new(),
        };
        let group = Entry {
            id: Some("group:chat".into()),
            role: Role::Group,
            members: vec!["chat".into()],
            ..app.clone()
        };
        let thumbnail = Entry {
            id: Some(window_id(7)),
            role: Role::Window,
            ..app.clone()
        };
        assert!(entry_has_attention(&attention, &windows, &app));
        assert!(entry_has_attention(&attention, &windows, &group));
        assert!(entry_has_attention(&attention, &windows, &thumbnail));
        assert_eq!(active_attention_window(&windows, 7), 0);
        attention.acknowledge(active_attention_window(&windows, 8));
        assert!(entry_has_attention(&attention, &windows, &app));
        attention.acknowledge(7);
        assert!(!entry_has_attention(&attention, &windows, &group));
    }

    #[test]
    #[ignore = "Requires Explorer's shell hooks; uses only off-screen test-owned windows"]
    fn native_flash_request_produces_the_shell_event_used_by_dock_badges() -> Result<()> {
        use std::time::Duration;
        use windows::Win32::UI::WindowsAndMessaging::{
            DeregisterShellHookWindow, DestroyWindow, FLASHW_STOP, FLASHW_TRAY, FLASHWINFO,
            FlashWindowEx, SW_SHOWNOACTIVATE, ShowWindow, WS_EX_APPWINDOW, WS_OVERLAPPEDWINDOW,
        };
        struct Window(HWND);
        impl Drop for Window {
            fn drop(&mut self) {
                unsafe {
                    let _ = DeregisterShellHookWindow(self.0);
                    let _ = DestroyWindow(self.0);
                }
            }
        }
        let receiver = Window(unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!("Muelle badge receiver test"),
                WS_POPUP,
                -32000,
                -32000,
                80,
                40,
                None,
                None,
                None,
                None,
            )?
        });
        let source = Window(unsafe {
            CreateWindowExW(
                WS_EX_APPWINDOW | WS_EX_NOACTIVATE,
                w!("STATIC"),
                w!("Muelle native attention test"),
                WS_OVERLAPPEDWINDOW,
                -32000,
                -32000,
                160,
                120,
                None,
                None,
                None,
                None,
            )?
        });
        let message = unsafe {
            RegisterShellHookWindow(receiver.0).ok()?;
            let _ = ShowWindow(source.0, SW_SHOWNOACTIVATE);
            RegisterWindowMessageW(w!("SHELLHOOK"))
        };
        let pump = |duration: Duration| {
            let until = Instant::now() + duration;
            let mut found = false;
            while Instant::now() < until {
                let mut event = MSG::default();
                while unsafe { PeekMessageW(&mut event, None, 0, 0, PM_REMOVE) }.as_bool() {
                    found |= event.hwnd == receiver.0
                        && event.message == message
                        && event.wParam.0 == HSHELL_FLASH
                        && event.lParam.0 == source.0.0 as isize;
                    unsafe {
                        DispatchMessageW(&event);
                    }
                }
                if found {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            found
        };
        pump(Duration::from_millis(350));
        let mut info = FLASHWINFO {
            cbSize: size_of::<FLASHWINFO>() as u32,
            hwnd: source.0,
            dwFlags: FLASHW_TRAY,
            uCount: 2,
            dwTimeout: 100,
        };
        unsafe {
            let _ = FlashWindowEx(&info);
        }
        let found = pump(Duration::from_secs(2));
        info.dwFlags = FLASHW_STOP;
        unsafe {
            let _ = FlashWindowEx(&info);
        }
        assert!(found, "Explorer did not publish the native attention event");
        Ok(())
    }
}

#[cfg(test)]
mod performance_tests {
    use super::BinQueryGate;

    #[test]
    fn recycle_notifications_share_one_scan_and_preserve_the_last_change() {
        let gate = BinQueryGate::new();
        assert!(gate.request());
        gate.begin_pass();
        for _ in 0..10_000 {
            assert!(!gate.request());
        }
        assert!(gate.needs_another_pass());
        gate.begin_pass();
        assert!(!gate.needs_another_pass());
        assert!(gate.request());
    }

    #[test]
    fn quiet_recycle_scan_allows_the_next_independent_scan() {
        let gate = BinQueryGate::new();
        assert!(gate.request());
        gate.begin_pass();
        assert!(!gate.needs_another_pass());
        assert!(gate.request());
    }
}

#[cfg(test)]
mod mascot_power_tests {
    use super::{mascot::Act, power_act};

    #[test]
    fn initial_power_state_and_repeated_notifications_do_not_play_an_act() {
        assert_eq!(power_act(None, (Some(true), true), true), None);
        assert_eq!(
            power_act(Some((Some(false), true)), (Some(false), true), true),
            None
        );
        assert_eq!(
            power_act(Some((Some(true), false)), (Some(true), false), true),
            None
        );
    }

    #[test]
    fn saver_entry_and_charging_have_distinct_acts() {
        assert_eq!(
            power_act(Some((Some(false), false)), (Some(false), true), true),
            Some(Act::Sleepy)
        );
        assert_eq!(
            power_act(Some((Some(false), false)), (Some(true), false), true),
            Some(Act::Charging)
        );
        assert_eq!(
            power_act(Some((Some(false), false)), (Some(true), true), true),
            Some(Act::Sleepy)
        );
    }

    #[test]
    fn charging_cooldown_and_unknown_battery_state_suppress_false_plug_events() {
        assert_eq!(
            power_act(Some((Some(false), false)), (Some(true), false), false),
            None
        );
        assert_eq!(
            power_act(Some((None, false)), (Some(true), false), true),
            None
        );
        assert_eq!(
            power_act(Some((Some(true), true)), (Some(false), false), true),
            Some(Act::Unplugged)
        );
    }

    #[test]
    fn unplugging_is_not_suppressed_by_charging_cooldown_or_automatic_saver() {
        assert_eq!(
            power_act(Some((Some(true), false)), (Some(false), true), false),
            Some(Act::Unplugged)
        );
        assert_eq!(
            power_act(Some((Some(true), false)), (Some(false), false), false),
            Some(Act::Unplugged)
        );
    }

    #[test]
    fn missing_or_repeated_power_readings_do_not_announce_a_disconnect() {
        for (before, now) in [
            (None, Some(false)),
            (Some(true), None),
            (Some(false), Some(false)),
        ] {
            assert_eq!(power_act(Some((before, false)), (now, false), true), None);
        }
    }
}

#[cfg(test)]
mod switcher_tests {
    use super::selection_after_closing;

    #[test]
    fn selection_after_closing_should_keep_its_place_in_the_list() {
        // Nothing closed.
        assert_eq!(selection_after_closing(1, &[true, true, true]), Some(1));
        // A window before the selection closes: the selection moves up one.
        assert_eq!(selection_after_closing(2, &[true, false, true]), Some(1));
        // A window after it closes: nothing moves.
        assert_eq!(selection_after_closing(0, &[true, true, false]), Some(0));
        // The selected window closes: the next one takes its place.
        assert_eq!(selection_after_closing(1, &[true, false, true]), Some(1));
        // The selected window was the last: the new last one.
        assert_eq!(selection_after_closing(2, &[true, true, false]), Some(1));
        // Everything closed.
        assert_eq!(selection_after_closing(0, &[false, false]), None);
    }
}

#[cfg(test)]
mod activation_tests {
    use super::group_is_foreground;
    use windows::{
        Win32::{
            Foundation::HWND,
            UI::WindowsAndMessaging::{
                CreateWindowExW, DestroyWindow, IsIconic, IsWindowVisible, SW_HIDE,
                SW_SHOWMINNOACTIVE, SW_SHOWNOACTIVATE, ShowWindow, WS_EX_TOOLWINDOW,
                WS_OVERLAPPEDWINDOW,
            },
        },
        core::w,
    };

    struct Window(HWND);
    impl Window {
        fn new() -> windows::core::Result<Self> {
            // An off-screen test-owned tool window, never the user's apps.
            unsafe {
                CreateWindowExW(
                    WS_EX_TOOLWINDOW,
                    w!("STATIC"),
                    w!("Muelle activation test"),
                    WS_OVERLAPPEDWINDOW,
                    -32000,
                    -32000,
                    160,
                    120,
                    None,
                    None,
                    None,
                    None,
                )
                .map(Self)
            }
        }
        fn raw(&self) -> isize {
            self.0.0 as isize
        }
    }
    impl Drop for Window {
        fn drop(&mut self) {
            unsafe {
                let _ = DestroyWindow(self.0);
            }
        }
    }

    #[test]
    #[ignore = "Requires Windows window management; modifies test-owned windows only"]
    fn a_minimized_foreground_window_must_restore_on_the_first_dock_click()
    -> windows::core::Result<()> {
        let target = Window::new()?;
        unsafe {
            let _ = ShowWindow(target.0, SW_SHOWMINNOACTIVE);
        }
        assert!(unsafe { IsIconic(target.0) }.as_bool());
        // Minimized windows retain WS_VISIBLE. Checking visibility or a
        // potentially stale tracker snapshot alone cannot settle this case.
        assert!(unsafe { IsWindowVisible(target.0) }.as_bool());
        assert!(
            !group_is_foreground(&[target.raw()], target.raw()),
            "a minimized foreground handle must not select Minimize again"
        );
        let peer = Window::new()?;
        unsafe {
            let _ = ShowWindow(peer.0, SW_SHOWNOACTIVATE);
        }
        assert!(!group_is_foreground(
            &[target.raw(), peer.raw()],
            target.raw()
        ));
        Ok(())
    }

    #[test]
    #[ignore = "Requires Windows window management; modifies test-owned windows only"]
    fn only_a_visible_restored_foreground_member_selects_minimize() -> windows::core::Result<()> {
        let target = Window::new()?;
        let other = Window::new()?;
        unsafe {
            let _ = ShowWindow(target.0, SW_SHOWNOACTIVATE);
        }
        assert!(group_is_foreground(&[target.raw()], target.raw()));
        assert!(!group_is_foreground(&[target.raw()], other.raw()));
        assert!(!group_is_foreground(&[], target.raw()));
        unsafe {
            let _ = ShowWindow(target.0, SW_HIDE);
        }
        assert!(!group_is_foreground(&[target.raw()], target.raw()));
        Ok(())
    }
}

#[cfg(test)]
mod group_drop_tests {
    use super::{Rect, group_drop_contains};

    #[test]
    fn grouping_requires_the_pointer_over_the_icon_not_another_monitor_below_it() {
        let icon = Rect {
            x: 80.0,
            y: -24.0,
            width: 72.0,
            height: 72.0,
        };
        assert!(group_drop_contains(&icon, 116.0, 12.0));
        for y in [-25.0, 48.0, 200.0] {
            assert!(
                !group_drop_contains(&icon, 116.0, y),
                "outside the icon at y={y}"
            );
        }
    }

    #[test]
    fn the_sides_of_an_icon_remain_available_for_reordering() {
        let icon = Rect {
            x: 80.0,
            y: -24.0,
            width: 72.0,
            height: 72.0,
        };
        for x in [79.0, 81.0, 151.0, 153.0] {
            assert!(!group_drop_contains(&icon, x, 12.0));
        }
    }
}

#[cfg(test)]
mod preview_order_tests {
    use super::{PreviewOrder, TrackedWindow};

    fn window(hwnd: isize, app: &str, minimized: bool) -> TrackedWindow {
        TrackedWindow {
            hwnd,
            app_id: app.into(),
            minimized,
        }
    }

    #[test]
    fn activation_and_minimizing_do_not_move_preview_cards() {
        let mut order = PreviewOrder::default();
        order.update(&[
            window(3, "browser", false),
            window(2, "browser", false),
            window(1, "browser", false),
        ]);
        let current = [
            window(1, "browser", false),
            window(3, "browser", true),
            window(2, "browser", false),
        ];
        order.update(&current);
        assert_eq!(order.windows_of(&current, "browser"), vec![1, 2, 3]);
        assert_eq!(
            current[0].hwnd, 1,
            "activation order must remain independent"
        );
    }

    #[test]
    fn newly_opened_windows_append_and_closed_windows_leave_no_stale_positions() {
        let mut order = PreviewOrder::default();
        order.update(&[window(2, "browser", false), window(1, "browser", false)]);
        let current = [window(3, "browser", false), window(1, "browser", false)];
        order.update(&current);
        assert_eq!(order.windows_of(&current, "browser"), vec![1, 3]);
        let current = [
            window(2, "browser", false),
            window(3, "browser", false),
            window(1, "browser", false),
        ];
        order.update(&current);
        assert_eq!(order.windows_of(&current, "browser"), vec![1, 3, 2]);
    }

    #[test]
    fn app_groups_keep_their_order_when_other_apps_change() {
        let mut order = PreviewOrder::default();
        order.update(&[
            window(3, "editor", false),
            window(2, "browser", false),
            window(1, "browser", false),
        ]);
        let current = [
            window(4, "editor", false),
            window(2, "browser", false),
            window(1, "browser", false),
        ];
        order.update(&current);
        assert_eq!(order.windows_of(&current, "browser"), vec![1, 2]);
        assert_eq!(order.windows_of(&current, "editor"), vec![4]);
        order.update(&[]);
        assert!(order.0.is_empty());
    }
}
