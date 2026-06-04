use std::collections::BTreeSet;
use std::env;
use std::ffi::CString;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
const DEFAULT_WATCH_INTERVAL_SECS: u64 = 60;
const DEFAULT_RECONCILE_INTERVAL_SECS: u64 = 60;
type CGDirectDisplayID = u32;
type CGDisplayChangeSummaryFlags = u32;
type CGDisplayConfigRef = *mut std::ffi::c_void;
type CGError = i32;
const K_CG_CONFIGURE_PERMANENTLY: i32 = 1;

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
}

type CGDisplayReconfigurationCallBack =
    unsafe extern "C" fn(CGDirectDisplayID, CGDisplayChangeSummaryFlags, *mut std::ffi::c_void);

unsafe extern "C" {
    fn dlopen(path: *const std::ffi::c_char, mode: i32) -> *mut std::ffi::c_void;
    fn dlsym(
        handle: *mut std::ffi::c_void,
        symbol: *const std::ffi::c_char,
    ) -> *mut std::ffi::c_void;
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
        if display.contextual_id.is_none()
            && display.serial_id.is_none()
            && display.persistent_id.is_none()
        {
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

        if let (Some(target_serial), Some(display_serial)) =
            (self.serial_id.as_deref(), display.serial_id.as_deref())
        {
            if normalize_id(target_serial) == normalize_id(display_serial) {
                return true;
            }
        }

        if let (Some(target_contextual), Some(display_contextual)) = (
            self.contextual_id.as_deref(),
            display.contextual_id.as_deref(),
        ) {
            if normalize_id(target_contextual) == normalize_id(display_contextual) {
                return true;
            }
        }

        if let (Some(target_id), Some(display_id)) = (
            self.persistent_id.as_deref(),
            display.persistent_id.as_deref(),
        ) {
            if normalize_id(target_id) == normalize_id(display_id) {
                return true;
            }
        }

        false
    }

    fn same_identity(&self, other: &MonitorTarget) -> bool {
        if let (Some(left), Some(right)) = (self.serial_id.as_deref(), other.serial_id.as_deref()) {
            if normalize_id(left) == normalize_id(right) {
                return true;
            }
        }

        if let (Some(left), Some(right)) = (
            self.persistent_id.as_deref(),
            other.persistent_id.as_deref(),
        ) {
            if normalize_id(left) == normalize_id(right) {
                return true;
            }
        }

        if let (Some(left), Some(right)) = (
            self.contextual_id.as_deref(),
            other.contextual_id.as_deref(),
        ) {
            if normalize_id(left) == normalize_id(right) {
                return true;
            }
        }

        false
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
    DisplayChanged,
    Reconcile,
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
  displayed watch [--target-id ID | --target-serial SERIAL] [--interval SECONDS] [--dry-run] [--force]

Notes:
  - Running without arguments opens interactive mode.
  - `displayed watch` without a target uses monitors saved in interactive mode.
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
        start_display_event_source(DEFAULT_RECONCILE_INTERVAL_SECS);
    let mut targets = load_targets()?;
    let mut selected = 0usize;
    let mut status = event_status.unwrap_or_else(|| "ready".to_string());
    let mut parsed = load_display_state_for_ui(&mut selected, &mut status);
    let mut width = terminal::size()
        .map(|(columns, _)| columns as usize)
        .unwrap_or(100);

    if !targets.is_empty() {
        match apply_watch_targets(&targets, ActionMode::Execute, false, false) {
            Ok(true) => {
                status = "auto target matched; internal disabled".to_string();
                parsed = load_display_state_for_ui(&mut selected, &mut status);
            }
            Ok(false) => {}
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

                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
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
                            if !targets.is_empty() {
                                match apply_watch_targets(
                                    &targets,
                                    ActionMode::Execute,
                                    false,
                                    false,
                                ) {
                                    Ok(true) => {
                                        status =
                                            "auto target matched; internal disabled".to_string()
                                    }
                                    Ok(false) => {}
                                    Err(error) => status = format!("auto watch failed: {error}"),
                                }
                                should_refresh = true;
                            }
                            needs_render = true;
                        }
                        _ => {}
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
                WatchEvent::DisplayChanged => {
                    should_refresh = true;
                    if !targets.is_empty() {
                        match apply_watch_targets(&targets, ActionMode::Execute, false, false) {
                            Ok(true) => {
                                status = "auto target matched; internal disabled".to_string()
                            }
                            Ok(false) => status = "display change detected".to_string(),
                            Err(error) => status = format!("auto watch failed: {error}"),
                        }
                    } else {
                        status = "display change detected".to_string();
                    }
                    needs_render = true;
                }
                WatchEvent::Reconcile => {
                    should_refresh = true;
                    if !targets.is_empty() {
                        match apply_watch_targets(&targets, ActionMode::Execute, false, false) {
                            Ok(true) => {
                                status = "auto target reconciled; internal disabled".to_string()
                            }
                            Ok(false) => status = "periodic reconcile".to_string(),
                            Err(error) => status = format!("auto watch failed: {error}"),
                        }
                    }
                    needs_render = true;
                }
            }
        }

        if should_refresh {
            parsed = load_display_state_for_ui(&mut selected, &mut status);
            needs_render = true;
        }

        if needs_render {
            render_interactive(parsed.as_ref(), &targets, selected, &status, width)?;
        }
    }

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
            "[up/down] select  [space] toggle  [i] internal  [a] auto target  [s] safe reset  [r] refresh  [q] quit\r\n"
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
            restore_internal_once(None, ActionMode::Execute, false, false)?;
            Ok("internal restore requested".to_string())
        }
    } else {
        restore_internal_once(None, ActionMode::Execute, false, false)?;
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

    restore_internal_once(display_id, mode, displayplacer_fallback, true)
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
) -> AppResult<()> {
    detect_displays(mode, verbose)?;

    let display_id = match display_id {
        Some(display_id) => Some(display_id),
        None => read_state_value("internal_display_id")?
            .map(|value| value.parse())
            .transpose()?,
    };

    if let Some(display_id) = display_id {
        set_native_display_enabled(display_id, true, mode, verbose)?;
        if mode == ActionMode::Execute {
            remember_contextual_display_id(display_id)?;
        }
    } else if verbose {
        eprintln!(
            "warning: no internal display id; ran display redetection only. Try `restore-internal --display-id 1` if the built-in display's contextual/display ID is 1."
        );
    }

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

    if target.contextual_id.is_none()
        && target.serial_id.is_none()
        && target.persistent_id.is_none()
    {
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

fn apply_watch_targets(
    targets: &[MonitorTarget],
    mode: ActionMode,
    force: bool,
    verbose: bool,
) -> AppResult<bool> {
    if targets.is_empty() {
        return Ok(false);
    }

    let parsed = load_displayplacer()?;
    remember_internal(&parsed);

    let target_present = parsed.displays.iter().any(|display| {
        display.is_enabled() && targets.iter().any(|target| target.matches_display(display))
    });
    if !target_present {
        return Ok(false);
    }

    if !parsed
        .displays
        .iter()
        .any(|display| display.is_internal() && display.is_enabled())
    {
        return Ok(false);
    }

    disable_internal_once_with_verbosity(None, mode, force, false, verbose)?;
    Ok(true)
}

fn apply_raw_watch_target(
    target: &str,
    mode: ActionMode,
    force: bool,
    verbose: bool,
) -> AppResult<bool> {
    let parsed = load_displayplacer()?;
    remember_internal(&parsed);

    let target_present = parsed
        .displays
        .iter()
        .any(|display| display.matches(target) && display.is_enabled());
    if !target_present {
        return Ok(false);
    }

    if !parsed
        .displays
        .iter()
        .any(|display| display.is_internal() && display.is_enabled())
    {
        return Ok(false);
    }

    disable_internal_once_with_verbosity(Some(target), mode, force, false, verbose)?;
    Ok(true)
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
            Ok(true) => println!("watch matched target; internal disabled"),
            Ok(false) => {}
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
            Ok(true) => println!("watch matched saved target; internal disabled"),
            Ok(false) => {}
            Err(error) => eprintln!("watch iteration failed: {error}"),
        }
    }

    loop {
        let event = events.recv()?;
        match event {
            WatchEvent::DisplayChanged | WatchEvent::Reconcile => {
                if let Some(target) = target.as_deref() {
                    match apply_raw_watch_target(target, mode, force, true) {
                        Ok(true) => println!("watch matched target; internal disabled"),
                        Ok(false) => {}
                        Err(error) => eprintln!("watch iteration failed: {error}"),
                    }
                } else {
                    match apply_watch_targets(&saved_targets, mode, force, true) {
                        Ok(true) => println!("watch matched saved target; internal disabled"),
                        Ok(false) => {}
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
    _display: CGDirectDisplayID,
    _flags: CGDisplayChangeSummaryFlags,
    user_info: *mut std::ffi::c_void,
) {
    if user_info.is_null() {
        return;
    }

    let sender = unsafe { &*(user_info as *const mpsc::Sender<WatchEvent>) };
    let _ = sender.send(WatchEvent::DisplayChanged);
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
        return Err(format!("CGSConfigureDisplayEnabled failed: {configure}").into());
    }

    let complete = unsafe { CGCompleteDisplayConfiguration(config, K_CG_CONFIGURE_PERMANENTLY) };
    if complete != 0 {
        return Err(format!("CGCompleteDisplayConfiguration failed: {complete}").into());
    }

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

    let mut known_display_ids: BTreeSet<CGDirectDisplayID> =
        remembered_display_ids()?.into_iter().collect();
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

    let internal_display_id = state_value_from_body(&state, "internal_display_id");
    for value in state_values_from_body(&state, "known_display_id") {
        if displays
            .iter()
            .any(|display| display.contextual_id.as_deref() == Some(value.as_str()))
        {
            continue;
        }
        let is_internal = internal_display_id.as_deref() == Some(value.as_str());
        upsert_remembered_display(
            &mut displays,
            Display {
                contextual_id: Some(value),
                display_type: Some(if is_internal {
                    "MacBook built in screen".to_string()
                } else {
                    "remembered external display".to_string()
                }),
                enabled: Some(false),
                remembered_absent: true,
                ..Display::default()
            },
        );
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
    if let (Some(left), Some(right)) = (
        left.contextual_id.as_deref(),
        right.contextual_id.as_deref(),
    ) {
        if normalize_id(left) == normalize_id(right) {
            return true;
        }
    }

    if let (Some(left), Some(right)) = (left.serial_id.as_deref(), right.serial_id.as_deref()) {
        if normalize_id(left) == normalize_id(right) {
            return true;
        }
    }

    if let (Some(left), Some(right)) = (
        left.persistent_id.as_deref(),
        right.persistent_id.as_deref(),
    ) {
        if normalize_id(left) == normalize_id(right) {
            return true;
        }
    }

    false
}

fn display_to_state_line(display: &Display) -> Option<String> {
    if display.contextual_id.is_none()
        && display.persistent_id.is_none()
        && display.serial_id.is_none()
    {
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

    if display.contextual_id.is_none()
        && display.persistent_id.is_none()
        && display.serial_id.is_none()
    {
        return None;
    }

    if display.display_type.is_none() {
        display.display_type = Some(if internal_hint {
            "MacBook built in screen".to_string()
        } else {
            "remembered external display".to_string()
        });
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
}
