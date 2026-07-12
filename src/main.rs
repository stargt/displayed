use std::collections::BTreeSet;
use std::env;
use std::ffi::CString;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    execute, queue,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{self, Clear, ClearType},
};

type AppResult<T> = Result<T, Box<dyn std::error::Error>>;
const STATE_FILE_NAME: &str = "state";
const TARGETS_FILE_NAME: &str = "targets";
const EVENT_LOG_FILE_NAME: &str = "displayed.log";
const GUARD_LABEL: &str = "com.stargt.displayed.guard";
const DEFAULT_WATCH_INTERVAL_SECS: u64 = 60;
const DEFAULT_INTERACTIVE_RECONCILE_INTERVAL_SECS: u64 = 5;
const DEFAULT_GUARD_INTERVAL_SECS: u64 = 30;
const INTERNAL_RESTORE_ATTEMPTS: u32 = 3;
const INTERNAL_RESTORE_RETRY_DELAY: Duration = Duration::from_secs(1);
const INTERNAL_RESTORE_VERIFY_TIMEOUT: Duration = Duration::from_secs(2);
type CGDirectDisplayID = u32;
// The built-in panel's contextual id on this machine (Apple Silicon assigns it 1). Used
// as a last-resort enable target when the disabled built-in has dropped out of the
// online display list entirely, so neither the live lookup nor the remembered id can
// name it — empirically the only thing that brings the panel back in that state.
const FALLBACK_BUILTIN_DISPLAY_ID: CGDirectDisplayID = 1;
type CGDisplayChangeSummaryFlags = u32;
type CGDisplayConfigRef = *mut std::ffi::c_void;
type CGError = i32;
type CFRunLoopRef = *mut std::ffi::c_void;
type CFRunLoopSourceRef = *mut std::ffi::c_void;
type CFStringRef = *const std::ffi::c_void;
type IoConnect = u32;
type IoObject = u32;
type IoService = u32;
type IoNotificationPortRef = *mut std::ffi::c_void;
type IoReturn = i32;
type Natural = u32;
const K_CG_CONFIGURE_PERMANENTLY: i32 = 1;
const K_CG_DISPLAY_REMOVE_FLAG: CGDisplayChangeSummaryFlags = 1 << 5;
const K_CG_DISPLAY_DISABLED_FLAG: CGDisplayChangeSummaryFlags = 1 << 9;
const K_IO_MESSAGE_CAN_SYSTEM_SLEEP: Natural = 0xe0000270;
const K_IO_MESSAGE_SYSTEM_WILL_SLEEP: Natural = 0xe0000280;
const K_IO_MESSAGE_SYSTEM_WILL_POWER_ON: Natural = 0xe0000320;
const K_IO_MESSAGE_SYSTEM_HAS_POWERED_ON: Natural = 0xe0000300;
static POWER_ROOT_PORT: AtomicU32 = AtomicU32::new(0);
static POWER_DRY_RUN: AtomicBool = AtomicBool::new(false);

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGBeginDisplayConfiguration(config: *mut CGDisplayConfigRef) -> CGError;
    fn CGCompleteDisplayConfiguration(config: CGDisplayConfigRef, option: i32) -> CGError;
    fn CGCancelDisplayConfiguration(config: CGDisplayConfigRef) -> CGError;
    fn CGSConfigureDisplayEnabled(
        config: CGDisplayConfigRef,
        display: CGDirectDisplayID,
        enabled: bool,
    ) -> CGError;
    fn CGDisplayRegisterReconfigurationCallback(
        callback: CGDisplayReconfigurationCallBack,
        user_info: *mut std::ffi::c_void,
    ) -> CGError;
    fn CGDisplayRemoveReconfigurationCallback(
        callback: CGDisplayReconfigurationCallBack,
        user_info: *mut std::ffi::c_void,
    ) -> CGError;
    fn CGDisplayIsBuiltin(display: CGDirectDisplayID) -> i32;
    fn CGDisplayIsActive(display: CGDirectDisplayID) -> i32;
    fn CGGetOnlineDisplayList(
        max_displays: u32,
        online_displays: *mut CGDirectDisplayID,
        display_count: *mut u32,
    ) -> CGError;
}

type CGDisplayReconfigurationCallBack =
    unsafe extern "C" fn(CGDirectDisplayID, CGDisplayChangeSummaryFlags, *mut std::ffi::c_void);

type IoServiceInterestCallback =
    unsafe extern "C" fn(*mut std::ffi::c_void, IoService, Natural, *mut std::ffi::c_void);

unsafe extern "C" {
    fn dlopen(path: *const std::ffi::c_char, mode: i32) -> *mut std::ffi::c_void;
    fn dlsym(
        handle: *mut std::ffi::c_void,
        symbol: *const std::ffi::c_char,
    ) -> *mut std::ffi::c_void;
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IORegisterForSystemPower(
        refcon: *mut std::ffi::c_void,
        the_port_ref: *mut IoNotificationPortRef,
        callback: IoServiceInterestCallback,
        notifier: *mut IoObject,
    ) -> IoConnect;
    fn IOAllowPowerChange(kernel_port: IoConnect, notification_id: isize) -> IoReturn;
    fn IODeregisterForSystemPower(notifier: *mut IoObject) -> IoReturn;
    fn IONotificationPortGetRunLoopSource(notify: IoNotificationPortRef) -> CFRunLoopSourceRef;
    fn IONotificationPortDestroy(notify: IoNotificationPortRef);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CFRunLoopRun();
    static kCFRunLoopDefaultMode: CFStringRef;
}

const RTLD_LAZY: i32 = 0x1;

#[derive(Debug, Clone, Default)]
struct Display {
    persistent_id: Option<String>,
    contextual_id: Option<String>,
    serial_id: Option<String>,
    display_type: Option<String>,
    resolution: Option<String>,
    hertz: Option<String>,
    color_depth: Option<String>,
    scaling: Option<String>,
    origin: Option<String>,
    rotation: Option<String>,
    enabled: Option<bool>,
    is_main: bool,
    remembered_absent: bool,
}

impl Display {
    fn is_internal(&self) -> bool {
        self.display_type
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .contains("built in")
    }

    fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }

    fn stable_id(&self) -> Option<&str> {
        self.persistent_id.as_deref()
    }

    fn matches(&self, needle: &str) -> bool {
        let normalized = normalize_id(needle);
        [
            self.persistent_id.as_deref(),
            self.contextual_id.as_deref(),
            self.serial_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|id| normalize_id(id) == normalized)
    }

    fn brief(&self) -> String {
        format!(
            "{} {} {} enabled:{}{}",
            self.persistent_id
                .as_deref()
                .unwrap_or("<no-persistent-id>"),
            self.serial_id.as_deref().unwrap_or("<no-serial-id>"),
            self.display_type.as_deref().unwrap_or("<unknown-type>"),
            self.enabled
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            if self.is_main { " main" } else { "" }
        )
    }

    fn label(&self) -> String {
        let display_type = self.display_type.as_deref().unwrap_or("unknown display");
        let resolution = self.resolution.as_deref().unwrap_or("unknown resolution");
        let serial = self.serial_id.as_deref().unwrap_or("no serial");
        format!("{display_type} {resolution} {serial}")
    }

    fn contextual_display_id(&self) -> Option<CGDirectDisplayID> {
        self.contextual_id.as_deref()?.parse().ok()
    }

    fn has_stable_identity(&self) -> bool {
        has_stable_display_identity(self.persistent_id.as_deref(), self.serial_id.as_deref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MonitorTarget {
    contextual_id: Option<String>,
    serial_id: Option<String>,
    persistent_id: Option<String>,
    display_type: Option<String>,
    resolution: Option<String>,
    label: String,
}

impl MonitorTarget {
    fn from_display(display: &Display) -> Option<Self> {
        if display.is_internal() {
            return None;
        }
        if !display.has_stable_identity() {
            return None;
        }

        Some(Self {
            contextual_id: display.contextual_id.clone(),
            serial_id: display.serial_id.clone(),
            persistent_id: display.persistent_id.clone(),
            display_type: display.display_type.clone(),
            resolution: display.resolution.clone(),
            label: display.label(),
        })
    }

    fn matches_display(&self, display: &Display) -> bool {
        if display.is_internal() {
            return false;
        }

        if same_stable_display_identity(
            self.persistent_id.as_deref(),
            self.serial_id.as_deref(),
            display.persistent_id.as_deref(),
            display.serial_id.as_deref(),
        ) {
            return true;
        }

        if self.has_stable_identity() || display.has_stable_identity() {
            return false;
        }

        same_contextual_display_identity(
            self.contextual_id.as_deref(),
            display.contextual_id.as_deref(),
        )
    }

    fn same_identity(&self, other: &MonitorTarget) -> bool {
        if same_stable_display_identity(
            self.persistent_id.as_deref(),
            self.serial_id.as_deref(),
            other.persistent_id.as_deref(),
            other.serial_id.as_deref(),
        ) {
            return true;
        }

        if self.has_stable_identity() || other.has_stable_identity() {
            return false;
        }

        same_contextual_display_identity(
            self.contextual_id.as_deref(),
            other.contextual_id.as_deref(),
        )
    }

    fn summary(&self) -> String {
        format!(
            "{} ctx:{} serial:{} id:{}",
            self.label,
            self.contextual_id.as_deref().unwrap_or("-"),
            self.serial_id.as_deref().unwrap_or("-"),
            self.persistent_id.as_deref().unwrap_or("-")
        )
    }

    fn to_absent_display(&self) -> Display {
        Display {
            contextual_id: self.contextual_id.clone(),
            serial_id: self.serial_id.clone(),
            persistent_id: self.persistent_id.clone(),
            display_type: self
                .display_type
                .clone()
                .or_else(|| Some("saved external display".to_string())),
            resolution: self.resolution.clone(),
            enabled: Some(false),
            remembered_absent: true,
            ..Display::default()
        }
    }

    fn has_stable_identity(&self) -> bool {
        has_stable_display_identity(self.persistent_id.as_deref(), self.serial_id.as_deref())
    }
}

#[derive(Debug, Clone)]
struct ParsedDisplayplacer {
    displays: Vec<Display>,
    restore_command: Option<String>,
    raw: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionMode {
    DryRun,
    Execute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchEvent {
    DisplayChanged(DisplayChange),
    Reconcile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PowerEvent {
    SystemWillPowerOn,
    SystemHasPoweredOn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClamshellState {
    Open,
    Closed,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuardAction {
    None,
    RestoreInternal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DisplayChange {
    display: CGDirectDisplayID,
    flags: CGDisplayChangeSummaryFlags,
}

impl DisplayChange {
    fn is_display_loss(self) -> bool {
        self.flags & (K_CG_DISPLAY_REMOVE_FLAG | K_CG_DISPLAY_DISABLED_FLAG) != 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoWatchAction {
    None,
    DisabledInternal,
    RestoredInternal,
}

struct DisplayEventRegistration {
    sender: *mut mpsc::Sender<WatchEvent>,
    registered: bool,
}

impl Drop for DisplayEventRegistration {
    fn drop(&mut self) {
        unsafe {
            if self.registered {
                let _ = CGDisplayRemoveReconfigurationCallback(
                    display_reconfiguration_callback,
                    self.sender.cast(),
                );
            }
            if !self.sender.is_null() {
                drop(Box::from_raw(self.sender));
            }
        }
    }
}

struct PowerEventRegistration {
    sender: *mut mpsc::Sender<PowerEvent>,
    notification_port: IoNotificationPortRef,
    notifier: IoObject,
    root_port: IoConnect,
    registered: bool,
}

impl Drop for PowerEventRegistration {
    fn drop(&mut self) {
        unsafe {
            if self.registered {
                let _ = IODeregisterForSystemPower(&mut self.notifier);
            }
            if !self.notification_port.is_null() {
                IONotificationPortDestroy(self.notification_port);
            }
            if !self.sender.is_null() {
                drop(Box::from_raw(self.sender));
            }
        }
        if self.root_port != 0 {
            POWER_ROOT_PORT.store(0, Ordering::SeqCst);
        }
    }
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> AppResult<Self> {
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), cursor::Show, terminal::LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> AppResult<()> {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        interactive_mode()?;
        return Ok(());
    };

    match command.as_str() {
        "-h" | "--help" | "help" => print_usage(),
        "list" => list_displays()?,
        "snapshot" => snapshot(args.collect())?,
        "disable-internal" => disable_internal(args.collect())?,
        "enable-internal" => enable_internal(args.collect())?,
        "restore-internal" => restore_internal(args.collect())?,
        "restore-display" => restore_display(args.collect())?,
        "redetect-displays" => redetect_displays(args.collect())?,
        "safe-reset" | "reset-safe" => safe_reset(args.collect())?,
        "targets" => targets_command(args.collect())?,
        "guard" => guard(args.collect())?,
        "install-guard" => install_guard(args.collect())?,
        "uninstall-guard" => uninstall_guard(args.collect())?,
        "watch" => watch(args.collect())?,
        unknown => {
            return Err(format!("unknown command `{unknown}`. Run `displayed --help`.").into());
        }
    }

    Ok(())
}

fn print_usage() {
    println!(
        "\
displayed

Commands:
  displayed
  displayed list
  displayed snapshot [--label LABEL] [--out DIR]
  displayed disable-internal [--when-id ID | --when-serial SERIAL] [--dry-run] [--force]
  displayed enable-internal [--id ID] [--dry-run]
  displayed restore-internal [--display-id ID] [--dry-run] [--displayplacer-fallback]
  displayed restore-display --display-id ID [--dry-run]
  displayed redetect-displays [--dry-run]
  displayed safe-reset [--dry-run]
  displayed targets [--clear]
  displayed guard [--reason REASON] [--dry-run] [--quiet]
  displayed install-guard [--interval SECONDS] [--no-load] [--dry-run]
  displayed uninstall-guard [--no-unload] [--dry-run]
  displayed watch [--target-id ID | --target-serial SERIAL] [--interval SECONDS] [--dry-run] [--force]

Notes:
  - Running without arguments opens interactive mode.
  - `displayed watch` without a target uses monitors saved in interactive mode.
  - `displayed guard` is restore-only; it never disables the internal display.
  - `displayed install-guard` installs a one-shot launchd safety guard for crash/stuck recovery.
  - Display IDs come from the contextual ID shown by `displayed list`.
  - SERIAL may be either `12345` or `s12345`.
  - `--force` allows disabling even when only one enabled display is reported.
"
    );
}

fn list_displays() -> AppResult<()> {
    let parsed = load_displayplacer()?;
    remember_internal(&parsed);
    print_parsed_displays(&parsed);

    if let Some(command) = parsed.restore_command {
        println!();
        println!("restore command:");
        println!("{command}");
    }

    Ok(())
}

fn interactive_mode() -> AppResult<()> {
    let _terminal = TerminalGuard::enter()?;
    let (watch_rx, _display_events, event_status) =
        start_display_event_source(DEFAULT_INTERACTIVE_RECONCILE_INTERVAL_SECS);
    POWER_DRY_RUN.store(false, Ordering::SeqCst);
    let (power_rx, _power_events, power_status) = match start_power_event_source() {
        Ok((rx, registration)) => (rx, Some(registration), None),
        Err(error) => {
            let (_tx, rx) = mpsc::channel();
            (
                rx,
                None,
                Some(format!("power event registration failed: {error}")),
            )
        }
    };
    let mut targets = load_targets()?;
    let mut selected = 0usize;
    let mut show_help = false;
    let mut status = event_status
        .or(power_status)
        .unwrap_or_else(|| "ready".to_string());
    let mut parsed = load_display_state_for_ui(&mut selected, &mut status);
    let mut width = terminal::size()
        .map(|(columns, _)| columns as usize)
        .unwrap_or(100);

    if !targets.is_empty() {
        match apply_interactive_display_recovery(&targets, ActionMode::Execute, false, false) {
            Ok(AutoWatchAction::DisabledInternal) => {
                status = "auto target matched; internal disabled".to_string();
                parsed = load_display_state_for_ui(&mut selected, &mut status);
            }
            Ok(AutoWatchAction::RestoredInternal) => {
                status = "internal restore requested".to_string();
                parsed = load_display_state_for_ui(&mut selected, &mut status);
            }
            Ok(AutoWatchAction::None) => {}
            Err(error) => status = format!("auto watch failed: {error}"),
        }
    }

    render_interactive(parsed.as_ref(), &targets, selected, &status, width)?;

    loop {
        let mut should_refresh = false;
        let mut needs_render = false;

        if event::poll(Duration::from_millis(200))? {
            match event::read()? {
                Event::Key(key) => {
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        break;
                    }

                    if show_help {
                        // Any key dismisses the help screen without triggering its action.
                        show_help = false;
                        needs_render = true;
                    } else {
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => break,
                            KeyCode::Char('?') => {
                                show_help = true;
                                needs_render = true;
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                selected = selected.saturating_sub(1);
                                needs_render = true;
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                if let Some(parsed) = parsed.as_ref() {
                                    if selected + 1 < parsed.displays.len() {
                                        selected += 1;
                                        needs_render = true;
                                    }
                                }
                            }
                            KeyCode::Char('r') => {
                                should_refresh = true;
                                status = "refreshed".to_string();
                                needs_render = true;
                            }
                            KeyCode::Char('s') => {
                                match safe_reset_once_with_verbosity(ActionMode::Execute, false) {
                                    Ok(()) => status = "safe reset completed".to_string(),
                                    Err(error) => status = format!("safe reset failed: {error}"),
                                }
                                should_refresh = true;
                                needs_render = true;
                            }
                            KeyCode::Char('i') => {
                                status = match toggle_internal_for_ui() {
                                    Ok(message) => message,
                                    Err(error) => format!("internal toggle failed: {error}"),
                                };
                                should_refresh = true;
                                needs_render = true;
                            }
                            KeyCode::Enter | KeyCode::Char(' ') => {
                                status = match parsed
                                    .as_ref()
                                    .and_then(|parsed| parsed.displays.get(selected))
                                {
                                    Some(display) => match toggle_display_for_ui(display) {
                                        Ok(message) => message,
                                        Err(error) => format!("display toggle failed: {error}"),
                                    },
                                    None => "no selected display".to_string(),
                                };
                                should_refresh = true;
                                needs_render = true;
                            }
                            KeyCode::Char('a') => {
                                status = match parsed
                                    .as_ref()
                                    .and_then(|parsed| parsed.displays.get(selected))
                                {
                                    Some(display) => match toggle_target_for_display(display) {
                                        Ok(true) => {
                                            "auto target saved for selected external".to_string()
                                        }
                                        Ok(false) => {
                                            "auto target removed for selected external".to_string()
                                        }
                                        Err(error) => format!("auto target update failed: {error}"),
                                    },
                                    None => "no selected display".to_string(),
                                };
                                targets = load_targets()?;
                                match apply_interactive_display_recovery(
                                    &targets,
                                    ActionMode::Execute,
                                    false,
                                    false,
                                ) {
                                    Ok(AutoWatchAction::DisabledInternal) => {
                                        status =
                                            "auto target matched; internal disabled".to_string()
                                    }
                                    Ok(AutoWatchAction::RestoredInternal) => {
                                        status = "internal restore requested".to_string()
                                    }
                                    Ok(AutoWatchAction::None) => {}
                                    Err(error) => status = format!("auto watch failed: {error}"),
                                }
                                should_refresh = true;
                                needs_render = true;
                            }
                            _ => {}
                        }
                    }
                }
                Event::Resize(columns, _) => {
                    width = columns as usize;
                    needs_render = true;
                }
                _ => {}
            }
        }

        while let Ok(event) = watch_rx.try_recv() {
            match event {
                WatchEvent::DisplayChanged(change) => {
                    log_event(&format!(
                        "display-change: display={} flags={:#x} loss={}",
                        change.display,
                        change.flags,
                        change.is_display_loss()
                    ));
                    should_refresh = true;
                    match apply_interactive_display_recovery_for_change(
                        &targets,
                        change,
                        ActionMode::Execute,
                        false,
                        false,
                    ) {
                        Ok(AutoWatchAction::DisabledInternal) => {
                            status = "auto target matched; internal disabled".to_string()
                        }
                        Ok(AutoWatchAction::RestoredInternal) => {
                            status = "internal restore requested".to_string()
                        }
                        Ok(AutoWatchAction::None) => status = "display change detected".to_string(),
                        Err(error) => status = format!("display recovery failed: {error}"),
                    }
                    needs_render = true;
                }
                WatchEvent::Reconcile => {
                    should_refresh = true;
                    match apply_interactive_display_recovery(
                        &targets,
                        ActionMode::Execute,
                        false,
                        false,
                    ) {
                        Ok(AutoWatchAction::DisabledInternal) => {
                            status = "auto target reconciled; internal disabled".to_string()
                        }
                        Ok(AutoWatchAction::RestoredInternal) => {
                            status = "internal restore requested".to_string()
                        }
                        Ok(AutoWatchAction::None) => status = "periodic reconcile".to_string(),
                        Err(error) => status = format!("display recovery failed: {error}"),
                    }
                    needs_render = true;
                }
            }
        }

        while let Ok(event) = power_rx.try_recv() {
            let reason = match event {
                PowerEvent::SystemWillPowerOn => "system-will-power-on",
                PowerEvent::SystemHasPoweredOn => "system-has-powered-on",
            };
            log_event(&format!("power: {reason} — running internal guard"));
            should_refresh = true;
            match run_internal_guard_once(reason, ActionMode::Execute, false) {
                Ok(()) => status = format!("power event reconciled: {reason}"),
                Err(error) => status = format!("power event recovery failed: {error}"),
            }
            needs_render = true;
        }

        if should_refresh {
            parsed = load_display_state_for_ui(&mut selected, &mut status);
            needs_render = true;
        }

        if needs_render {
            if show_help {
                render_help()?;
            } else {
                render_interactive(parsed.as_ref(), &targets, selected, &status, width)?;
            }
        }
    }

    Ok(())
}

fn render_help() -> AppResult<()> {
    let mut out = io::stdout();
    queue!(out, cursor::MoveTo(0, 0), Clear(ClearType::All))?;

    queue!(
        out,
        SetAttribute(Attribute::Bold),
        Print("displayed — help"),
        SetAttribute(Attribute::Reset),
        Print("\r\n\r\n")
    )?;

    for (key, description) in [
        ("up/down, k/j", "Move the selection between displays."),
        (
            "space, enter",
            "Toggle the selected display: disable it if it is on, enable it if it is off.",
        ),
        (
            "i",
            "Toggle the built-in (internal) display: disable it when it is on; when it is off or missing, enable it (native restore with redetection if needed).",
        ),
        (
            "a",
            "Save or remove the selected external display as an auto target. While an auto target is connected, the built-in display is kept disabled automatically; when it disconnects, the built-in is restored.",
        ),
        (
            "s",
            "Safe reset: redetect displays and re-enable every remembered or detected display. Recovery hammer for when the screen setup looks wrong.",
        ),
        ("r", "Refresh the display list."),
        ("?", "Show this help."),
        ("q, esc", "Quit."),
    ] {
        queue!(
            out,
            SetAttribute(Attribute::Bold),
            Print(format!("  {key:<14}")),
            SetAttribute(Attribute::Reset),
            Print(description),
            Print("\r\n")
        )?;
    }

    queue!(
        out,
        Print("\r\n"),
        SetAttribute(Attribute::Dim),
        Print("press any key to close this help"),
        SetAttribute(Attribute::Reset)
    )?;
    out.flush()?;
    Ok(())
}

fn load_display_state_for_ui(
    selected: &mut usize,
    status: &mut String,
) -> Option<ParsedDisplayplacer> {
    match load_displayplacer() {
        Ok(parsed) => {
            remember_internal(&parsed);
            let mut parsed = parsed;
            match merge_remembered_displays(parsed.displays.clone()) {
                Ok(displays) => parsed.displays = displays,
                Err(error) => *status = format!("failed to merge remembered displays: {error}"),
            }

            if parsed.displays.is_empty() {
                *selected = 0;
            } else if *selected >= parsed.displays.len() {
                *selected = parsed.displays.len() - 1;
            }
            Some(parsed)
        }
        Err(error) => {
            *status = format!("failed to read display state: {error}");
            None
        }
    }
}

fn render_interactive(
    parsed: Option<&ParsedDisplayplacer>,
    targets: &[MonitorTarget],
    selected: usize,
    status: &str,
    width: usize,
) -> AppResult<()> {
    let mut out = io::stdout();
    queue!(out, cursor::MoveTo(0, 0), Clear(ClearType::All))?;

    queue!(
        out,
        SetAttribute(Attribute::Bold),
        Print("displayed"),
        SetAttribute(Attribute::Reset)
    )?;
    queue!(
        out,
        Print("  auto:"),
        Print(if targets.is_empty() {
            "inactive"
        } else {
            "active"
        }),
        Print("  targets:"),
        Print(targets.len().to_string()),
        Print("\r\n\r\n")
    )?;

    match parsed {
        Some(parsed) if !parsed.displays.is_empty() => {
            for (index, display) in parsed.displays.iter().enumerate() {
                let is_selected = index == selected;
                if is_selected {
                    queue!(out, SetAttribute(Attribute::Reverse))?;
                }

                render_display_row(&mut out, index, display, targets, width)?;

                if is_selected {
                    queue!(out, SetAttribute(Attribute::Reset))?;
                }
                queue!(out, Print("\r\n"))?;

                if is_selected {
                    queue!(out, SetAttribute(Attribute::Dim))?;
                    queue!(
                        out,
                        Print("     id:"),
                        Print(display.persistent_id.as_deref().unwrap_or("-")),
                        Print("  ctx:"),
                        Print(display.contextual_id.as_deref().unwrap_or("-")),
                        Print("  serial:"),
                        Print(display.serial_id.as_deref().unwrap_or("-")),
                        Print("  origin:"),
                        Print(display.origin.as_deref().unwrap_or("-")),
                        Print(if display.remembered_absent {
                            "  remembered"
                        } else {
                            ""
                        }),
                        Print("\r\n")
                    )?;
                    queue!(out, SetAttribute(Attribute::Reset))?;
                }
            }
        }
        Some(_) => {
            queue!(
                out,
                Print("No displays parsed from displayplacer output.\r\n")
            )?;
        }
        None => {
            queue!(out, Print("Display state unavailable.\r\n"))?;
        }
    }

    if parsed
        .map(|parsed| !parsed.displays.iter().any(Display::is_internal))
        .unwrap_or(true)
    {
        if let Some(summary) = remembered_internal_summary()? {
            queue!(
                out,
                Print("\r\n"),
                SetAttribute(Attribute::Dim),
                Print(summary),
                SetAttribute(Attribute::Reset),
                Print("\r\n")
            )?;
        }
    }

    if !targets.is_empty() {
        queue!(out, Print("\r\nSaved auto targets:\r\n"))?;
        for target in targets {
            queue!(out, Print("  - "), Print(target.summary()), Print("\r\n"))?;
        }
    }

    queue!(out, Print("\r\n"))?;
    queue!(out, SetAttribute(Attribute::Dim))?;
    queue!(
        out,
        Print(
            "[up/down] select  [space] toggle  [i] internal  [a] auto target  [s] safe reset  [r] refresh  [?] help  [q] quit\r\n"
        )
    )?;
    queue!(
        out,
        Print("status: "),
        Print(status),
        SetAttribute(Attribute::Reset)
    )?;
    out.flush()?;
    Ok(())
}

fn render_display_row(
    out: &mut impl Write,
    index: usize,
    display: &Display,
    targets: &[MonitorTarget],
    width: usize,
) -> AppResult<()> {
    let role = if display.is_internal() {
        "internal"
    } else {
        "external"
    };
    let state = match display.enabled {
        _ if display.remembered_absent => "off",
        Some(true) => "on",
        Some(false) => "off",
        None => "unknown",
    };
    let auto = if targets.iter().any(|target| target.matches_display(display)) {
        "auto"
    } else {
        "-"
    };
    let main = if display.is_main { " main" } else { "" };
    let color = if display.remembered_absent {
        Color::DarkGrey
    } else if display.is_internal() {
        Color::Blue
    } else if display.is_enabled() {
        Color::Green
    } else {
        Color::DarkGrey
    };
    let label = trim_to_width(
        &format!(
            "{} {}{}",
            display.display_type.as_deref().unwrap_or("<unknown-type>"),
            display
                .resolution
                .as_deref()
                .unwrap_or("<unknown-resolution>"),
            main
        ),
        width.saturating_sub(28),
    );

    queue!(
        out,
        Print(format!("{index:>2} ")),
        SetForegroundColor(color),
        Print(format!("{role:<8}")),
        ResetColor,
        Print(" "),
        Print(format!("{state:<7}")),
        Print(" "),
        Print(format!("{auto:<5}")),
        Print(" "),
        Print(label)
    )?;
    Ok(())
}

fn remembered_internal_summary() -> AppResult<Option<String>> {
    let persistent_id = read_state_value("internal_id")?;
    let serial = read_state_value("internal_serial")?;
    let display_id = read_state_value("internal_display_id")?;

    if persistent_id.is_none() && serial.is_none() && display_id.is_none() {
        return Ok(None);
    }

    Ok(Some(format!(
        "remembered internal id:{} display_id:{} serial:{}",
        persistent_id.as_deref().unwrap_or("-"),
        display_id.as_deref().unwrap_or("-"),
        serial.as_deref().unwrap_or("-")
    )))
}

fn toggle_internal_for_ui() -> AppResult<String> {
    let parsed = load_displayplacer()?;
    remember_internal(&parsed);

    if let Some(internal) = parsed.displays.iter().find(|display| display.is_internal()) {
        if internal.is_enabled() {
            disable_internal_once_with_verbosity(None, ActionMode::Execute, false, false, false)?;
            Ok("internal disabled".to_string())
        } else if let Some(id) = internal.stable_id() {
            set_displayplacer_enabled(id, true, ActionMode::Execute, false)?;
            Ok("internal enabled".to_string())
        } else {
            restore_internal_once(
                None,
                ActionMode::Execute,
                false,
                false,
                INTERNAL_RESTORE_ATTEMPTS,
            )?;
            Ok("internal restore requested".to_string())
        }
    } else {
        restore_internal_once(
            None,
            ActionMode::Execute,
            false,
            false,
            INTERNAL_RESTORE_ATTEMPTS,
        )?;
        Ok("internal restore requested".to_string())
    }
}

fn toggle_display_for_ui(display: &Display) -> AppResult<String> {
    let enabled = display
        .enabled
        .ok_or_else(|| "selected display has unknown enabled state".to_string())?;

    if !enabled || display.remembered_absent {
        let display_id = display.contextual_display_id().ok_or_else(|| {
            "selected display has no remembered contextual display id".to_string()
        })?;
        restore_display_id(display_id, ActionMode::Execute, false)?;
        remember_contextual_display_id(display_id)?;
        return Ok(format!(
            "{} restore requested",
            if display.is_internal() {
                "internal"
            } else {
                "external"
            }
        ));
    }

    if enabled {
        let parsed = load_displayplacer()?;
        let enabled_count = parsed
            .displays
            .iter()
            .filter(|display| display.is_enabled())
            .count();
        if enabled_count <= 1 {
            return Err("refusing to disable the only enabled display".into());
        }
    }

    if let Some(display_id) = display.contextual_display_id() {
        remember_contextual_display_id(display_id)?;
    }
    remember_display_metadata(display)?;

    let id = display
        .stable_id()
        .ok_or_else(|| "selected display has no persistent id".to_string())?
        .to_string();
    set_displayplacer_enabled(&id, !enabled, ActionMode::Execute, false)?;
    Ok(format!(
        "{} {}",
        if display.is_internal() {
            "internal"
        } else {
            "display"
        },
        if enabled { "disabled" } else { "enabled" }
    ))
}

fn snapshot(args: Vec<String>) -> AppResult<()> {
    let mut label = "state".to_string();
    let mut out_dir = PathBuf::from("snapshots");
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--label" => {
                i += 1;
                label = args
                    .get(i)
                    .ok_or("missing value after --label")?
                    .to_string();
            }
            "--out" => {
                i += 1;
                out_dir = PathBuf::from(args.get(i).ok_or("missing value after --out")?);
            }
            other => return Err(format!("unknown snapshot option `{other}`").into()),
        }
        i += 1;
    }

    fs::create_dir_all(&out_dir)?;
    let timestamp = unix_timestamp()?;
    let file_name = format!("{timestamp}-{}.txt", slug(&label));
    let path = out_dir.join(file_name);
    let mut body = String::new();

    body.push_str("# displayed snapshot\n");
    body.push_str(&format!("label: {label}\n"));
    body.push_str(&format!("unix_timestamp: {timestamp}\n\n"));

    match load_displayplacer() {
        Ok(parsed) => {
            remember_internal(&parsed);
            body.push_str("## parsed displayplacer summary\n");
            if parsed.displays.is_empty() {
                body.push_str("No displays parsed from displayplacer output.\n");
            } else {
                for display in &parsed.displays {
                    body.push_str(&format!("- {}\n", display.brief()));
                }
            }
            if let Some(command) = parsed.restore_command {
                body.push_str("\nrestore command:\n");
                body.push_str(&command);
                body.push('\n');
            }
            body.push('\n');
            body.push_str("## displayplacer raw\n");
            body.push_str(&parsed.raw);
            body.push('\n');
        }
        Err(error) => {
            body.push_str("## displayplacer raw\n");
            body.push_str(&format!("failed: {error}\n"));
        }
    }

    append_betterdisplay_preferences(&mut body);

    let probes: &[(&str, &[&str])] = &[
        ("system_profiler", &["SPDisplaysDataType"]),
        ("pmset", &["-g", "assertions"]),
        ("ioreg", &["-lw0", "-r", "-c", "AppleBacklightDisplay"]),
        ("ioreg", &["-lw0", "-r", "-c", "IODisplayConnect"]),
        ("ioreg", &["-lw0", "-r", "-c", "IODisplay"]),
        ("ioreg", &["-lw0", "-r", "-c", "IOFramebuffer"]),
    ];

    for (program, arguments) in probes {
        append_command_capture(&mut body, program, arguments);
    }

    fs::write(&path, body)?;
    println!("{}", path.display());
    Ok(())
}

fn disable_internal(args: Vec<String>) -> AppResult<()> {
    let mut condition: Option<String> = None;
    let mut mode = ActionMode::Execute;
    let mut force = false;
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--when-id" | "--when-serial" => {
                i += 1;
                condition = Some(
                    args.get(i)
                        .ok_or("missing value after condition option")?
                        .to_string(),
                );
            }
            "--dry-run" => mode = ActionMode::DryRun,
            "--force" => force = true,
            other => return Err(format!("unknown disable-internal option `{other}`").into()),
        }
        i += 1;
    }

    disable_internal_once(condition.as_deref(), mode, force, true)?;
    Ok(())
}

fn enable_internal(args: Vec<String>) -> AppResult<()> {
    let mut id: Option<String> = None;
    let mut mode = ActionMode::Execute;
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--id" => {
                i += 1;
                id = Some(args.get(i).ok_or("missing value after --id")?.to_string());
            }
            "--dry-run" => mode = ActionMode::DryRun,
            other => return Err(format!("unknown enable-internal option `{other}`").into()),
        }
        i += 1;
    }

    let id = match id {
        Some(id) => id,
        None => read_state_value("internal_id")?.ok_or_else(|| {
            "no remembered internal id in displayed state; pass --id <built-in-persistent-id>"
                .to_string()
        })?,
    };

    remember_internal_id(&id, None, None);
    run_displayplacer_enabled(&id, true, mode)
}

