//! `ask_user`: an agent asks its own person, with a pop-up on this machine, and gets the answer back as the tool's
//! result: their own decision, not a message from anyone. A dialog with a note on macOS, a toast with one on Windows,
//! a notification with buttons on Linux.

use super::{Daemon, log, mail::unregistered};
use crate::{proto::Message, util};
use anyhow::{Result, anyhow, bail};
use std::{process::Command, sync::Arc, time::Duration};
use tokio::sync::watch;
use ulid::Ulid;

/// A pop-up nobody answers closes after this.
const OPEN_SECS: u64 = 3600;

/// What came of a question.
#[derive(Clone)]
pub(super) enum Answer {
    Approved(String),
    Denied(String),
    /// Closed, or not answered in time.
    None,
    /// It couldn't be shown.
    Failed(String),
}

impl Daemon {
    /// Asks this machine's person on behalf of the session `key`, and waits up to `wait_secs` for the answer. A
    /// session has one question up at a time: asking again while it is up waits for that one. An answer that comes
    /// once the session has stopped waiting reaches it as a message from its person.
    pub(super) async fn ask(self: &Arc<Self>, key: String, question: String, wait_secs: u64) -> Result<String> {
        let (asker, stop) = {
            let g = self.inner.lock().unwrap();
            let l = g.live.get(&key).ok_or_else(unregistered)?;
            (self.addr(&l.name), l.stop_gen)
        };
        let mut answer = {
            let mut open = self.asks.lock().unwrap();
            match open.get(&key) {
                Some(up) => up.subscribe(),
                None => {
                    let cmd = dialog(&format!("{asker} asks"), &question).ok_or_else(|| {
                        anyhow!(
                            "{} has no screen to ask {} on: ask them in your own session",
                            self.machine,
                            self.person
                        )
                    })?;
                    let (tx, rx) = watch::channel(None);
                    open.insert(key.clone(), tx);
                    tokio::spawn(self.clone().pop_up(key.clone(), question, cmd));
                    rx
                }
            }
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_secs);
        loop {
            // The client giving up on the call (`StopWaiting`) wakes `arrived`.
            let stopped = self.arrived.notified();
            tokio::pin!(stopped);
            stopped.as_mut().enable();
            if let Some(a) = answer.borrow_and_update().clone() {
                return self.said(&a);
            }
            let gave_up = self.inner.lock().unwrap().live.get(&key).is_none_or(|l| l.stop_gen != stop);
            if gave_up || tokio::time::Instant::now() >= deadline {
                return Ok(format!(
                    "{} hasn't answered yet; the pop-up stays up. Ask again to keep waiting: an answer after that \
                     comes to you as a message from them.",
                    self.person
                ));
            }
            tokio::select! {
                changed = answer.changed() => if changed.is_err() { return self.said(&Answer::None) },
                _ = tokio::time::timeout_at(deadline, stopped) => {}
            }
        }
    }

    /// The answer, as the asking agent reads it.
    fn said(&self, answer: &Answer) -> Result<String> {
        let note = |n: &str| if n.is_empty() { String::new() } else { format!(" Their note: {n}") };
        Ok(match answer {
            Answer::Approved(n) => format!("{} approved.{}", self.person, note(n)),
            Answer::Denied(n) => format!("{} said no.{}", self.person, note(n)),
            Answer::None => format!("{} didn't answer.", self.person),
            Answer::Failed(e) => {
                bail!("couldn't ask {} with a pop-up ({e}): ask them in your own session", self.person)
            }
        })
    }

    /// Shows the question until it is answered or closes, then hands the answer to whoever waits for it.
    async fn pop_up(self: Arc<Self>, key: String, question: String, cmd: Command) {
        let mut cmd = tokio::process::Command::from(cmd);
        let out = tokio::time::timeout(Duration::from_secs(OPEN_SECS + 60), cmd.kill_on_drop(true).output()).await;
        let answer = match out {
            Ok(Ok(o)) if o.status.success() => parse(&String::from_utf8_lossy(&o.stdout)),
            Ok(Ok(o)) => Answer::Failed(String::from_utf8_lossy(&o.stderr).trim().to_string()),
            Ok(Err(e)) => Answer::Failed(e.to_string()),
            Err(_) => Answer::None,
        };
        if let Answer::Failed(e) = &answer {
            log(&format!("pop-up failed: {e}"));
        }
        let Some(tx) = self.asks.lock().unwrap().remove(&key) else { return };
        // Nobody waits for it any more: the answer goes to the session as a message from its person.
        if tx.send(Some(answer.clone())).is_err() && matches!(answer, Answer::Approved(_) | Answer::Denied(_)) {
            self.answer_later(&key, &question, &answer);
        }
    }

