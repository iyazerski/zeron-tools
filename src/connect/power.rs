//! This Mac's battery, for the phone: read with `pmset` every minute.

use std::time::Duration;

use tokio::sync::watch;
use zeron_relay_proto::{Battery, BatteryState};

/// How often to read the battery. The charge moves slowly.
const READ_EVERY: Duration = Duration::from_secs(60);
/// A read gets this long, so a stuck `pmset` keeps the last state.
const PMSET_TIMEOUT: Duration = Duration::from_secs(5);

/// Read the battery forever and publish it; `None` on a Mac without one.
pub fn spawn() -> watch::Receiver<Option<Battery>> {
    let (tx, rx) = watch::channel(None);
    tokio::spawn(async move {
        loop {
            if let Some(battery) = read().await {
                tx.send_if_modified(|now| {
                    let changed = *now != battery;
                    *now = battery;
                    changed
                });
            }
            tokio::time::sleep(READ_EVERY).await;
        }
    });
    rx
}

/// Read the battery once: `None` if `pmset` failed, `Some(None)` if the Mac has no battery.
async fn read() -> Option<Option<Battery>> {
    let output = tokio::process::Command::new("/usr/bin/pmset")
        .args(["-g", "batt"])
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(PMSET_TIMEOUT, output).await {
        Ok(Ok(o)) if o.status.success() => Some(parse(&String::from_utf8_lossy(&o.stdout))),
        _ => None,
    }
}

/// Read the battery from `pmset -g batt` output, which looks like this (with
/// a tab before the percentage):
///
/// ```text
/// Now drawing from 'AC Power'
///  -InternalBattery-0 (id=23199843)    85%; charging; 1:02 remaining present: true
/// ```
fn parse(pmset: &str) -> Option<Battery> {
    let line = pmset.lines().find(|l| l.contains("InternalBattery"))?;
    let mut fields = line.split_once('\t')?.1.split(';').map(str::trim);
    let percent = fields.next()?.strip_suffix('%')?.parse().ok()?;
    let charge = fields.next().unwrap_or_default();
    let state = if !pmset.contains("'AC Power'") {
        BatteryState::Unplugged
    } else if charge == "charging" || charge == "finishing charge" {
        BatteryState::Charging
    } else {
        // `charged`, or `AC attached` while macOS holds the charge.
        BatteryState::Plugged
    };
    Some(Battery { percent, state })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_comes_from_pmset() {
        let read = |source: &str, rest: &str| {
            parse(&format!(
                "Now drawing from '{source}'\n -InternalBattery-0 (id=23199843)\t{rest} present: true\n"
            ))
        };
        let battery = |percent, state| Some(Battery { percent, state });
        assert_eq!(
            read("AC Power", "85%; charging; 1:02 remaining"),
            battery(85, BatteryState::Charging)
        );
        assert_eq!(
            read("AC Power", "100%; charged; 0:00 remaining"),
            battery(100, BatteryState::Plugged)
        );
        assert_eq!(
            read("AC Power", "80%; AC attached; not charging"),
            battery(80, BatteryState::Plugged)
        );
        assert_eq!(
            read("Battery Power", "42%; discharging; 3:10 remaining"),
            battery(42, BatteryState::Unplugged)
        );
        // A desktop Mac.
        assert_eq!(parse("Now drawing from 'AC Power'\n"), None);
    }
}