fn restore_internal(args: Vec<String>) -> AppResult<()> {
    let mut display_id: Option<CGDirectDisplayID> = None;
    let mut mode = ActionMode::Execute;
    let mut displayplacer_fallback = false;
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--display-id" => {
                i += 1;
                display_id = Some(
                    args.get(i)
                        .ok_or("missing value after --display-id")?
                        .parse()?,
                );
            }
            "--dry-run" => mode = ActionMode::DryRun,
            "--displayplacer-fallback" => displayplacer_fallback = true,
            other => return Err(format!("unknown restore-internal option `{other}`").into()),
        }
        i += 1;
    }

    restore_internal_once(
        display_id,
        mode,
        displayplacer_fallback,
        true,
        INTERNAL_RESTORE_ATTEMPTS,
    )
}

fn restore_display(args: Vec<String>) -> AppResult<()> {
    let mut display_id: Option<CGDirectDisplayID> = None;
    let mut mode = ActionMode::Execute;
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--display-id" => {
                i += 1;
                display_id = Some(
                    args.get(i)
                        .ok_or("missing value after --display-id")?
                        .parse()?,
                );
            }
            "--dry-run" => mode = ActionMode::DryRun,
            other => return Err(format!("unknown restore-display option `{other}`").into()),
        }
        i += 1;
    }

    let display_id = display_id.ok_or("restore-display requires --display-id <contextual-id>")?;
    restore_display_id(display_id, mode, true)?;
    if mode == ActionMode::Execute {
        remember_contextual_display_id(display_id)?;
    }
    Ok(())
}

fn restore_internal_once(
    display_id: Option<CGDirectDisplayID>,
    mode: ActionMode,
    displayplacer_fallback: bool,
    verbose: bool,
    attempts: u32,
) -> AppResult<()> {
    detect_displays(mode, verbose)?;
    enable_internal_with_retry(display_id, attempts, mode, verbose)?;

    if displayplacer_fallback {
        if let Some(id) = read_state_value("internal_id")? {
            set_displayplacer_enabled(&id, true, mode, verbose)?;
        } else if verbose {
            eprintln!(
                "warning: no remembered internal id in displayed state; skipped displayplacer fallback"
            );
        }
    }

    Ok(())
}

// Candidate ids to enable when bringing the built-in panel back, in trust order. An
// explicit caller id is authoritative and tried alone. Otherwise: the live online-list
// lookup (correct whenever the disabled built-in is still enumerable), the remembered
// contextual id, and finally FALLBACK_BUILTIN_DISPLAY_ID for the observed worst case
// where the panel is offline and every recorded id has gone stale.
fn internal_restore_candidates(
    explicit: Option<CGDirectDisplayID>,
    live_builtin: Option<CGDirectDisplayID>,
    remembered: Option<CGDirectDisplayID>,
) -> Vec<CGDirectDisplayID> {
    if let Some(explicit) = explicit {
        return vec![explicit];
    }

    let mut candidates = Vec::new();
    for candidate in [live_builtin, remembered, Some(FALLBACK_BUILTIN_DISPLAY_ID)]
        .into_iter()
        .flatten()
    {
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }
    candidates
}

// Natively enable the built-in panel and verify it actually came up, retrying across
// the wake window. Two failure modes make one shot unreliable: ids are reassigned
// across deep sleep (a stale id errors with 1001, or worse succeeds as a silent no-op),
// and right after wake macOS holds the display configuration so even a correct id gets
// a transient 1001. Re-resolving candidates and verifying with CGDisplayIsActive on
// every attempt covers both.
fn enable_internal_with_retry(
    explicit: Option<CGDirectDisplayID>,
    attempts: u32,
    mode: ActionMode,
    verbose: bool,
) -> AppResult<()> {
    if mode == ActionMode::DryRun {
        let remembered = remembered_internal_display_id()?;
        let candidates = internal_restore_candidates(explicit, None, remembered);
        return set_native_display_enabled(candidates[0], true, mode, verbose);
    }

    let mut last_error: Option<String> = None;
    for attempt in 1..=attempts.max(1) {
        if attempt > 1 {
            thread::sleep(INTERNAL_RESTORE_RETRY_DELAY);
            let _ = detect_displays(mode, false);
        }

        if builtin_display_active() {
            log_event(&format!(
                "restore-internal: builtin already active (attempt {attempt}/{attempts})"
            ));
            return Ok(());
        }

        let live_builtin = current_builtin_display_id();
        let remembered = remembered_internal_display_id()?;
        let candidates = internal_restore_candidates(explicit, live_builtin, remembered);
        log_event(&format!(
            "restore-internal: attempt {attempt}/{attempts} candidates {candidates:?} (live={live_builtin:?} remembered={remembered:?})"
        ));

        for display_id in candidates {
            match set_native_display_enabled(display_id, true, mode, verbose) {
                Ok(()) => {
                    if wait_for_builtin_active(INTERNAL_RESTORE_VERIFY_TIMEOUT) {
                        log_event(&format!(
                            "restore-internal: builtin active after enabling id {display_id}"
                        ));
                        remember_contextual_display_id(display_id)?;
                        return Ok(());
                    }
                    log_event(&format!(
                        "restore-internal: id {display_id} enabled ok but builtin not active"
                    ));
                    last_error = Some(format!(
                        "enable of display id {display_id} reported ok but built-in did not become active"
                    ));
                }
                Err(error) => last_error = Some(error.to_string()),
            }
        }
    }

    Err(last_error
        .unwrap_or_else(|| "internal restore did not enable the built-in display".to_string())
        .into())
}

fn restore_display_id(
    display_id: CGDirectDisplayID,
    mode: ActionMode,
    verbose: bool,
) -> AppResult<()> {
    detect_displays(mode, verbose)?;
    set_native_display_enabled(display_id, true, mode, verbose)
}

fn redetect_displays(args: Vec<String>) -> AppResult<()> {
    let mut mode = ActionMode::Execute;
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--dry-run" => mode = ActionMode::DryRun,
            other => return Err(format!("unknown redetect-displays option `{other}`").into()),
        }
        i += 1;
    }

    run_sls_detect_displays(mode)
}

fn targets_command(args: Vec<String>) -> AppResult<()> {
    let mut clear = false;
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--clear" => clear = true,
            other => return Err(format!("unknown targets option `{other}`").into()),
        }
        i += 1;
    }

    if clear {
        save_targets(&[])?;
        println!("cleared saved auto targets");
        return Ok(());
    }

    let targets = load_targets()?;
    if targets.is_empty() {
        println!("no saved auto targets");
    } else {
        for (index, target) in targets.iter().enumerate() {
            println!("[{index}] {}", target.summary());
        }
    }

    Ok(())
}

fn safe_reset(args: Vec<String>) -> AppResult<()> {
    let mut mode = ActionMode::Execute;
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--dry-run" => mode = ActionMode::DryRun,
            other => return Err(format!("unknown safe-reset option `{other}`").into()),
        }
        i += 1;
    }

    safe_reset_once(mode)
}

fn safe_reset_once(mode: ActionMode) -> AppResult<()> {
    safe_reset_once_with_verbosity(mode, true)
}