    fn answer_later(self: &Arc<Self>, key: &str, question: &str, answer: &Answer) {
        let Ok(said) = self.said(answer) else { return };
        let mut g = self.inner.lock().unwrap();
        let Some(to) = g.live.get(key).map(|l| self.addr(&l.name)) else {
            return log(&format!("{key} had gone before {} answered its question", self.person));
        };
        let msg = Message {
            id: Ulid::new().to_string(),
            from: format!("{}/terminal", self.person),
            from_agent: None,
            from_repo: None,
            to,
            via: None,
            reply_to: None,
            body: format!("Your question \"{}\" (ask_user): {said}", util::one_line(question, 200)),
            wake: true,
            notify: false,
            sent_at: util::now_ms(),
        };
        if let Err(e) = g.store.insert(&msg, false) {
            return log(&format!("couldn't keep a late answer: {e}"));
        }
        self.deliver(&mut g, key, vec![msg], true);
        drop(g);
        self.arrived.notify_waiters();
    }
}

/// `approve` or `deny` on the first line, the note after it; anything else is no answer.
fn parse(out: &str) -> Answer {
    let (choice, note) = out.trim().split_once('\n').unwrap_or((out.trim(), ""));
    let note = note.trim().to_string();
    match choice.trim().to_lowercase().as_str() {
        "approve" => Answer::Approved(note),
        "deny" => Answer::Denied(note),
        _ => Answer::None,
    }
}

/// Prints the button clicked and, on the next line, the note; nothing if nobody answered. Return approves, so it
/// waits for the person to stop typing for 2 seconds (5 minutes at most): what they type elsewhere can't answer it.
#[cfg(target_os = "macos")]
const DIALOG: &str = r#"on run argv
	repeat 600 times
		set secs to do shell script "ioreg -c IOHIDSystem | awk '/HIDIdleTime/ {print int($NF / 1000000000); exit}'"
		if (secs as integer) ≥ 2 then exit repeat
		delay 0.5
	end repeat
	set answer to display dialog (item 2 of argv) with title (item 1 of argv) default answer "" ¬
		buttons {"Deny", "Approve"} default button "Approve" ICON giving up after (item 3 of argv as integer)
	if gave up of answer then return ""
	return (button returned of answer) & linefeed & (text returned of answer)
end run"#;

#[cfg(target_os = "macos")]
fn dialog(title: &str, question: &str) -> Option<Command> {
    let mut cmd = Command::new(crate::agent::find_bin("osascript")?);
    let question: String = util::printable(question).trim().chars().take(1500).collect();
    let (icon, path) = match icon() {
        Some(path) => ("with icon (POSIX file (item 4 of argv))", path.display().to_string()),
        None => ("with icon note", String::new()),
    };
    cmd.args(["-e", &DIALOG.replace("ICON", icon), title, &question, &OPEN_SECS.to_string(), &path]);
    Some(cmd)
}

/// Reason's icon for the dialog, kept beside the network's database.
#[cfg(target_os = "macos")]
fn icon() -> Option<std::path::PathBuf> {
    const ICON: &[u8] = include_bytes!("../../assets/reason.icns");
    let path = crate::paths::home().join("reason.icns");
    if std::fs::metadata(&path).ok().map(|m| m.len()) != Some(ICON.len() as u64) {
        std::fs::write(&path, ICON).ok()?;
    }
    Some(path)
}

/// A notification with buttons: notify-send prints the one clicked, or nothing if it was closed or expired.
#[cfg(all(unix, not(target_os = "macos")))]
fn dialog(title: &str, question: &str) -> Option<Command> {
    use super::notify::{markup, notify_send};
    let mut cmd = notify_send()?;
    cmd.args(["--app-name=Reason Network", "--wait", "--action=approve=Approve", "--action=deny=Deny"])
        .arg(format!("--expire-time={}", OPEN_SECS * 1000))
        .args(["--".into(), markup(title), markup(question)]);
    Some(cmd)
}

/// A toast with a note box and the two buttons (Return in the box approves); prints the button and the note.
#[cfg(windows)]
fn dialog(title: &str, question: &str) -> Option<Command> {
    const ASK: &str = r#"$ErrorActionPreference = 'Stop'
$null = [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime]
$null = [Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime]
$e = [Security.SecurityElement]
$x = [Windows.Data.Xml.Dom.XmlDocument]::new()
$x.LoadXml("<toast scenario='reminder'><visual><binding template='ToastGeneric'><text>$($e::Escape($env:TINCAN_TITLE))</text><text>$($e::Escape($env:TINCAN_BODY))</text></binding></visual><actions><input id='note' type='text' placeHolderContent='Note (optional)'/><action content='Approve' arguments='approve' hint-inputId='note'/><action content='Deny' arguments='deny'/></actions></toast>")
$t = [Windows.UI.Notifications.ToastNotification]::new($x)
$null = Register-ObjectEvent $t Activated -SourceIdentifier answered
$null = Register-ObjectEvent $t Dismissed -SourceIdentifier closed
$app = '{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe'
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier($app).Show($t)
$a = (Wait-Event -Timeout $env:TINCAN_SECS).SourceArgs[1] -as [Windows.UI.Notifications.ToastActivatedEventArgs]
if ($a) { $a.Arguments; $a.UserInput['note'] }"#;
    let mut cmd = super::notify::powershell(ASK)?;
    cmd.env("TINCAN_TITLE", title).env("TINCAN_BODY", question).env("TINCAN_SECS", OPEN_SECS.to_string());
    Some(cmd)
}
