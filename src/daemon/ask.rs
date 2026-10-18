//! `ask_user`: an agent asks its own person, with a pop-up on this machine, and gets the answer back as the tool's
//! result: their own decision, not a message from anyone. A panel with a note on macOS, a toast with one on Windows,
//! a notification with buttons on Linux.

use super::{Daemon, MAX_WAIT, log, mail::unregistered};
use crate::{proto::Message, util};
use anyhow::{Result, anyhow, bail};
use std::{process::Command, sync::Arc, time::Duration};
use ulid::Ulid;

/// A pop-up nobody answers closes after this.
const OPEN_SECS: u64 = 3600;

/// What came of a question.
#[derive(Clone)]
pub(super) enum Answer {
    Approved(String),
    Denied(String),
    /// Closed, or not answered in time.
    Unanswered,
    /// It couldn't be shown.
    Failed(String),
}

/// A session's question while it is up.
pub(super) struct Asking {
    question: String,
    /// Calls waiting for its answer now.
    waiting: usize,
    answer: Option<Answer>,
    /// Whether a waiting call has returned the answer.
    taken: bool,
}

impl Daemon {
    /// Asks this machine's person on behalf of the session `key`, and waits for the answer. A session has one
    /// question up at a time: asking it again waits for it, asking another is refused until it is answered. An answer
    /// that comes once no call waits for it reaches the session as a message from its person.
    pub(super) async fn ask(self: &Arc<Self>, key: String, question: String) -> Result<String> {
        let (asker, stop) = {
            let g = self.inner.lock().unwrap();
            let l = g.live.get(&key).ok_or_else(unregistered)?;
            (self.addr(&l.name), l.stop_gen)
        };
        {
            let mut asks = self.asks.lock().unwrap();
            match asks.get_mut(&key) {
                Some(up) if up.question != question => bail!(
                    "your question \"{}\" is still up for {}: ask it again to keep waiting for its answer",
                    util::one_line(&up.question, 200),
                    self.person
                ),
                Some(up) => up.waiting += 1,
                None => {
                    let cmd = dialog(&format!("{asker} asks"), &question).ok_or_else(|| {
                        anyhow!(
                            "{} has no screen to ask {} on: ask them in your own session",
                            self.machine,
                            self.person
                        )
                    })?;
                    let up = Asking { question: question.clone(), waiting: 1, answer: None, taken: false };
                    asks.insert(key.clone(), up);
                    tokio::spawn(self.clone().pop_up(key.clone(), question, cmd));
                }
            }
        }
        let waiting = Waiting { daemon: self, key: &key };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(MAX_WAIT);
        loop {
            // The answer, and the client giving up on the call (`StopWaiting`), both wake `arrived`.
            let woke = self.arrived.notified();
            tokio::pin!(woke);
            woke.as_mut().enable();
            if let Some(answer) = waiting.take() {
                return self.said(&answer);
            }
            let gave_up = self.inner.lock().unwrap().live.get(&key).is_none_or(|l| l.stop_gen != stop);
            if gave_up || tokio::time::Instant::now() >= deadline {
                return Ok(format!(
                    "{} hasn't answered yet; the pop-up stays up. Ask again to keep waiting: an answer after that \
                     comes to you as a message from them.",
                    self.person
                ));
            }
            let _ = tokio::time::timeout_at(deadline, woke).await;
        }
    }

    /// The answer, as the asking agent reads it.
    fn said(&self, answer: &Answer) -> Result<String> {
        let note = |n: &str| if n.is_empty() { String::new() } else { format!(" Their note: {n}") };
        Ok(match answer {
            Answer::Approved(n) => format!("{} approved.{}", self.person, note(n)),
            Answer::Denied(n) => format!("{} said no.{}", self.person, note(n)),
            Answer::Unanswered => format!("{} didn't answer.", self.person),
            Answer::Failed(e) => {
                bail!("couldn't ask {} with a pop-up ({e}): ask them in your own session", self.person)
            }
        })
    }

    /// Shows the question until it is answered or closes, then hands the answer to the calls waiting for it.
    async fn pop_up(self: Arc<Self>, key: String, question: String, cmd: Command) {
        let mut cmd = tokio::process::Command::from(cmd);
        let out = tokio::time::timeout(Duration::from_secs(OPEN_SECS + 60), cmd.kill_on_drop(true).output()).await;
        let answer = match out {
            Ok(Ok(o)) if o.status.success() => parse(&String::from_utf8_lossy(&o.stdout)),
            Ok(Ok(o)) => Answer::Failed(String::from_utf8_lossy(&o.stderr).trim().to_string()),
            Ok(Err(e)) => Answer::Failed(e.to_string()),
            Err(_) => Answer::Unanswered,
        };
        if let Answer::Failed(e) = &answer {
            log(&format!("pop-up failed: {e}"));
        }
        let mut asks = self.asks.lock().unwrap();
        let Some(up) = asks.get_mut(&key) else { return };
        if up.waiting > 0 {
            up.answer = Some(answer);
            drop(asks);
            self.arrived.notify_waiters();
        } else {
            asks.remove(&key);
            drop(asks);
            self.answer_later(&key, &question, &answer);
        }
    }

