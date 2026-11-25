//! The launchd guard has no long-lived CoreGraphics connection. Use a live
//! physical link as a positive hint between full checks. Unknown input always
//! falls back to the regular guard, including on systems without these services.
use super::{
    ActionMode, AppResult, ClamshellState, MonitorTarget, ParsedDisplayplacer,
    check_internal_guard, load_targets, pending_wake_restore, read_clamshell_state,
    read_support_file, remove_support_file, run_command_with_timeout, run_wake_recovery_once,
    support_file_path, unix_timestamp, wake_reason_output_contains_lid,
};
use std::{fs, time::Duration};

const OBSERVED_WAKE: &str = "guard-observed-wake";
const VERIFIED_LINK: &str = "guard-verified-link";
const RECHECK_SECS: u64 = 300;

fn registry(class: &str) -> Option<String> {
    let out = run_command_with_timeout(
        "/usr/sbin/ioreg",
        &["-r", "-c", class, "-d", "1"],
        Duration::from_secs(1),
    )
    .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

fn property<'a>(row: &'a str, key: &str) -> Option<&'a str> {
    row.lines()
        .filter_map(|line| line.trim().split_once(" = "))
        .find_map(|(k, v)| {
            (k.strip_prefix('"')?.strip_suffix('"')? == key).then(|| v.trim_matches('"'))
        })
}

fn store(name: &str, value: &str) -> AppResult<()> {
    let path = support_file_path(name)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&temporary, value)?;
    fs::rename(temporary, path)?;
    Ok(())
}

pub(super) struct Wake {
    id: String,
    pub(super) lid: bool,
}

impl Wake {
    pub(super) fn read() -> Option<Self> {
        Self::from_registry(&registry("IOPMrootDomain")?)
    }