fn safe_reset_once_with_verbosity(mode: ActionMode, verbose: bool) -> AppResult<()> {
    if verbose {
        println!("safe reset: redetect displays");
    }
    if let Err(error) = detect_displays(mode, verbose) {
        if verbose {
            eprintln!("warning: display redetection failed: {error}");
        }
    }

    let remembered_display_ids = remembered_display_ids()?;
    if remembered_display_ids.is_empty() {
        if verbose {
            eprintln!("warning: no remembered display ids; skipped native restore");
        }
    } else {
        for display_id in remembered_display_ids {
            if verbose {
                println!("safe reset: enable remembered display id {display_id}");
            }
            if let Err(error) = set_native_display_enabled(display_id, true, mode, verbose) {
                if verbose {
                    eprintln!(
                        "warning: native restore failed for display id {display_id}: {error}"
                    );
                }
            }
        }
    }

    if mode == ActionMode::Execute {
        thread::sleep(Duration::from_millis(500));
    }

    let parsed = match load_displayplacer() {
        Ok(parsed) => parsed,
        Err(error) => {
            if verbose {
                eprintln!("warning: could not reload displayplacer list after redetect: {error}");
            }
            return Ok(());
        }
    };
    remember_internal(&parsed);

    let mut enabled_any = false;
    for display in parsed
        .displays
        .iter()
        .filter(|display| !display.is_enabled())
    {
        let Some(id) = display.stable_id() else {
            if verbose {
                eprintln!("warning: skipped disabled display without persistent id");
            }
            continue;
        };
        if verbose {
            println!("safe reset: enable detected display {id}");
        }
        if let Err(error) = set_displayplacer_enabled(id, true, mode, verbose) {
            if verbose {
                eprintln!("warning: failed to enable display {id}: {error}");
            }
        } else {
            enabled_any = true;
        }
    }

    if !enabled_any && verbose {
        println!("safe reset: no detected disabled displays to enable");
    }

    Ok(())
}

fn restore_internal_safety(mode: ActionMode, verbose: bool) -> AppResult<()> {
    // With the lid closed the panel cannot light up: every enable fails (err 1014) and
    // during closed-lid sleep this path used to spam failing CGS transactions for hours,
    // colliding with macOS's own reconfiguration. The guard restores once the lid opens.
    if read_clamshell_state(verbose) == ClamshellState::Closed {
        log_event("internal safety restore: skipped (clamshell closed)");
        return Ok(());
    }

    if verbose {
        println!("internal safety restore: redetect displays");
    }
    if let Err(error) = detect_displays(mode, verbose) {
        if verbose {
            eprintln!("warning: display redetection failed: {error}");
        }
    }

    // Only the built-in panel is restored here. Enabling every remembered contextual id
    // (as this once did) can silently power an external instead, because macOS reuses
    // those ids across monitors and deep sleeps.
    enable_internal_with_retry(None, INTERNAL_RESTORE_ATTEMPTS, mode, verbose)
}

fn guard(args: Vec<String>) -> AppResult<()> {
    let mut mode = ActionMode::Execute;
    let mut reason = "manual".to_string();
    let mut verbose = true;
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--dry-run" => mode = ActionMode::DryRun,
            "--quiet" => verbose = false,
            "--reason" => {
                i += 1;
                reason = args
                    .get(i)
                    .ok_or("missing value after --reason")?
                    .to_string();
            }
            other => return Err(format!("unknown guard option `{other}`").into()),
        }
        i += 1;
    }

    run_internal_guard_once(&reason, mode, verbose)
}

fn run_internal_guard_once(reason: &str, mode: ActionMode, verbose: bool) -> AppResult<()> {
    if verbose {
        println!("guard: reason={reason}");
    }

    // The restore path (restore_internal_once) runs SLSDetectDisplays itself, so the
    // idle guard stays read-only instead of re-detecting on every run. An
    // unconditional redetect here resurfaced the intentionally-disabled built-in every
    // guard interval, which the watcher then re-disabled, causing the built-in to flicker.
    let clamshell = read_clamshell_state(verbose);
    let parsed = load_displayplacer_with_timeout(Duration::from_secs(2));
    let action = match parsed.as_ref() {
        Ok(parsed) => {
            if mode == ActionMode::Execute {
                remember_internal(parsed);
            }
            internal_guard_action_for_state(clamshell, Some(parsed))
        }
        Err(error) => {
            if verbose {
                eprintln!("warning: guard display state unavailable: {error}");
            }
            internal_guard_action_for_state(clamshell, None)
        }
    };

    let (internal_enabled, external_enabled) = match parsed.as_ref() {
        Ok(parsed) => (
            parsed
                .displays
                .iter()
                .any(|display| display.is_internal() && display.is_enabled()),
            parsed
                .displays
                .iter()
                .any(|display| !display.is_internal() && display.is_enabled()),
        ),
        Err(_) => (false, false),
    };
    // Skip the boring "internal already on, nothing to do" heartbeat (runs every 30s);
    // log only the cases that matter for diagnosing a stuck-off built-in.
    if action == GuardAction::RestoreInternal || parsed.is_err() || !internal_enabled {
        log_event(&format!(
            "guard: reason={reason} clamshell={clamshell:?} state={} internal_enabled={internal_enabled} external_enabled={external_enabled} action={action:?}",
            if parsed.is_ok() { "ok" } else { "unavailable" }
        ));
    }

    match action {
        GuardAction::None => {
            if verbose {
                println!("guard: no internal restore needed");
            }
            Ok(())
        }
        GuardAction::RestoreInternal => {
            if verbose {
                println!("guard: restoring internal display");
            }
            restore_internal_only_safety(mode, verbose, INTERNAL_RESTORE_ATTEMPTS)
        }
    }
}

fn restore_internal_only_safety(mode: ActionMode, verbose: bool, attempts: u32) -> AppResult<()> {
    if let Err(error) = restore_internal_once(None, mode, false, verbose, attempts) {
        if verbose {
            eprintln!("warning: native internal restore failed: {error}");
        }
    }

    let parsed = match load_displayplacer_with_timeout(Duration::from_secs(2)) {
        Ok(parsed) => parsed,
        Err(error) => {
            if verbose {
                eprintln!("warning: could not verify internal restore: {error}");
            }
            return Ok(());
        }
    };
    if mode == ActionMode::Execute {
        remember_internal(&parsed);
    }

    if parsed
        .displays
        .iter()
        .any(|display| display.is_internal() && display.is_enabled())
    {
        return Ok(());
    }

    for display in parsed
        .displays
        .iter()
        .filter(|display| display.is_internal() && !display.is_enabled())
    {
        let Some(id) = display.stable_id() else {
            continue;
        };
        if let Err(error) = set_displayplacer_enabled(id, true, mode, verbose) {
            if verbose {
                eprintln!("warning: displayplacer internal restore failed: {error}");
            }
        }
        return Ok(());
    }

    Ok(())
}

fn internal_guard_action_for_state(
    clamshell: ClamshellState,
    parsed: Option<&ParsedDisplayplacer>,
) -> GuardAction {
    if clamshell == ClamshellState::Closed {
        return GuardAction::None;
    }

    // Fail open: when display state is unavailable and the lid is open, restore the
    // built-in so a dead watcher plus an unplugged external cannot leave the Mac with no
    // active display. The flicker this once caused came from the guard being blind to a
    // present external (displayplacer missing from launchd's PATH), now fixed by baking
    // PATH into the LaunchAgent, so failing open is safe again.
    let Some(parsed) = parsed else {
        return GuardAction::RestoreInternal;
    };

    let internal_enabled = parsed
        .displays
        .iter()
        .any(|display| display.is_internal() && display.is_enabled());
    if internal_enabled {
        return GuardAction::None;
    }

    let external_enabled = parsed
        .displays
        .iter()
        .any(|display| !display.is_internal() && display.is_enabled());
    if external_enabled {
        return GuardAction::None;
    }

    GuardAction::RestoreInternal
}

fn read_clamshell_state(verbose: bool) -> ClamshellState {
    match run_command_with_timeout(
        "ioreg",
        &["-r", "-k", "AppleClamshellState", "-d", "4"],
        Duration::from_secs(1),
    ) {
        Ok(output) => {
            let body = String::from_utf8_lossy(&output.stdout);
            if body.contains("\"AppleClamshellState\" = Yes") {
                ClamshellState::Closed
            } else if body.contains("\"AppleClamshellState\" = No") {
                ClamshellState::Open
            } else {
                ClamshellState::Unknown
            }
        }
        Err(error) => {
            if verbose {
                eprintln!("warning: clamshell state unavailable: {error}");
            }
            ClamshellState::Unknown
        }
    }
}

fn start_power_event_source() -> AppResult<(Receiver<PowerEvent>, PowerEventRegistration)> {
    let (tx, rx) = mpsc::channel();
    let sender = Box::into_raw(Box::new(tx));
    let mut notification_port: IoNotificationPortRef = std::ptr::null_mut();
    let mut notifier: IoObject = 0;
    let root_port = unsafe {
        IORegisterForSystemPower(
            sender.cast(),
            &mut notification_port,
            power_event_callback,
            &mut notifier,
        )
    };

    if root_port == 0 || notification_port.is_null() {
        unsafe {
            drop(Box::from_raw(sender));
        }
        return Err("IORegisterForSystemPower failed".into());
    }

    POWER_ROOT_PORT.store(root_port, Ordering::SeqCst);
    let source = unsafe { IONotificationPortGetRunLoopSource(notification_port) };
    let source_addr = source as usize;
    thread::spawn(move || unsafe {
        let run_loop = CFRunLoopGetCurrent();
        CFRunLoopAddSource(
            run_loop,
            source_addr as CFRunLoopSourceRef,
            kCFRunLoopDefaultMode,
        );
        CFRunLoopRun();
    });

    Ok((
        rx,
        PowerEventRegistration {
            sender,
            notification_port,
            notifier,
            root_port,
            registered: true,
        },
    ))
}

unsafe extern "C" fn power_event_callback(
    refcon: *mut std::ffi::c_void,
    _service: IoService,
    message_type: Natural,
    message_argument: *mut std::ffi::c_void,
) {
    let root_port = POWER_ROOT_PORT.load(Ordering::SeqCst);
    if message_type == K_IO_MESSAGE_CAN_SYSTEM_SLEEP {
        if root_port != 0 {
            let _ = unsafe { IOAllowPowerChange(root_port, message_argument as isize) };
        }
        return;
    }

    if message_type == K_IO_MESSAGE_SYSTEM_WILL_SLEEP {
        let mode = if POWER_DRY_RUN.load(Ordering::SeqCst) {
            ActionMode::DryRun
        } else {
            ActionMode::Execute
        };
        log_event("power: SystemWillSleep — restoring internal before sleep");
        // Single attempt only: this callback must hand the power change back promptly
        // (IOAllowPowerChange below), so it must not sit in the retry/verify loop and
        // delay sleep. The wake-side guard is the robust recovery path.
        let _ = restore_internal_only_safety(mode, false, 1);
        if root_port != 0 {
            let _ = unsafe { IOAllowPowerChange(root_port, message_argument as isize) };
        }
        return;
    }

    if refcon.is_null() {
        return;
    }
    let sender = unsafe { &*(refcon as *const mpsc::Sender<PowerEvent>) };
    match message_type {
        K_IO_MESSAGE_SYSTEM_WILL_POWER_ON => {
            let _ = sender.send(PowerEvent::SystemWillPowerOn);
        }
        K_IO_MESSAGE_SYSTEM_HAS_POWERED_ON => {
            let _ = sender.send(PowerEvent::SystemHasPoweredOn);
        }
        _ => {}
    }
}

fn install_guard(args: Vec<String>) -> AppResult<()> {
    let mut interval = DEFAULT_GUARD_INTERVAL_SECS;
    let mut load = true;
    let mut mode = ActionMode::Execute;
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--interval" => {
                i += 1;
                interval = args
                    .get(i)
                    .ok_or("missing value after --interval")?
                    .parse::<u64>()?
                    .max(5);
            }
            "--no-load" => load = false,
            "--dry-run" => mode = ActionMode::DryRun,
            other => return Err(format!("unknown install-guard option `{other}`").into()),
        }
        i += 1;
    }

    let exe = env::current_exe()?.canonicalize()?;
    let launch_agents = launch_agents_dir()?;
    let logs_dir = logs_dir()?;
    let guard_plist = launch_agents.join(format!("{GUARD_LABEL}.plist"));

    let guard_body = launch_agent_plist(
        GUARD_LABEL,
        &[
            exe.to_string_lossy().as_ref(),
            "guard",
            "--reason",
            "launchd",
            "--quiet",
        ],
        Some(interval),
        false,
        &logs_dir.join("displayed-guard.log"),
        &logs_dir.join("displayed-guard.err.log"),
        &guard_environment_path(),
    );

    if mode == ActionMode::DryRun {
        println!("dry-run: write {}", guard_plist.display());
        println!("{guard_body}");
    } else {
        fs::create_dir_all(&launch_agents)?;
        fs::create_dir_all(&logs_dir)?;
        fs::write(&guard_plist, guard_body)?;
        println!("installed {}", guard_plist.display());
    }

    if load {
        load_launch_agent(GUARD_LABEL, &guard_plist, mode)?;
    }

    Ok(())
}

fn uninstall_guard(args: Vec<String>) -> AppResult<()> {
    let mut unload = true;
    let mut mode = ActionMode::Execute;
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--no-unload" => unload = false,
            "--dry-run" => mode = ActionMode::DryRun,
            other => return Err(format!("unknown uninstall-guard option `{other}`").into()),
        }
        i += 1;
    }

    let launch_agents = launch_agents_dir()?;
    let guard_plist = launch_agents.join(format!("{GUARD_LABEL}.plist"));

    if unload {
        unload_launch_agent(GUARD_LABEL, mode)?;
    }

    if mode == ActionMode::DryRun {
        println!("dry-run: remove {}", guard_plist.display());
    } else if guard_plist.exists() {
        fs::remove_file(&guard_plist)?;
        println!("removed {}", guard_plist.display());
    }

    Ok(())
}