    /// An answer no call waited for any more: to the session, as a message from its person.
    fn answer_later(self: &Arc<Self>, key: &str, question: &str, answer: &Answer) {
        if !matches!(answer, Answer::Approved(_) | Answer::Denied(_)) {
            return;
        }
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

/// A call waiting for its session's question, counted for as long as it waits: when the last one leaves without the
/// answer, the answer goes to the session as a message instead.
struct Waiting<'a> {
    daemon: &'a Arc<Daemon>,
    key: &'a str,
}

impl Waiting<'_> {
    fn take(&self) -> Option<Answer> {
        let mut asks = self.daemon.asks.lock().unwrap();
        let up = asks.get_mut(self.key)?;
        up.taken |= up.answer.is_some();
        up.answer.clone()
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        let mut asks = self.daemon.asks.lock().unwrap();
        let Some(up) = asks.get_mut(self.key) else { return };
        up.waiting -= 1;
        if up.waiting > 0 || up.answer.is_none() {
            return;
        }
        let up = asks.remove(self.key).expect("just looked up");
        drop(asks);
        if !up.taken
            && let Some(answer) = up.answer
        {
            self.daemon.answer_later(self.key, &up.question, &answer);
        }
    }
}

/// `approve` or `deny` on the first line, the note after it; anything else is no answer.
fn parse(out: &str) -> Answer {
    let (choice, note) = out.trim().split_once('\n').unwrap_or((out.trim(), ""));
    let note = note.trim().to_string();
    match choice.trim().to_lowercase().as_str() {
        "approve" => Answer::Approved(note),
        "deny" => Answer::Denied(note),
        _ => Answer::Unanswered,
    }
}

/// A floating panel with a note box, Deny (Escape) and Approve (Return). It shows on top without taking the keyboard,
/// so nothing typed elsewhere reaches it; it gets keys once clicked. Prints the button and the note; nothing if it
/// closed unanswered.
#[cfg(target_os = "macos")]
const PANEL: &str = r#"ObjC.import("Cocoa")
function run([title, question, secs, icon]) {
  const app = $.NSApplication.sharedApplication
  app.setActivationPolicy($.NSApplicationActivationPolicyAccessory)
  let choice = ""
  ObjC.registerSubclass({ name: "Answer", methods: {
    "approve:": { types: ["void", ["id"]], implementation: () => { choice = "approve"; app.stop(null) } },
    "deny:": { types: ["void", ["id"]], implementation: () => { choice = "deny"; app.stop(null) } },
  } })
  const answer = $.Answer.alloc.init
  const label = $.NSTextField.wrappingLabelWithString(question)
  label.preferredMaxLayoutWidth = 330
  const h = Math.min(Math.max(label.fittingSize.height, 48), 320)
  // Titled, non-activating: it doesn't take the keyboard from the app in front.
  const panel = $.NSPanel.alloc.initWithContentRectStyleMaskBackingDefer($.NSMakeRect(0, 0, 440, h + 96), 1 | 128, 2, false)
  panel.title = title
  panel.level = 8
  panel.becomesKeyOnlyIfNeeded = true
  const put = (view, x, y, w, vh) => { view.frame = $.NSMakeRect(x, y, w, vh); panel.contentView.addSubview(view) }
  put(label, 86, 84, 330, h)
  const image = icon && $.NSImage.alloc.initWithContentsOfFile(icon)
  if (image && !image.isNil()) put($.NSImageView.imageViewWithImage(image), 20, h + 28, 48, 48)
  const note = $.NSTextField.alloc.init
  note.placeholderString = "Note (optional)"
  note.target = answer
  note.action = "approve:"
  put(note, 86, 48, 330, 24)
  const button = (name, x, action, key) => {
    const b = $.NSButton.buttonWithTitleTargetAction(name, answer, action)
    b.keyEquivalent = key
    put(b, x, 10, 96, 30)
  }
  button("Deny", 220, "deny:", "\u001b")
  button("Approve", 320, "approve:", "\r")
  panel.center
  panel.orderFrontRegardless
  $.NSTimer.scheduledTimerWithTimeIntervalTargetSelectorUserInfoRepeats(Number(secs), app, "stop:", null, false)
  app.run
  return choice && `${choice}\n${note.stringValue.js}`
}"#;

#[cfg(target_os = "macos")]
fn dialog(title: &str, question: &str) -> Option<Command> {
    let mut cmd = Command::new(crate::agent::find_bin("osascript")?);
    let question: String = util::printable(question).trim().chars().take(1500).collect();
    let icon = icon().map(|p| p.display().to_string()).unwrap_or_default();
    cmd.args(["-l", "JavaScript", "-e", PANEL, title, &question, &OPEN_SECS.to_string(), &icon]);
    Some(cmd)
}

/// Reason's icon for the panel, kept beside the network's database.
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
/// (`--wait` and `--action` need libnotify 0.7.10, from 2021; with an older one asking fails, and says so.)
#[cfg(all(unix, not(target_os = "macos")))]
fn dialog(title: &str, question: &str) -> Option<Command> {
    use super::notify::{markup, notify_send};
    let mut cmd = notify_send()?;
    cmd.args(["--app-name=Reason Network", "--wait", "--action=approve=Approve", "--action=deny=Deny"])
        .arg(format!("--expire-time={}", OPEN_SECS * 1000))
        .args(["--".into(), markup(title), markup(question)]);
    Some(cmd)
}

/// A toast with a note box and the two buttons (Return in the box approves); prints the button and the note. Taken
/// down if nobody answers in time.
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
$n = [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe')
$n.Show($t)
$ev = Wait-Event -Timeout $env:TINCAN_SECS
if (-not $ev) { $n.Hide($t) }
elseif ($a = $ev.SourceArgs[1] -as [Windows.UI.Notifications.ToastActivatedEventArgs]) { $a.Arguments; $a.UserInput['note'] }"#;
    let mut cmd = super::notify::powershell(ASK)?;
    cmd.env("TINCAN_TITLE", title).env("TINCAN_BODY", question).env("TINCAN_SECS", OPEN_SECS.to_string());
    Some(cmd)
}