    fn from_registry(root: &str) -> Option<Self> {
        let uuid = property(root, "SleepWakeUUID")?;
        if uuid.len() != 36 || !uuid.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-') {
            return None;
        }
        property(root, "Wake Reason")?;
        Some(Self {
            id: uuid.into(),
            lid: wake_reason_output_contains_lid(root),
        })
    }
    fn token(&self) -> String {
        format!("{}:{}", self.id, if self.lid { "lid" } else { "other" })
    }
    fn new_lid_wake(&self, previous: Option<&str>) -> bool {
        self.lid && previous.is_some_and(|old| old != self.token())
    }
    pub(super) fn was_acknowledged(&self) -> bool {
        read_support_file(OBSERVED_WAKE).ok().flatten().as_deref() == Some(&self.token())
    }

    fn should_record(&self, previous: Option<&str>) -> bool {
        // A DarkWake and subsequent full wake can share a UUID. Once this
        // cycle's lid wake was handled, keep that fact through other reasons.
        previous != Some(&self.token())
            && !(!self.lid && previous == Some(&format!("{}:lid", self.id)))
    }

    pub(super) fn acknowledge(&self) {
        // Missing optional state must never prevent a display recovery.
        let _ = store(OBSERVED_WAKE, &self.token());
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Link {
    entry: u64,
    serial: String,
}

impl Link {
    fn from_registry(body: &str, targets: &[MonitorTarget]) -> Option<Self> {
        for record in body.split("+-o ").skip(1) {
            let header = record.lines().next()?;
            if !header.contains("<class IOPortTransportStateDisplayPort,")
                || property(record, "Active") != Some("Yes")
                || property(record, "HPD_StateDescription") != Some("High")
                || property(record, "RoleDescription") != Some("Source")
                || property(record, "SinkCount") != Some("1")
                || !["LaneCount", "LinkRate"].iter().all(|key| {
                    property(record, key)
                        .and_then(|n| n.parse::<u32>().ok())
                        .is_some_and(|n| n > 0)
                })
            {
                continue;
            }
            let Some(serial) = property(record, "SerialNumber").and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            if serial == 0 || serial == u32::MAX {
                continue;
            }
            let serial = format!("s{serial}");
            if !targets
                .iter()
                .any(|target| target.serial_id.as_deref() == Some(&serial))
            {
                continue;
            }
            let entry =
                u64::from_str_radix(header.split_once(", id 0x")?.1.split(',').next()?, 16).ok()?;
            return Some(Self { entry, serial });
        }
        None
    }

    fn confirmed(&self, parsed: &ParsedDisplayplacer, targets: &[MonitorTarget]) -> bool {
        parsed.displays.iter().any(|d| {
            !d.remembered_absent
                && d.is_enabled()
                && !d.is_internal()
                && d.serial_id.as_deref() == Some(&self.serial)
                && targets.iter().any(|t| t.matches_display(d))
        })
    }
    fn key(&self, wake: &Wake) -> String {
        format!("{} {} {}", wake.token(), self.entry, self.serial)
    }
}

fn fresh_match(cached: &str, key: &str, now: u64) -> bool {
    let Some((time, old_key)) = cached.split_once(' ') else {
        return false;
    };
    old_key == key
        && time
            .parse()
            .ok()
            .and_then(|t| now.checked_sub(t))
            .is_some_and(|age| age < RECHECK_SECS)
}

// On upgrade the first saved observation may be missing. Only treat a
// preexisting lid-wake reason as new if the kernel just recorded the wake.
fn recent_system_wake() -> bool {
    let Ok(out) = run_command_with_timeout(
        "/usr/sbin/sysctl",
        &["-n", "kern.waketime"],
        Duration::from_secs(1),
    ) else {
        return false;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let seconds = text
        .split_once("sec = ")
        .and_then(|(_, rest)| rest.split(',').next())
        .and_then(|s| s.trim().parse::<u64>().ok());
    seconds
        .zip(unix_timestamp().ok())
        .and_then(|(wake, now)| now.checked_sub(wake))
        .is_some_and(|age| age <= 60)
}

pub(super) fn run(verbose: bool) -> AppResult<()> {
    let mode = ActionMode::Execute;
    let previous = read_support_file(OBSERVED_WAKE).ok().flatten();
    let wake = Wake::read();
    if pending_wake_restore()?.is_some() {
        return run_wake_recovery_once("launchd", mode, verbose);
    }
    if wake.as_ref().is_some_and(|w| {
        w.new_lid_wake(previous.as_deref()) || (previous.is_none() && w.lid && recent_system_wake())
    }) {
        return run_wake_recovery_once("launchd-lid-wake", mode, verbose);
    }
    let lid = read_clamshell_state(verbose);
    if lid == ClamshellState::Closed {
        let _ = remove_support_file(VERIFIED_LINK);
        if let Some(wake) = wake
            && wake.should_record(previous.as_deref())
        {
            wake.acknowledge();
        }
        return Ok(());
    }

    let targets = load_targets().unwrap_or_default();
    let link = if lid == ClamshellState::Open && wake.is_some() && !targets.is_empty() {
        registry("IOPortTransportStateDisplayPort")
            .and_then(|body| Link::from_registry(&body, &targets))
    } else {
        None
    };
    let key = link.as_ref().zip(wake.as_ref()).map(|(l, w)| l.key(w));
    let now = unix_timestamp()?;
    let cached = read_support_file(VERIFIED_LINK).ok().flatten();
    if key
        .as_ref()
        .zip(cached.as_deref())
        .is_some_and(|(key, saved)| fresh_match(saved, key, now))
    {
        if verbose {
            println!("guard: saved display is on the verified physical link");
        }
        return Ok(());
    }
    // Discard a previous positive result before attempting the fresh check: a
    // failure, unplug, wake or target edit must not revive that result later.
    if cached.is_some() {
        let _ = remove_support_file(VERIFIED_LINK);
    }
    let parsed = check_internal_guard("launchd", mode, verbose, lid, &targets)?;
    if let (Some(key), Some(link), Some(parsed)) = (key, link, parsed)
        && link.confirmed(&parsed, &targets)
    {
        let _ = store(VERIFIED_LINK, &format!("{now} {key}"));
    }
    if let Some(wake) = wake
        && wake.should_record(previous.as_deref())
    {
        wake.acknowledge();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Display, parse_displayplacer};

    const PORT: &str = r#"+-o DisplayPort  <class IOPortTransportStateDisplayPort, id 0x23, active>
    {
      "Active" = Yes
      "HPD_StateDescription" = "High"
      "RoleDescription" = "Source"
      "SinkCount" = 1
      "LaneCount" = 4
      "LinkRate" = 3
      "SerialNumber" = 4242
    }
"#;

    fn saved() -> MonitorTarget {
        MonitorTarget::from_display(&Display {
            persistent_id: Some("external-uuid".into()),
            serial_id: Some("s4242".into()),
            display_type: Some("27 inch external screen".into()),
            ..Display::default()
        })
        .unwrap()
    }

    #[test]
    fn link_requires_live_signals_on_the_same_saved_single_sink() {
        assert_eq!(
            Link::from_registry(PORT, &[saved()]),
            Some(Link {
                entry: 35,
                serial: "s4242".into()
            })
        );
        for (key, old, new) in [
            ("Active", "Yes", "No"),
            ("HPD_StateDescription", "\"High\"", "\"Low\""),
            ("RoleDescription", "\"Source\"", "\"Sink\""),
            ("SinkCount", "1", "2"),
            ("LaneCount", "4", "0"),
            ("LinkRate", "3", "0"),
            ("SerialNumber", "4242", "0"),
            ("SerialNumber", "4242", "4243"),
        ] {
            let record = PORT.replace(&format!("\"{key}\" = {old}"), &format!("\"{key}\" = {new}"));
            assert!(Link::from_registry(&record, &[saved()]).is_none(), "{key}");
        }
        let split = format!(
            "{}{}",
            PORT.replace("4242", "2"),
            PORT.replace(" = Yes", " = No")
        );
        assert!(Link::from_registry(&split, &[saved()]).is_none());
        assert!(Link::from_registry(PORT, &[]).is_none());
    }

    #[test]
    fn a_full_check_must_confirm_the_same_live_enabled_monitor() {
        let target = saved();
        let link = Link::from_registry(PORT, std::slice::from_ref(&target)).unwrap();
        let mut layout = parse_displayplacer(
            "Persistent screen id: external-uuid\nSerial screen id: s4242\nType: 27 inch external screen\nEnabled: true\n",
        );
        let targets = [target];
        assert!(link.confirmed(&layout, &targets));
        layout.displays[0].enabled = Some(false);
        assert!(!link.confirmed(&layout, &targets));
        layout.displays[0].enabled = Some(true);
        layout.displays[0].remembered_absent = true;
        assert!(!link.confirmed(&layout, &targets));
    }

    #[test]
    fn physical_link_validation_expires_and_cannot_cross_sleep_or_reconnect() {
        let saved = "100 wakeA:other 35 s4242";
        assert!(fresh_match(saved, "wakeA:other 35 s4242", 399));
        assert!(!fresh_match(saved, "wakeA:other 35 s4242", 400));
        assert!(!fresh_match(saved, "wakeA:other 35 s4242", 99));
        assert!(!fresh_match(saved, "wakeB:other 35 s4242", 103));
        assert!(!fresh_match(saved, "wakeA:other 36 s4242", 103));
        assert!(!fresh_match("partial write", "wakeA:other 35 s4242", 103));
    }

    #[test]
    fn power_observation_needs_a_wake_reason_and_cycle_id() {
        let raw = r#""SleepWakeUUID" = "12345678-0000-0000-0000-000000000001"
                      "Wake Reason" = "smc.sysState.Wake lid SMC.OutboxNotEmpty""#;
        let wake = Wake::from_registry(raw).unwrap();
        assert!(wake.lid);
        assert!(Wake::from_registry(&raw.replace("SleepWakeUUID", "Other")).is_none());
        assert!(Wake::from_registry(&raw.replace("Wake Reason", "Other")).is_none());
        let later = Wake::from_registry(&raw.replace(" lid ", " RTC ")).unwrap();
        assert!(!later.should_record(Some(&wake.token())));
        assert!(wake.should_record(Some(&later.token())));
    }

    #[test]
    fn periodic_guard_recognizes_a_lid_wake_with_or_without_a_preceding_darkwake() {
        let lid = Wake {
            id: "cycle-b".into(),
            lid: true,
        };
        assert!(lid.new_lid_wake(Some("cycle-a:lid")));
        assert!(lid.new_lid_wake(Some("cycle-b:other")));
        assert!(!lid.new_lid_wake(Some("cycle-b:lid")));
        assert!(!lid.new_lid_wake(None));
        assert!(
            !Wake {
                id: "cycle-b".into(),
                lid: false
            }
            .new_lid_wake(Some("cycle-a:lid"))
        );
    }
}