fn guard_environment_path() -> String {
    // The guard shells out to `displayplacer` (typically under Homebrew). launchd runs
    // agents with a minimal PATH that omits /opt/homebrew/bin, so bake a usable PATH into
    // the plist: Homebrew locations, the install-time PATH, then standard system dirs.
    let mut dirs: Vec<String> = vec![
        "/opt/homebrew/bin".to_string(),
        "/usr/local/bin".to_string(),
    ];
    if let Ok(existing) = env::var("PATH") {
        for dir in existing.split(':').filter(|dir| !dir.is_empty()) {
            if !dirs.iter().any(|known| known == dir) {
                dirs.push(dir.to_string());
            }
        }
    }
    for fallback in ["/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
        if !dirs.iter().any(|known| known == fallback) {
            dirs.push(fallback.to_string());
        }
    }
    dirs.join(":")
}

fn launch_agent_plist(
    label: &str,
    arguments: &[&str],
    start_interval: Option<u64>,
    keep_alive: bool,
    stdout_path: &std::path::Path,
    stderr_path: &std::path::Path,
    path_env: &str,
) -> String {
    let mut body = String::new();
    body.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    body.push_str("<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n");
    body.push_str("<plist version=\"1.0\">\n<dict>\n");
    body.push_str("  <key>Label</key>\n");
    body.push_str(&format!("  <string>{}</string>\n", xml_escape(label)));
    body.push_str("  <key>ProgramArguments</key>\n  <array>\n");
    for argument in arguments {
        body.push_str(&format!("    <string>{}</string>\n", xml_escape(argument)));
    }
    body.push_str("  </array>\n");
    if !path_env.is_empty() {
        body.push_str("  <key>EnvironmentVariables</key>\n  <dict>\n");
        body.push_str("    <key>PATH</key>\n");
        body.push_str(&format!("    <string>{}</string>\n", xml_escape(path_env)));
        body.push_str("  </dict>\n");
    }
    body.push_str("  <key>RunAtLoad</key>\n  <true/>\n");
    if keep_alive {
        body.push_str("  <key>KeepAlive</key>\n  <true/>\n");
    }
    if let Some(interval) = start_interval {
        body.push_str("  <key>StartInterval</key>\n");
        body.push_str(&format!("  <integer>{interval}</integer>\n"));
    }
    body.push_str("  <key>LimitLoadToSessionType</key>\n  <string>Aqua</string>\n");
    body.push_str("  <key>StandardOutPath</key>\n");
    body.push_str(&format!(
        "  <string>{}</string>\n",
        xml_escape(&stdout_path.to_string_lossy())
    ));
    body.push_str("  <key>StandardErrorPath</key>\n");
    body.push_str(&format!(
        "  <string>{}</string>\n",
        xml_escape(&stderr_path.to_string_lossy())
    ));
    body.push_str("</dict>\n</plist>\n");
    body
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn launch_agents_dir() -> AppResult<PathBuf> {
    Ok(home_dir()?.join("Library").join("LaunchAgents"))
}

fn logs_dir() -> AppResult<PathBuf> {
    Ok(home_dir()?.join("Library").join("Logs"))
}

fn home_dir() -> AppResult<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".into())
}

// Append a timestamped line to the persistent event log. Best-effort and silent on
// failure: the interactive TUI runs in raw mode and the launchd guard runs with
// --quiet, so neither can use stdout/stderr. A file is the only durable trace of what
// the watcher/guard saw and did across a sleep/wake when nobody is watching the screen.
fn log_event(message: &str) {
    let Ok(dir) = logs_dir() else {
        return;
    };
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let stamp = unix_timestamp().unwrap_or(0);
    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(EVENT_LOG_FILE_NAME))
    {
        let _ = file.write_all(format!("{stamp} {message}\n").as_bytes());
    }
}

fn load_launch_agent(label: &str, plist: &std::path::Path, mode: ActionMode) -> AppResult<()> {
    if mode == ActionMode::DryRun {
        println!("dry-run: launchctl bootstrap gui/<uid> {}", plist.display());
        println!("dry-run: launchctl kickstart -k gui/<uid>/{label}");
        return Ok(());
    }

    unload_launch_agent(label, mode)?;
    let domain = launchd_gui_domain()?;
    run_launchctl(&["bootstrap", &domain, &plist.to_string_lossy()])?;
    run_launchctl(&["kickstart", "-k", &format!("{domain}/{label}")])?;
    println!("loaded {domain}/{label}");
    Ok(())
}

fn unload_launch_agent(label: &str, mode: ActionMode) -> AppResult<()> {
    if mode == ActionMode::DryRun {
        println!("dry-run: launchctl bootout gui/<uid>/{label}");
        return Ok(());
    }

    let domain = launchd_gui_domain()?;
    let service = format!("{domain}/{label}");
    let _ = Command::new("launchctl")
        .args(["bootout", &service])
        .output();
    Ok(())
}

fn launchd_gui_domain() -> AppResult<String> {
    let output = run_command_with_timeout("id", &["-u"], Duration::from_secs(1))?;
    if !output.status.success() {
        return Err("id -u failed".into());
    }
    let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(format!("gui/{uid}"))
}

fn run_launchctl(arguments: &[&str]) -> AppResult<()> {
    let output = Command::new("launchctl").args(arguments).output()?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "launchctl {} failed with status {}:\n{}{}",
        arguments.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .into())
}

fn load_targets() -> AppResult<Vec<MonitorTarget>> {
    let Some(raw) = read_support_file(TARGETS_FILE_NAME)? else {
        return Ok(Vec::new());
    };

    let mut targets = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(target) = parse_target_line(line) {
            targets.push(target);
        }
    }
    Ok(targets)
}

fn save_targets(targets: &[MonitorTarget]) -> AppResult<()> {
    if targets.is_empty() {
        remove_support_file(TARGETS_FILE_NAME)?;
        return Ok(());
    }

    let mut body = String::new();
    for target in targets {
        body.push_str(&target_to_line(target));
        body.push('\n');
    }
    write_support_file(TARGETS_FILE_NAME, &body)?;
    Ok(())
}

fn parse_target_line(line: &str) -> Option<MonitorTarget> {
    let mut target = MonitorTarget {
        contextual_id: None,
        serial_id: None,
        persistent_id: None,
        display_type: None,
        resolution: None,
        label: "saved display".to_string(),
    };

    for field in line.split('\t') {
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        let value = decode_field(value);
        match key {
            "contextual" => target.contextual_id = non_empty(value),
            "serial" => target.serial_id = non_empty(value),
            "persistent" => target.persistent_id = non_empty(value),
            "type" => target.display_type = non_empty(value),
            "resolution" => target.resolution = non_empty(value),
            "label" => {
                if !value.is_empty() {
                    target.label = value;
                }
            }
            _ => {}
        }
    }

    if !target.has_stable_identity() {
        None
    } else {
        Some(target)
    }
}

fn target_to_line(target: &MonitorTarget) -> String {
    let mut fields = Vec::new();
    push_target_field(&mut fields, "contextual", target.contextual_id.as_deref());
    push_target_field(&mut fields, "serial", target.serial_id.as_deref());
    push_target_field(&mut fields, "persistent", target.persistent_id.as_deref());
    push_target_field(&mut fields, "type", target.display_type.as_deref());
    push_target_field(&mut fields, "resolution", target.resolution.as_deref());
    push_target_field(&mut fields, "label", Some(&target.label));
    fields.join("\t")
}

fn push_target_field(fields: &mut Vec<String>, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        fields.push(format!("{key}={}", encode_field(value)));
    }
}

fn encode_field(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
}

fn decode_field(value: &str) -> String {
    let mut decoded = String::new();
    let mut escaped = false;
    for ch in value.chars() {
        if escaped {
            match ch {
                't' => decoded.push('\t'),
                'n' => decoded.push('\n'),
                '\\' => decoded.push('\\'),
                other => {
                    decoded.push('\\');
                    decoded.push(other);
                }
            }
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else {
            decoded.push(ch);
        }
    }
    if escaped {
        decoded.push('\\');
    }
    decoded
}

fn non_empty(value: String) -> Option<String> {
    if value.is_empty() { None } else { Some(value) }
}

fn toggle_target_for_display(display: &Display) -> AppResult<bool> {
    let target = MonitorTarget::from_display(display)
        .ok_or("selected display cannot be used as an auto target")?;
    let mut targets = load_targets()?;

    if let Some(index) = targets
        .iter()
        .position(|candidate| candidate.same_identity(&target))
    {
        targets.remove(index);
        save_targets(&targets)?;
        Ok(false)
    } else {
        targets.push(target);
        save_targets(&targets)?;
        remember_display_metadata(display)?;
        Ok(true)
    }
}

fn apply_interactive_display_recovery(
    targets: &[MonitorTarget],
    mode: ActionMode,
    force: bool,
    verbose: bool,
) -> AppResult<AutoWatchAction> {
    if !targets.is_empty() {
        match apply_watch_targets(targets, mode, force, verbose)? {
            AutoWatchAction::None => {}
            action => return Ok(action),
        }
    }

    apply_no_enabled_display_safety(mode, verbose)
}

fn apply_interactive_display_recovery_for_change(
    targets: &[MonitorTarget],
    change: DisplayChange,
    mode: ActionMode,
    force: bool,
    verbose: bool,
) -> AppResult<AutoWatchAction> {
    match display_loss_restore_action_for_change(
        change,
        remembered_internal_display_id()?,
        display_is_builtin(change.display),
    ) {
        AutoWatchAction::RestoredInternal => {
            restore_internal_safety(mode, verbose)?;
            return Ok(AutoWatchAction::RestoredInternal);
        }
        AutoWatchAction::None if change.is_display_loss() => return Ok(AutoWatchAction::None),
        AutoWatchAction::None => {}
        AutoWatchAction::DisabledInternal => {}
    }

    apply_interactive_display_recovery(targets, mode, force, verbose)
}

fn apply_no_enabled_display_safety(mode: ActionMode, verbose: bool) -> AppResult<AutoWatchAction> {
    let Some(parsed) = load_displayplacer_for_auto_watch(mode, verbose)? else {
        return Ok(AutoWatchAction::RestoredInternal);
    };
    remember_internal(&parsed);

    match no_enabled_display_safety_action_for_state(&parsed) {
        AutoWatchAction::RestoredInternal => {
            restore_internal_safety(mode, verbose)?;
            Ok(AutoWatchAction::RestoredInternal)
        }
        AutoWatchAction::None => Ok(AutoWatchAction::None),
        AutoWatchAction::DisabledInternal => Ok(AutoWatchAction::None),
    }
}

fn apply_watch_targets(
    targets: &[MonitorTarget],
    mode: ActionMode,
    force: bool,
    verbose: bool,
) -> AppResult<AutoWatchAction> {
    if targets.is_empty() {
        return Ok(AutoWatchAction::None);
    }

    let Some(parsed) = load_displayplacer_for_auto_watch(mode, verbose)? else {
        return Ok(AutoWatchAction::RestoredInternal);
    };
    remember_internal(&parsed);

    let target_present = parsed.displays.iter().any(|display| {
        display.is_enabled() && targets.iter().any(|target| target.matches_display(display))
    });
    apply_auto_watch_action(&parsed, target_present, None, mode, force, verbose)
}

fn apply_raw_watch_target(
    target: &str,
    mode: ActionMode,
    force: bool,
    verbose: bool,
) -> AppResult<AutoWatchAction> {
    let Some(parsed) = load_displayplacer_for_auto_watch(mode, verbose)? else {
        return Ok(AutoWatchAction::RestoredInternal);
    };
    remember_internal(&parsed);

    let target_present = parsed
        .displays
        .iter()
        .any(|display| display.matches(target) && display.is_enabled());
    apply_auto_watch_action(&parsed, target_present, Some(target), mode, force, verbose)
}

fn apply_auto_watch_action(
    parsed: &ParsedDisplayplacer,
    target_present: bool,
    raw_condition: Option<&str>,
    mode: ActionMode,
    force: bool,
    verbose: bool,
) -> AppResult<AutoWatchAction> {
    match auto_watch_action_for_state(parsed, target_present) {
        AutoWatchAction::DisabledInternal => {
            disable_internal_once_with_verbosity(raw_condition, mode, force, false, verbose)?;
            Ok(AutoWatchAction::DisabledInternal)
        }
        AutoWatchAction::RestoredInternal => {
            restore_internal_safety(mode, verbose)?;
            Ok(AutoWatchAction::RestoredInternal)
        }
        AutoWatchAction::None => Ok(AutoWatchAction::None),
    }
}

fn load_displayplacer_for_auto_watch(
    mode: ActionMode,
    verbose: bool,
) -> AppResult<Option<ParsedDisplayplacer>> {
    match load_displayplacer() {
        Ok(parsed) => Ok(Some(parsed)),
        Err(error) => {
            if verbose {
                eprintln!(
                    "warning: display state unavailable; requesting internal restore: {error}"
                );
            }
            restore_internal_safety(mode, verbose)?;
            Ok(None)
        }
    }
}

fn display_is_builtin(display: CGDirectDisplayID) -> bool {
    unsafe { CGDisplayIsBuiltin(display) != 0 }
}

fn online_display_ids() -> Vec<CGDirectDisplayID> {
    let mut ids = [0 as CGDirectDisplayID; 32];
    let mut count: u32 = 0;
    let status = unsafe { CGGetOnlineDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
    if status != 0 {
        return Vec::new();
    }
    ids[..count as usize].to_vec()
}

// Resolve the built-in display's *current* CGDirectDisplayID from the live online list.
// macOS reassigns CGDirectDisplayIDs across deep sleep / reconnect, so a remembered
// contextual id can address the wrong display or none at all, which makes the native
// enable a silent no-op and leaves the panel dark. The live id is authoritative.
fn current_builtin_display_id() -> Option<CGDirectDisplayID> {
    online_display_ids()
        .into_iter()
        .find(|&id| display_is_builtin(id))
}

fn builtin_display_active() -> bool {
    current_builtin_display_id()
        .map(|id| unsafe { CGDisplayIsActive(id) != 0 })
        .unwrap_or(false)
}

// The enable commits asynchronously: CGCompleteDisplayConfiguration can return 0 before
// the panel is actually driven (and a stale id "succeeds" without ever driving it), so
// poll the live state briefly instead of trusting the return code.
fn wait_for_builtin_active(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if builtin_display_active() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn display_loss_restore_action_for_change(
    change: DisplayChange,
    internal_display_id: Option<CGDirectDisplayID>,
    change_is_builtin: bool,
) -> AutoWatchAction {
    if !change.is_display_loss() {
        return AutoWatchAction::None;
    }

    // Never treat the built-in's own disable as an external loss. The live
    // CGDisplayIsBuiltin check covers the case where internal_display_id is a stale
    // contextual id from an earlier session that no longer matches change.display.
    if change_is_builtin || internal_display_id == Some(change.display) {
        return AutoWatchAction::None;
    }

    AutoWatchAction::RestoredInternal
}

fn auto_watch_action_for_state(
    parsed: &ParsedDisplayplacer,
    target_present: bool,
) -> AutoWatchAction {
    let internal_enabled = parsed
        .displays
        .iter()
        .any(|display| display.is_internal() && display.is_enabled());

    if target_present {
        if internal_enabled {
            AutoWatchAction::DisabledInternal
        } else {
            AutoWatchAction::None
        }
    } else if internal_enabled {
        AutoWatchAction::None
    } else {
        AutoWatchAction::RestoredInternal
    }
}

fn no_enabled_display_safety_action_for_state(parsed: &ParsedDisplayplacer) -> AutoWatchAction {
    if parsed.displays.iter().any(Display::is_enabled) {
        AutoWatchAction::None
    } else {
        AutoWatchAction::RestoredInternal
    }
}

fn watch(args: Vec<String>) -> AppResult<()> {
    let mut target: Option<String> = None;
    let mut interval = Duration::from_secs(DEFAULT_WATCH_INTERVAL_SECS);
    let mut mode = ActionMode::Execute;
    let mut force = false;
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--target-id" | "--target-serial" => {
                i += 1;
                target = Some(
                    args.get(i)
                        .ok_or("missing value after target option")?
                        .to_string(),
                );
            }
            "--interval" => {
                i += 1;
                let seconds: u64 = args
                    .get(i)
                    .ok_or("missing value after --interval")?
                    .parse()?;
                interval = Duration::from_secs(seconds.max(1));
            }
            "--dry-run" => mode = ActionMode::DryRun,
            "--force" => force = true,
            other => return Err(format!("unknown watch option `{other}`").into()),
        }
        i += 1;
    }

    let saved_targets = if target.is_none() {
        load_targets()?
    } else {
        Vec::new()
    };

    if target.is_none() && saved_targets.is_empty() {
        return Err(
            "watch has no target. Run `displayed`, select an external display, press `a`, then run `displayed watch`."
                .into(),
        );
    }

    let (events, _display_events, event_status) = start_display_event_source(interval.as_secs());
    if let Some(status) = event_status {
        eprintln!("warning: {status}");
    }

    if let Some(target) = target.as_deref() {
        println!(
            "watching target `{target}`; display events plus {}s reconcile; press Ctrl-C to stop",
            interval.as_secs()
        );
        match apply_raw_watch_target(target, mode, force, true) {
            Ok(AutoWatchAction::DisabledInternal) => {
                println!("watch matched target; internal disabled")
            }
            Ok(AutoWatchAction::RestoredInternal) => {
                println!("watch target absent; internal restore requested")
            }
            Ok(AutoWatchAction::None) => {}
            Err(error) => eprintln!("watch iteration failed: {error}"),
        }
    } else {
        println!(
            "watching {} saved target(s); display events plus {}s reconcile; press Ctrl-C to stop",
            saved_targets.len(),
            interval.as_secs()
        );
        for target in &saved_targets {
            println!("  - {}", target.summary());
        }
        match apply_watch_targets(&saved_targets, mode, force, true) {
            Ok(AutoWatchAction::DisabledInternal) => {
                println!("watch matched saved target; internal disabled")
            }
            Ok(AutoWatchAction::RestoredInternal) => {
                println!("watch saved target absent; internal restore requested")
            }
            Ok(AutoWatchAction::None) => {}
            Err(error) => eprintln!("watch iteration failed: {error}"),
        }
    }

    loop {
        let event = events.recv()?;
        match event {
            WatchEvent::DisplayChanged(_) | WatchEvent::Reconcile => {
                if let Some(target) = target.as_deref() {
                    match apply_raw_watch_target(target, mode, force, true) {
                        Ok(AutoWatchAction::DisabledInternal) => {
                            println!("watch matched target; internal disabled")
                        }
                        Ok(AutoWatchAction::RestoredInternal) => {
                            println!("watch target absent; internal restore requested")
                        }
                        Ok(AutoWatchAction::None) => {}
                        Err(error) => eprintln!("watch iteration failed: {error}"),
                    }
                } else {
                    match apply_watch_targets(&saved_targets, mode, force, true) {
                        Ok(AutoWatchAction::DisabledInternal) => {
                            println!("watch matched saved target; internal disabled")
                        }
                        Ok(AutoWatchAction::RestoredInternal) => {
                            println!("watch saved target absent; internal restore requested")
                        }
                        Ok(AutoWatchAction::None) => {}
                        Err(error) => eprintln!("watch iteration failed: {error}"),
                    }
                }
            }
        }
    }
}

