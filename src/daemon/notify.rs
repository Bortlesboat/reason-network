//! Desktop notifications, for a message that needs the person and not only their agent (`notify`). Shown with
//! what the system has: osascript on macOS, notify-send on Linux, a toast from Windows PowerShell. Where there is
//! nothing to show one with, the sender is told; people turn them off in their system's notification settings.

use super::{Daemon, log};
use crate::{agent, proto::Message, util};
use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// At most one notification per person sending in this time: a looping agent can't flood the screen.
const GAP: Duration = Duration::from_secs(60);
/// Older than this (it waited in an outbox while this machine was off), it no longer pops up.
const STALE_MS: i64 = 15 * 60 * 1000;

impl Daemon {
    /// Shows `msg` to this machine's person. Returns whether it was handed to the system's notifications (which may
    /// still hide it, say if the person turned them off), and a note for the sender.
    pub(super) fn notify(&self, msg: &Message) -> (bool, String) {
        if util::now_ms() - msg.sent_at > STALE_MS {
            return (false, "not popped up: it's over 15 minutes old".into());
        }
        let person = msg.from.split('/').next().unwrap_or_default().to_string();
        let title = match msg.to.split_once('/') {
            Some((_, session)) => format!("{} → {session}", msg.from),
            None => msg.from.clone(),
        };
        let mut shown = self.shown.lock().unwrap();
        shown.retain(|_, at| at.elapsed() < GAP);
        if shown.contains_key(&person) {
            return (false, format!("not popped up: {} had one from {person} a moment ago", self.machine));
        }
        let Some(mut cmd) = command(&title, &util::one_line(&msg.body, 200)) else {
            return (false, format!("not popped up: {} has no desktop to show it on", self.machine));
        };
        shown.insert(person, Instant::now());
        std::thread::spawn(move || {
            match cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).output() {
                Ok(o) if o.status.success() => {}
                Ok(o) => log(&format!("notification failed: {}", String::from_utf8_lossy(&o.stderr).trim())),
                Err(e) => log(&format!("notification failed: {e}")),
            }
        });
        (true, format!("popped up for {} ({})", self.person, self.machine))
    }
}

#[cfg(target_os = "macos")]
fn command(title: &str, body: &str) -> Option<Command> {
    let mut cmd = Command::new(agent::find_bin("osascript")?);
    let show = r#"display notification (item 2 of argv) with title (item 1 of argv) sound name "Glass""#;
    cmd.args(["-e", "on run argv", "-e", show, "-e", "end run", title, body]);
    Some(cmd)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn command(title: &str, body: &str) -> Option<Command> {
    let mut cmd = notify_send()?;
    // `--` keeps a body starting with `-` from being taken for an option.
    cmd.args(["--app-name=Reason Network".into(), "--".into(), markup(title), markup(body)]);
    Some(cmd)
}

/// A daemon started outside the desktop session may lack the session bus's address; without a bus, no desktop.
#[cfg(all(unix, not(target_os = "macos")))]
pub(super) fn notify_send() -> Option<Command> {
    let mut cmd = Command::new(agent::find_bin("notify-send")?);
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
        let bus = format!("/run/user/{}/bus", unsafe { libc::getuid() });
        if !std::path::Path::new(&bus).exists() {
            return None;
        }
        cmd.env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={bus}"));
    }
    Some(cmd)
}

/// Most notification servers read markup.
#[cfg(all(unix, not(target_os = "macos")))]
pub(super) fn markup(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(windows)]
fn command(title: &str, body: &str) -> Option<Command> {
    const TOAST: &str = "$ErrorActionPreference = 'Stop'
$n = [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime]
$x = $n::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02)
$t = $x.GetElementsByTagName('text')
$t.Item(0).AppendChild($x.CreateTextNode($env:TINCAN_TITLE)) > $null
$t.Item(1).AppendChild($x.CreateTextNode($env:TINCAN_BODY)) > $null
$app = '{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\\WindowsPowerShell\\v1.0\\powershell.exe'
$n::CreateToastNotifier($app).Show([Windows.UI.Notifications.ToastNotification]::new($x))";
    let mut cmd = powershell(TOAST)?;
    cmd.env("TINCAN_TITLE", title).env("TINCAN_BODY", body);
    Some(cmd)
}

#[cfg(windows)]
pub(super) fn powershell(script: &str) -> Option<Command> {
    use std::os::windows::process::CommandExt;
    let mut cmd = Command::new(agent::find_bin("powershell")?);
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", script]);
    // CREATE_NO_WINDOW: no console flashing up next to the notification.
    cmd.creation_flags(0x0800_0000);
    Some(cmd)
}