fn start_display_event_source(
    reconcile_interval_secs: u64,
) -> (
    Receiver<WatchEvent>,
    DisplayEventRegistration,
    Option<String>,
) {
    let (tx, rx) = mpsc::channel();
    let registration = register_display_reconfiguration_callback(tx.clone());

    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_secs(reconcile_interval_secs.max(1)));
            if tx.send(WatchEvent::Reconcile).is_err() {
                break;
            }
        }
    });

    let status = if registration.registered {
        None
    } else {
        Some(
            "CoreGraphics display callback registration failed; using periodic reconcile only"
                .to_string(),
        )
    };

    (rx, registration, status)
}

fn register_display_reconfiguration_callback(
    tx: mpsc::Sender<WatchEvent>,
) -> DisplayEventRegistration {
    let sender = Box::into_raw(Box::new(tx));
    let status = unsafe {
        CGDisplayRegisterReconfigurationCallback(display_reconfiguration_callback, sender.cast())
    };

    DisplayEventRegistration {
        sender,
        registered: status == 0,
    }
}

unsafe extern "C" fn display_reconfiguration_callback(
    display: CGDirectDisplayID,
    flags: CGDisplayChangeSummaryFlags,
    user_info: *mut std::ffi::c_void,
) {
    if user_info.is_null() {
        return;
    }

    let sender = unsafe { &*(user_info as *const mpsc::Sender<WatchEvent>) };
    let _ = sender.send(WatchEvent::DisplayChanged(DisplayChange { display, flags }));
}

fn disable_internal_once(
    condition: Option<&str>,
    mode: ActionMode,
    force: bool,
    print_skips: bool,
) -> AppResult<()> {
    disable_internal_once_with_verbosity(condition, mode, force, print_skips, true)
}

fn disable_internal_once_with_verbosity(
    condition: Option<&str>,
    mode: ActionMode,
    force: bool,
    print_skips: bool,
    verbose: bool,
) -> AppResult<()> {
    let parsed = load_displayplacer()?;
    remember_internal(&parsed);

    if parsed.displays.is_empty() {
        if print_skips {
            println!("skip: displayplacer reported no online displays");
        }
        return Ok(());
    }

    if let Some(target) = condition {
        let target_present = parsed
            .displays
            .iter()
            .any(|display| display.matches(target) && display.is_enabled());
        if !target_present {
            if print_skips {
                println!("skip: target `{target}` is not present/enabled");
            }
            return Ok(());
        }
    }

    let Some(internal) = parsed
        .displays
        .iter()
        .find(|display| display.is_internal() && display.is_enabled())
    else {
        if print_skips {
            println!("skip: no enabled internal display reported");
        }
        return Ok(());
    };

    let enabled_count = parsed
        .displays
        .iter()
        .filter(|display| display.is_enabled())
        .count();
    if enabled_count <= 1 && !force {
        return Err(
            "refusing to disable the only enabled display; pass --force to override".into(),
        );
    }

    let Some(id) = internal.stable_id() else {
        return Err("internal display has no persistent id in displayplacer output".into());
    };

    set_displayplacer_enabled(id, false, mode, verbose).map(|_| ())
}

fn run_displayplacer_enabled(id: &str, enabled: bool, mode: ActionMode) -> AppResult<()> {
    set_displayplacer_enabled(id, enabled, mode, true).map(|_| ())
}

fn set_displayplacer_enabled(
    id: &str,
    enabled: bool,
    mode: ActionMode,
    verbose: bool,
) -> AppResult<String> {
    let argument = format!("id:{id} enabled:{enabled}");
    if mode == ActionMode::DryRun {
        let message = format!("dry-run: displayplacer \"{argument}\"");
        if verbose {
            println!("{message}");
        }
        return Ok(message);
    }

    if verbose {
        println!("running: displayplacer \"{argument}\"");
    }
    let output = Command::new("displayplacer").arg(&argument).output()?;
    let combined_output = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() {
        return Err(format!(
            "displayplacer failed with status {}:\n{}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }

    if verbose {
        print!("{}", String::from_utf8_lossy(&output.stdout));
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(combined_output)
}

fn run_sls_detect_displays(mode: ActionMode) -> AppResult<()> {
    detect_displays(mode, true)
}

fn detect_displays(mode: ActionMode, verbose: bool) -> AppResult<()> {
    if mode == ActionMode::DryRun {
        if verbose {
            println!("dry-run: SLSDetectDisplays()");
        }
        return Ok(());
    }

    if verbose {
        println!("running: SLSDetectDisplays()");
    }
    unsafe {
        call_sls_detect_displays()?;
    }
    Ok(())
}

unsafe fn call_sls_detect_displays() -> AppResult<()> {
    type SLSDetectDisplaysFn = unsafe extern "C" fn();

    let framework = CString::new("/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight")?;
    let handle = unsafe { dlopen(framework.as_ptr(), RTLD_LAZY) };
    if handle.is_null() {
        return Err("dlopen SkyLight failed".into());
    }

    let symbol = CString::new("SLSDetectDisplays")?;
    let raw = unsafe { dlsym(handle, symbol.as_ptr()) };
    if raw.is_null() {
        return Err("dlsym SLSDetectDisplays failed".into());
    }

    let function: SLSDetectDisplaysFn = unsafe { std::mem::transmute(raw) };
    unsafe {
        function();
    }

    Ok(())
}

fn set_native_display_enabled(
    display_id: CGDirectDisplayID,
    enabled: bool,
    mode: ActionMode,
    verbose: bool,
) -> AppResult<()> {
    if mode == ActionMode::DryRun {
        if verbose {
            println!(
                "dry-run: CGSConfigureDisplayEnabled(display_id:{display_id}, enabled:{enabled})"
            );
        }
        return Ok(());
    }

    if verbose {
        println!("running: CGSConfigureDisplayEnabled(display_id:{display_id}, enabled:{enabled})");
    }
    let mut config: CGDisplayConfigRef = std::ptr::null_mut();
    let begin = unsafe { CGBeginDisplayConfiguration(&mut config) };
    if begin != 0 {
        return Err(format!("CGBeginDisplayConfiguration failed: {begin}").into());
    }

    let configure = unsafe { CGSConfigureDisplayEnabled(config, display_id, enabled) };
    if configure != 0 {
        unsafe {
            CGCancelDisplayConfiguration(config);
        }
        log_event(&format!(
            "cgs-configure: display_id={display_id} enabled={enabled} configure_err={configure}"
        ));
        return Err(format!("CGSConfigureDisplayEnabled failed: {configure}").into());
    }

    let complete = unsafe { CGCompleteDisplayConfiguration(config, K_CG_CONFIGURE_PERMANENTLY) };
    if complete != 0 {
        log_event(&format!(
            "cgs-configure: display_id={display_id} enabled={enabled} complete_err={complete}"
        ));
        return Err(format!("CGCompleteDisplayConfiguration failed: {complete}").into());
    }

    log_event(&format!(
        "cgs-configure: display_id={display_id} enabled={enabled} ok"
    ));
    Ok(())
}

fn remember_internal(parsed: &ParsedDisplayplacer) {
    if let Err(error) = remember_displays(&parsed.displays) {
        eprintln!("warning: failed to write displayed state: {error}");
    }
}

fn remember_displays(displays: &[Display]) -> AppResult<()> {
    let mut remembered = read_remembered_displays()?;
    for display in displays {
        upsert_remembered_display(&mut remembered, display.clone());
    }

    // Rebuild the known-id set from currently remembered displays only. Seeding from the
    // previous file union let stale CGDirectDisplayIDs (which macOS reassigns on every
    // reconnect/reboot) accumulate without bound and bloated the restore set.
    let mut known_display_ids: BTreeSet<CGDirectDisplayID> = BTreeSet::new();
    for display in &remembered {
        if let Some(display_id) = display.contextual_display_id() {
            known_display_ids.insert(display_id);
        }
    }

    let internal = displays
        .iter()
        .find(|display| display.is_internal())
        .or_else(|| remembered.iter().find(|display| display.is_internal()));

    let mut state = String::new();
    if let Some(internal) = internal {
        if let Some(id) = internal.stable_id() {
            state.push_str(&format!("internal_id={id}\n"));
        }
        if let Some(serial) = internal.serial_id.as_deref() {
            state.push_str(&format!("internal_serial={serial}\n"));
        }
        if let Some(display_id) = internal.contextual_id.as_deref() {
            state.push_str(&format!("internal_display_id={display_id}\n"));
        }
    }

    for display_id in &known_display_ids {
        state.push_str(&format!("known_display_id={display_id}\n"));
    }

    for display in &remembered {
        if let Some(line) = display_to_state_line(display) {
            state.push_str(&line);
            state.push('\n');
        }
    }

    write_support_file(STATE_FILE_NAME, &state)
}

fn remember_display_metadata(display: &Display) -> AppResult<()> {
    remember_displays(std::slice::from_ref(display))
}

fn merge_remembered_displays(live_displays: Vec<Display>) -> AppResult<Vec<Display>> {
    let mut remaining_live = live_displays;
    let mut displays = Vec::new();
    let mut remembered = read_remembered_displays()?;

    for target in load_targets()? {
        upsert_remembered_display(&mut remembered, target.to_absent_display());
    }

    for mut remembered in remembered {
        if let Some(index) = remaining_live
            .iter()
            .position(|display| same_display_identity(display, &remembered))
        {
            displays.push(remaining_live.remove(index));
            continue;
        }
        remembered.enabled = Some(false);
        remembered.remembered_absent = true;
        remembered.is_main = false;
        displays.push(remembered);
    }

    displays.extend(remaining_live);
    Ok(displays)
}

fn read_remembered_displays() -> AppResult<Vec<Display>> {
    let state = read_support_file(STATE_FILE_NAME)?.unwrap_or_default();
    let mut displays = Vec::new();

    for line in state.lines() {
        let Some(raw_display) = line.strip_prefix("display\t") else {
            continue;
        };
        if let Some(display) = parse_display_state_line(raw_display) {
            upsert_remembered_display(&mut displays, display);
        }
    }

    if let Some(internal_display_id) = state_value_from_body(&state, "internal_display_id") {
        let has_internal = displays.iter().any(Display::is_internal);
        if !has_internal {
            upsert_remembered_display(
                &mut displays,
                Display {
                    persistent_id: state_value_from_body(&state, "internal_id"),
                    contextual_id: Some(internal_display_id),
                    serial_id: state_value_from_body(&state, "internal_serial"),
                    display_type: Some("MacBook built in screen".to_string()),
                    enabled: Some(false),
                    remembered_absent: true,
                    ..Display::default()
                },
            );
        }
    }

    Ok(displays)
}

fn upsert_remembered_display(displays: &mut Vec<Display>, mut display: Display) {
    display.remembered_absent = true;
    display.enabled = Some(false);
    display.is_main = false;

    if let Some(index) = displays
        .iter()
        .position(|candidate| same_display_identity(candidate, &display))
    {
        displays[index] = display;
    } else {
        displays.push(display);
    }
}

fn same_display_identity(left: &Display, right: &Display) -> bool {
    if same_stable_display_identity(
        left.persistent_id.as_deref(),
        left.serial_id.as_deref(),
        right.persistent_id.as_deref(),
        right.serial_id.as_deref(),
    ) {
        return true;
    }

    if left.has_stable_identity() || right.has_stable_identity() {
        return false;
    }

    same_contextual_display_identity(
        left.contextual_id.as_deref(),
        right.contextual_id.as_deref(),
    )
}

fn same_stable_display_identity(
    left_persistent: Option<&str>,
    left_serial: Option<&str>,
    right_persistent: Option<&str>,
    right_serial: Option<&str>,
) -> bool {
    if let (Some(left), Some(right)) = (
        normalized_persistent_identity(left_persistent),
        normalized_persistent_identity(right_persistent),
    ) {
        if left == right {
            return true;
        }
    }

    if let (Some(left), Some(right)) = (
        normalized_serial_identity(left_serial),
        normalized_serial_identity(right_serial),
    ) {
        if left == right {
            return true;
        }
    }

    false
}

fn same_contextual_display_identity(left: Option<&str>, right: Option<&str>) -> bool {
    if let (Some(left), Some(right)) = (left, right) {
        return normalize_id(left) == normalize_id(right);
    }

    false
}

fn has_stable_display_identity(persistent_id: Option<&str>, serial_id: Option<&str>) -> bool {
    normalized_persistent_identity(persistent_id).is_some()
        || normalized_serial_identity(serial_id).is_some()
}

fn normalized_persistent_identity(value: Option<&str>) -> Option<String> {
    let normalized = value?.trim().to_ascii_lowercase();
    if normalized.is_empty() || normalized == "-" {
        None
    } else {
        Some(normalized)
    }
}

fn normalized_serial_identity(value: Option<&str>) -> Option<String> {
    let normalized = normalize_id(value?);
    if normalized.is_empty()
        || normalized == "-"
        || normalized == "0"
        || normalized == "unknown"
        || normalized == "no serial"
    {
        None
    } else {
        Some(normalized)
    }
}

fn display_to_state_line(display: &Display) -> Option<String> {
    if !display.has_stable_identity() {
        return None;
    }

    let mut fields = vec!["display".to_string()];
    push_target_field(&mut fields, "contextual", display.contextual_id.as_deref());
    push_target_field(&mut fields, "persistent", display.persistent_id.as_deref());
    push_target_field(&mut fields, "serial", display.serial_id.as_deref());
    push_target_field(&mut fields, "type", display.display_type.as_deref());
    push_target_field(&mut fields, "resolution", display.resolution.as_deref());
    fields.push(format!("internal={}", display.is_internal()));
    Some(fields.join("\t"))
}

fn parse_display_state_line(raw: &str) -> Option<Display> {
    let mut display = Display {
        enabled: Some(false),
        remembered_absent: true,
        ..Display::default()
    };
    let mut internal_hint = false;

    for field in raw.split('\t') {
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        let value = decode_field(value);
        match key {
            "contextual" => display.contextual_id = non_empty(value),
            "persistent" => display.persistent_id = non_empty(value),
            "serial" => display.serial_id = non_empty(value),
            "type" => display.display_type = non_empty(value),
            "resolution" => display.resolution = non_empty(value),
            "internal" => internal_hint = value == "true",
            _ => {}
        }
    }

    if display.display_type.is_none() {
        display.display_type = Some(if internal_hint {
            "MacBook built in screen".to_string()
        } else {
            "remembered external display".to_string()
        });
    }

    if !display.has_stable_identity() {
        return None;
    }

    Some(display)
}

fn remember_internal_id(id: &str, serial: Option<&str>, display_id: Option<&str>) {
    let mut state = format!("internal_id={id}\n");
    if let Some(serial) = serial {
        state.push_str(&format!("internal_serial={serial}\n"));
    }
    if let Some(display_id) = display_id {
        state.push_str(&format!("internal_display_id={display_id}\n"));
        if display_id.parse::<CGDirectDisplayID>().is_ok() {
            state.push_str(&format!("known_display_id={display_id}\n"));
        }
    }

    if let Err(error) = write_support_file(STATE_FILE_NAME, &state) {
        eprintln!("warning: failed to write displayed state: {error}");
    }
}

fn remember_contextual_display_id(display_id: CGDirectDisplayID) -> AppResult<()> {
    let value = display_id.to_string();
    let existing = read_support_file(STATE_FILE_NAME)?.unwrap_or_default();
    let known_line = format!("known_display_id={value}");
    let already_known = existing.lines().any(|line| line == known_line);
    if already_known {
        return Ok(());
    }

    let mut state = existing;
    if !state.is_empty() && !state.ends_with('\n') {
        state.push('\n');
    }
    state.push_str(&format!("known_display_id={value}\n"));
    write_support_file(STATE_FILE_NAME, &state)
}

fn read_state_value(key: &str) -> AppResult<Option<String>> {
    let Some(state) = read_support_file(STATE_FILE_NAME)? else {
        return Ok(None);
    };

    for line in state.lines() {
        let Some((candidate, value)) = line.split_once('=') else {
            continue;
        };
        if candidate == key {
            return Ok(Some(value.to_string()));
        }
    }

    Ok(None)
}

fn read_state_values(key: &str) -> AppResult<Vec<String>> {
    let Some(state) = read_support_file(STATE_FILE_NAME)? else {
        return Ok(Vec::new());
    };

    let mut values = Vec::new();
    for line in state.lines() {
        let Some((candidate, value)) = line.split_once('=') else {
            continue;
        };
        if candidate == key {
            values.push(value.to_string());
        }
    }
    Ok(values)
}

fn state_value_from_body(body: &str, key: &str) -> Option<String> {
    state_values_from_body(body, key).into_iter().next()
}

fn state_values_from_body(body: &str, key: &str) -> Vec<String> {
    let mut values = Vec::new();
    for line in body.lines() {
        let Some((candidate, value)) = line.split_once('=') else {
            continue;
        };
        if candidate == key {
            values.push(value.to_string());
        }
    }
    values
}

fn remembered_display_ids() -> AppResult<Vec<CGDirectDisplayID>> {
    let mut ids = BTreeSet::new();

    if let Some(display_id) = read_state_value("internal_display_id")? {
        if let Ok(display_id) = display_id.parse::<CGDirectDisplayID>() {
            ids.insert(display_id);
        }
    }

    for value in read_state_values("known_display_id")? {
        if let Ok(display_id) = value.parse::<CGDirectDisplayID>() {
            ids.insert(display_id);
        }
    }

    Ok(ids.into_iter().collect())
}

fn remembered_internal_display_id() -> AppResult<Option<CGDirectDisplayID>> {
    read_state_value("internal_display_id")?
        .map(|value| value.parse::<CGDirectDisplayID>())
        .transpose()
        .map_err(Into::into)
}

fn read_support_file(file_name: &str) -> AppResult<Option<String>> {
    let path = support_file_path(file_name)?;
    match fs::read_to_string(&path) {
        Ok(body) => Ok(Some(body)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn write_support_file(file_name: &str, body: &str) -> AppResult<()> {
    let path = support_file_path(file_name)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, body)?;
    Ok(())
}

fn remove_support_file(file_name: &str) -> AppResult<()> {
    remove_file_if_exists(support_file_path(file_name)?)?;
    Ok(())
}

fn remove_file_if_exists(path: PathBuf) -> AppResult<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn support_file_path(file_name: &str) -> AppResult<PathBuf> {
    let home = env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("displayed")
        .join(file_name))
}

fn load_displayplacer() -> AppResult<ParsedDisplayplacer> {
    let output = Command::new("displayplacer").arg("list").output()?;
    parse_displayplacer_output(output)
}

fn load_displayplacer_with_timeout(timeout: Duration) -> AppResult<ParsedDisplayplacer> {
    let output = run_command_with_timeout("displayplacer", &["list"], timeout)?;
    parse_displayplacer_output(output)
}

fn parse_displayplacer_output(output: Output) -> AppResult<ParsedDisplayplacer> {
    let raw = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    if !output.status.success() {
        return Err(format!("displayplacer list failed with status {}", output.status).into());
    }

    Ok(parse_displayplacer(&raw))
}

fn run_command_with_timeout(
    program: &str,
    arguments: &[&str],
    timeout: Duration,
) -> AppResult<Output> {
    let mut child = Command::new(program)
        .args(arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let started = Instant::now();

    loop {
        if child.try_wait()?.is_some() {
            return Ok(child.wait_with_output()?);
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait_with_output();
            return Err(format!(
                "`{} {}` timed out after {}ms",
                program,
                arguments.join(" "),
                timeout.as_millis()
            )
            .into());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn parse_displayplacer(raw: &str) -> ParsedDisplayplacer {
    let mut displays = Vec::new();
    let mut current: Option<Display> = None;
    let mut restore_lines = Vec::new();
    let mut in_restore_command = false;

    for line in raw.lines() {
        if let Some(value) = line.strip_prefix("Persistent screen id: ") {
            if let Some(display) = current.take() {
                displays.push(display);
            }
            current = Some(Display {
                persistent_id: Some(value.trim().to_string()),
                ..Display::default()
            });
            in_restore_command = false;
            continue;
        }

        if line.starts_with("displayplacer") {
            in_restore_command = true;
            restore_lines.push(line.trim_end().to_string());
            continue;
        }

        if in_restore_command {
            restore_lines.push(line.trim_end().to_string());
            continue;
        }

        let Some(display) = current.as_mut() else {
            continue;
        };

        if let Some(value) = line.strip_prefix("Contextual screen id: ") {
            display.contextual_id = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("Serial screen id: ") {
            display.serial_id = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("Type: ") {
            display.display_type = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("Resolution: ") {
            display.resolution = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("Hertz: ") {
            display.hertz = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("Color Depth: ") {
            display.color_depth = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("Scaling: ") {
            display.scaling = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("Origin: ") {
            display.is_main = value.contains(" - main display");
            let origin = value.split(" - ").next().unwrap_or(value).trim();
            display.origin = Some(origin.to_string());
        } else if let Some(value) = line.strip_prefix("Rotation: ") {
            let rotation = value.split(" - ").next().unwrap_or(value).trim();
            display.rotation = Some(rotation.to_string());
        } else if let Some(value) = line.strip_prefix("Enabled: ") {
            display.enabled = match value.trim() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            };
        }
    }

    if let Some(display) = current {
        displays.push(display);
    }

    ParsedDisplayplacer {
        displays,
        restore_command: if restore_lines.is_empty() {
            None
        } else {
            Some(restore_lines.join("\n"))
        },
        raw: raw.to_string(),
    }
}

fn print_parsed_displays(parsed: &ParsedDisplayplacer) {
    if parsed.displays.is_empty() {
        println!("No displays parsed from displayplacer output.");
        return;
    }

    for (index, display) in parsed.displays.iter().enumerate() {
        println!("[{index}] {}", display.brief());
        if let Some(contextual_id) = &display.contextual_id {
            println!("    contextual: {contextual_id}");
        }
        if let Some(resolution) = &display.resolution {
            println!("    resolution: {resolution}");
        }
        if let Some(origin) = &display.origin {
            println!("    origin: {origin}");
        }
        if let Some(rotation) = &display.rotation {
            println!("    rotation: {rotation}");
        }
    }
}

fn append_command_capture(body: &mut String, program: &str, arguments: &[&str]) {
    body.push_str(&format!(
        "\n## command: {} {}\n",
        program,
        arguments.join(" ")
    ));

    match run_capture(program, arguments) {
        Ok(capture) => {
            body.push_str(&format!("status: {}\n\n", capture.status));
            if !capture.stdout.is_empty() {
                body.push_str("stdout:\n");
                body.push_str(&capture.stdout);
                if !capture.stdout.ends_with('\n') {
                    body.push('\n');
                }
            }
            if !capture.stderr.is_empty() {
                body.push_str("stderr:\n");
                body.push_str(&capture.stderr);
                if !capture.stderr.ends_with('\n') {
                    body.push('\n');
                }
            }
        }
        Err(error) => {
            body.push_str(&format!("failed: {error}\n"));
        }
    }
}

fn append_betterdisplay_preferences(body: &mut String) {
    body.push_str("\n## BetterDisplay preferences\n");
    body.push_str(
        "warning: this section can include monitor serials, EDID data, and license metadata.\n",
    );
    append_command_capture(
        body,
        "defaults",
        &["read", "pro.betterdisplay.BetterDisplay"],
    );

    if let Some(home) = env::var_os("HOME") {
        let plist = PathBuf::from(home)
            .join("Library")
            .join("Preferences")
            .join("pro.betterdisplay.BetterDisplay.plist");
        if plist.exists() {
            let plist_string = plist.to_string_lossy().to_string();
            append_command_capture(body, "plutil", &["-p", &plist_string]);
        }
    }
}

struct Capture {
    status: String,
    stdout: String,
    stderr: String,
}

fn run_capture<S>(program: &str, arguments: &[S]) -> AppResult<Capture>
where
    S: AsRef<OsStr>,
{
    let output = Command::new(program).args(arguments).output()?;
    Ok(Capture {
        status: output.status.to_string(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

fn trim_to_width(value: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }

    let mut trimmed = String::new();
    for ch in value.chars() {
        if trimmed.len() + ch.len_utf8() > max_width {
            break;
        }
        trimmed.push(ch);
    }

    if trimmed.len() < value.len() && max_width > 3 {
        while trimmed.len() + 3 > max_width {
            trimmed.pop();
        }
        trimmed.push_str("...");
    }

    trimmed
}

fn normalize_id(value: &str) -> String {
    value
        .trim()
        .trim_start_matches('s')
        .trim_start_matches('S')
        .to_ascii_lowercase()
}

fn unix_timestamp() -> AppResult<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn slug(value: &str) -> String {
    let mut slug = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            slug.push(ch);
        } else if ch.is_whitespace() {
            slug.push('-');
        }
    }
    if slug.is_empty() {
        "state".to_string()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_displayplacer_blocks() {
        let raw = r#"Persistent screen id: ABC
Contextual screen id: 1
Serial screen id: s123
Type: MacBook built in screen
Resolution: 1512x982
Origin: (0,0) - main display
Rotation: 0 - rotate internal screen example
Enabled: true

Persistent screen id: DEF
Contextual screen id: 2
Serial screen id: s456
Type: 27 inch external screen
Resolution: 2560x1440
Origin: (1512,0)
Rotation: 0
Enabled: true

Execute the command below to set your screens to the current arrangement.

displayplacer "id:ABC enabled:true"
"#;

        let parsed = parse_displayplacer(raw);
        assert_eq!(parsed.displays.len(), 2);
        assert!(parsed.displays[0].is_internal());
        assert!(parsed.displays[0].is_main);
        assert!(parsed.displays[1].matches("s456"));
        assert!(parsed.restore_command.unwrap().contains("displayplacer"));
    }

    #[test]
    fn normalizes_serial_prefix() {
        assert_eq!(normalize_id("s123"), normalize_id("123"));
        assert_eq!(normalize_id("SABC"), normalize_id("abc"));
    }

    #[test]
    fn internal_restore_candidates_prefers_live_then_remembered_then_fallback() {
        assert_eq!(
            internal_restore_candidates(None, Some(5), Some(2)),
            vec![5, 2, FALLBACK_BUILTIN_DISPLAY_ID]
        );
    }

    #[test]
    fn internal_restore_candidates_dedupes_overlapping_ids() {
        assert_eq!(
            internal_restore_candidates(None, Some(1), Some(1)),
            vec![FALLBACK_BUILTIN_DISPLAY_ID]
        );
        assert_eq!(
            internal_restore_candidates(None, None, Some(2)),
            vec![2, FALLBACK_BUILTIN_DISPLAY_ID]
        );
    }

    #[test]
    fn internal_restore_candidates_falls_back_to_builtin_id_when_nothing_known() {
        assert_eq!(
            internal_restore_candidates(None, None, None),
            vec![FALLBACK_BUILTIN_DISPLAY_ID]
        );
    }

    #[test]
    fn internal_restore_candidates_uses_explicit_id_alone() {
        assert_eq!(
            internal_restore_candidates(Some(7), Some(5), Some(2)),
            vec![7]
        );
    }

    #[test]
    fn serializes_and_matches_monitor_targets() {
        let display = Display {
            persistent_id: Some("ABC".to_string()),
            contextual_id: Some("2".to_string()),
            serial_id: Some("s123".to_string()),
            display_type: Some("24 inch external screen".to_string()),
            resolution: Some("1920x1080".to_string()),
            enabled: Some(true),
            ..Display::default()
        };
        let target = MonitorTarget::from_display(&display).unwrap();
        let line = target_to_line(&target);
        let parsed = parse_target_line(&line).unwrap();

        assert!(parsed.matches_display(&display));
        assert!(parsed.same_identity(&target));
        assert_eq!(parsed.contextual_id.as_deref(), Some("2"));
    }

    #[test]
    fn monitor_target_rejects_contextual_only_display() {
        let display = Display {
            contextual_id: Some("17".to_string()),
            display_type: Some("remembered external display".to_string()),
            enabled: Some(false),
            remembered_absent: true,
            ..Display::default()
        };

        assert!(MonitorTarget::from_display(&display).is_none());
        assert!(parse_target_line("contextual=17\tlabel=remembered").is_none());
    }

    #[test]
    fn monitor_target_does_not_match_stale_contextual_when_stable_identity_differs() {
        let target = MonitorTarget {
            contextual_id: Some("2".to_string()),
            persistent_id: Some("TARGET".to_string()),
            serial_id: Some("s123".to_string()),
            display_type: Some("24 inch external screen".to_string()),
            resolution: Some("1920x1080".to_string()),
            label: "target".to_string(),
        };
        let display = Display {
            contextual_id: Some("2".to_string()),
            persistent_id: Some("OTHER".to_string()),
            serial_id: Some("s456".to_string()),
            display_type: Some("24 inch external screen".to_string()),
            resolution: Some("1920x1080".to_string()),
            enabled: Some(true),
            ..Display::default()
        };

        assert!(!target.matches_display(&display));
    }

    #[test]
    fn serializes_remembered_display_rows() {
        let display = Display {
            persistent_id: Some("ABC".to_string()),
            contextual_id: Some("2".to_string()),
            serial_id: Some("s123".to_string()),
            display_type: Some("24 inch external screen".to_string()),
            resolution: Some("1920x1080".to_string()),
            enabled: Some(true),
            ..Display::default()
        };

        let line = display_to_state_line(&display).unwrap();
        let raw = line.strip_prefix("display\t").unwrap();
        let parsed = parse_display_state_line(raw).unwrap();

        assert!(same_display_identity(&display, &parsed));
        assert_eq!(parsed.contextual_display_id(), Some(2));
        assert!(parsed.remembered_absent);
        assert!(!parsed.is_enabled());
    }

    #[test]
    fn remembered_display_rows_require_stable_identity() {
        let display = Display {
            contextual_id: Some("17".to_string()),
            display_type: Some("remembered external display".to_string()),
            enabled: Some(false),
            remembered_absent: true,
            ..Display::default()
        };

        assert!(display_to_state_line(&display).is_none());
        assert!(
            parse_display_state_line(
                "contextual=17\ttype=remembered external display\tinternal=false"
            )
            .is_none()
        );
    }

    #[test]
    fn remembered_display_identity_prefers_stable_ids_over_contextual_ids() {
        let remembered = Display {
            contextual_id: Some("2".to_string()),
            persistent_id: Some("ABC".to_string()),
            serial_id: Some("s123".to_string()),
            display_type: Some("24 inch external screen".to_string()),
            resolution: Some("1920x1080".to_string()),
            enabled: Some(false),
            remembered_absent: true,
            ..Display::default()
        };
        let same_display_new_contextual = Display {
            contextual_id: Some("9".to_string()),
            persistent_id: Some("ABC".to_string()),
            serial_id: Some("s123".to_string()),
            display_type: Some("24 inch external screen".to_string()),
            resolution: Some("1920x1080".to_string()),
            enabled: Some(true),
            ..Display::default()
        };
        let different_display_same_contextual = Display {
            contextual_id: Some("2".to_string()),
            persistent_id: Some("DEF".to_string()),
            serial_id: Some("s456".to_string()),
            display_type: Some("27 inch external screen".to_string()),
            resolution: Some("2560x1440".to_string()),
            enabled: Some(true),
            ..Display::default()
        };

        assert!(same_display_identity(
            &remembered,
            &same_display_new_contextual
        ));
        assert!(!same_display_identity(
            &remembered,
            &different_display_same_contextual
        ));
    }

    #[test]
    fn auto_watch_disables_internal_when_target_is_present() {
        let parsed = parsed_with_displays(vec![internal_display(true), external_display(true)]);

        assert_eq!(
            auto_watch_action_for_state(&parsed, true),
            AutoWatchAction::DisabledInternal
        );
    }

    #[test]
    fn auto_watch_leaves_internal_off_while_target_remains_present() {
        let parsed = parsed_with_displays(vec![external_display(true)]);

        assert_eq!(
            auto_watch_action_for_state(&parsed, true),
            AutoWatchAction::None
        );
    }

    #[test]
    fn auto_watch_restores_internal_when_target_is_absent_and_internal_is_off() {
        let parsed = parsed_with_displays(vec![external_display(false)]);

        assert_eq!(
            auto_watch_action_for_state(&parsed, false),
            AutoWatchAction::RestoredInternal
        );
    }

    #[test]
    fn auto_watch_does_not_restore_when_internal_is_already_enabled() {
        let parsed = parsed_with_displays(vec![internal_display(true)]);

        assert_eq!(
            auto_watch_action_for_state(&parsed, false),
            AutoWatchAction::None
        );
    }

    #[test]
    fn no_enabled_display_safety_restores_when_display_list_is_empty() {
        let parsed = parsed_with_displays(Vec::new());

        assert_eq!(
            no_enabled_display_safety_action_for_state(&parsed),
            AutoWatchAction::RestoredInternal
        );
    }

    #[test]
    fn no_enabled_display_safety_restores_when_all_displays_are_off() {
        let parsed = parsed_with_displays(vec![internal_display(false), external_display(false)]);

        assert_eq!(
            no_enabled_display_safety_action_for_state(&parsed),
            AutoWatchAction::RestoredInternal
        );
    }

    #[test]
    fn no_enabled_display_safety_stays_idle_when_external_is_enabled() {
        let parsed = parsed_with_displays(vec![external_display(true)]);

        assert_eq!(
            no_enabled_display_safety_action_for_state(&parsed),
            AutoWatchAction::None
        );
    }

    #[test]
    fn guard_stays_idle_when_clamshell_is_closed() {
        let parsed = parsed_with_displays(Vec::new());

        assert_eq!(
            internal_guard_action_for_state(ClamshellState::Closed, Some(&parsed)),
            GuardAction::None
        );
    }

    #[test]
    fn guard_stays_idle_when_internal_is_enabled() {
        let parsed = parsed_with_displays(vec![internal_display(true)]);

        assert_eq!(
            internal_guard_action_for_state(ClamshellState::Open, Some(&parsed)),
            GuardAction::None
        );
    }

    #[test]
    fn guard_preserves_external_only_when_external_is_enabled() {
        let parsed = parsed_with_displays(vec![external_display(true)]);

        assert_eq!(
            internal_guard_action_for_state(ClamshellState::Open, Some(&parsed)),
            GuardAction::None
        );
    }

    #[test]
    fn guard_restores_when_open_and_no_enabled_display_exists() {
        let parsed = parsed_with_displays(vec![internal_display(false), external_display(false)]);

        assert_eq!(
            internal_guard_action_for_state(ClamshellState::Open, Some(&parsed)),
            GuardAction::RestoreInternal
        );
    }

    #[test]
    fn guard_restores_when_open_and_display_state_is_unavailable() {
        assert_eq!(
            internal_guard_action_for_state(ClamshellState::Open, None),
            GuardAction::RestoreInternal
        );
    }

    #[test]
    fn display_change_detects_remove_or_disable_flags_as_display_loss() {
        assert!(
            DisplayChange {
                display: 2,
                flags: K_CG_DISPLAY_REMOVE_FLAG,
            }
            .is_display_loss()
        );
        assert!(
            DisplayChange {
                display: 2,
                flags: K_CG_DISPLAY_DISABLED_FLAG,
            }
            .is_display_loss()
        );
        assert!(
            !DisplayChange {
                display: 2,
                flags: 0,
            }
            .is_display_loss()
        );
    }

    #[test]
    fn display_loss_restore_ignores_remembered_internal_disable() {
        let change = DisplayChange {
            display: 1,
            flags: K_CG_DISPLAY_DISABLED_FLAG,
        };

        assert_eq!(
            display_loss_restore_action_for_change(change, Some(1), false),
            AutoWatchAction::None
        );
    }

    #[test]
    fn display_loss_restore_runs_for_external_remove() {
        let change = DisplayChange {
            display: 2,
            flags: K_CG_DISPLAY_REMOVE_FLAG,
        };

        assert_eq!(
            display_loss_restore_action_for_change(change, Some(1), false),
            AutoWatchAction::RestoredInternal
        );
    }

    #[test]
    fn display_loss_restore_stays_idle_for_non_loss_event() {
        let change = DisplayChange {
            display: 2,
            flags: 0,
        };

        assert_eq!(
            display_loss_restore_action_for_change(change, Some(1), false),
            AutoWatchAction::None
        );
    }

    #[test]
    fn display_loss_restore_ignores_builtin_even_when_remembered_id_is_stale() {
        // Built-in disabled, but the remembered internal_display_id is a stale
        // contextual id (1) that no longer matches the live id (7). The live builtin
        // check must still suppress the restore so the watcher does not loop.
        let change = DisplayChange {
            display: 7,
            flags: K_CG_DISPLAY_DISABLED_FLAG,
        };

        assert_eq!(
            display_loss_restore_action_for_change(change, Some(1), true),
            AutoWatchAction::None
        );
    }

    fn parsed_with_displays(displays: Vec<Display>) -> ParsedDisplayplacer {
        ParsedDisplayplacer {
            displays,
            restore_command: None,
            raw: String::new(),
        }
    }

    fn internal_display(enabled: bool) -> Display {
        Display {
            persistent_id: Some("INTERNAL".to_string()),
            contextual_id: Some("1".to_string()),
            display_type: Some("MacBook built in screen".to_string()),
            enabled: Some(enabled),
            ..Display::default()
        }
    }

    fn external_display(enabled: bool) -> Display {
        Display {
            persistent_id: Some("EXTERNAL".to_string()),
            contextual_id: Some("2".to_string()),
            serial_id: Some("s456".to_string()),
            display_type: Some("27 inch external screen".to_string()),
            enabled: Some(enabled),
            ..Display::default()
        }
    }
}
