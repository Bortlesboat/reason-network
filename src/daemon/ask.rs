//! `ask_user`: an agent asks its own person, with a pop-up on this machine, and gets the answer back as the tool's
//! result: their own decision, not a message from anyone. A panel with a note on macOS and Windows, a notification
//! with buttons on Linux.

use super::{Daemon, MAX_WAIT, log, mail::unregistered};
use crate::{
    proto::{Approval, HookCall, Message},
    team::Signed,
    util,
};
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
    /// The answer, and what to add to it for the asking agent (whether a signed answer went to a teammate).
    answer: Option<(Answer, String)>,
    /// Whether a waiting call has returned the answer.
    taken: bool,
}

impl Daemon {
    /// Asks this machine's person on behalf of the session `key`, and waits for the answer. A session has one
    /// question up at a time: asking it again waits for it, asking another is refused until it is answered. An answer
    /// that comes once no call waits for it reaches the session as a message from its person. `about`, a teammate's
    /// message to this session, is shown with the question, and its sender gets the answer signed by this machine.
    pub(super) async fn ask(self: &Arc<Self>, key: String, question: String, about: Option<String>) -> Result<String> {
        let (asker, stop, request) = {
            let g = self.inner.lock().unwrap();
            let l = g.live.get(&key).ok_or_else(unregistered)?;
            let asker = self.addr(&l.name);
            let request = match about {
                Some(id) => Some(
                    g.store
                        .message(&id)?
                        .filter(|m| m.to == asker && !m.from.starts_with(&format!("{}/", self.person)))
                        .ok_or_else(|| anyhow!("no message {id} from a teammate to this session"))?,
                ),
                None => None,
            };
            (asker, l.stop_gen, request)
        };
        // A teammate's request comes first, in their words (cut only where it ends in "…"), and the agent's question gets
        // the room left: the request the person sees is the one the signed answer says they saw.
        let request = request.map(|m| {
            let text = clip(&util::one_line(&m.body, 2000), 500);
            (m, text)
        });
        let question = if request.is_some() { clip(&question, 400) } else { question };
        let shown = match &request {
            Some((m, text)) => format!("{} asked: {text}\n\nYour agent asks: {question}", m.from),
            None => question.clone(),
        };
        {
            let mut asks = self.asks.lock().unwrap();
            match asks.get_mut(&key) {
                Some(up) if up.question != shown => bail!(
                    "your question \"{}\" is still up for {}: ask it again to keep waiting for its answer",
                    util::one_line(&up.question, 200),
                    self.person
                ),
                Some(up) => up.waiting += 1,
                None => {
                    let cmd = dialog(&format!("{asker} asks"), &shown).ok_or_else(|| {
                        anyhow!(
                            "{} has no screen to ask {} on: ask them in your own session",
                            self.machine,
                            self.person
                        )
                    })?;
                    let up = Asking { question: shown, waiting: 1, answer: None, taken: false };
                    asks.insert(key.clone(), up);
                    tokio::spawn(self.clone().pop_up(key.clone(), question, request, cmd));
                }
            }
        }
        let waiting = Waiting { daemon: self, key: &key };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(MAX_WAIT);
        loop {
            // The answer, and the client giving up on the call (`StopWaiting`), both wake `arrived`. Giving up is per
            // session: its other waits end too, and their answer comes as a message.
            let woke = self.arrived.notified();
            tokio::pin!(woke);
            woke.as_mut().enable();
            if let Some((answer, receipt)) = waiting.take() {
                return Ok(format!("{}{receipt}", self.said(&answer)?));
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

    /// Shows the question until it is answered or closes, then hands the answer to the calls waiting for it (and, for
    /// a teammate's request, to its sender).
    async fn pop_up(self: Arc<Self>, key: String, question: String, request: Option<(Message, String)>, cmd: Command) {
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
        // The teammate's answer goes first, so the calls waiting can say whether it went.
        let receipt = match &request {
            Some((m, text)) => self.send_approval(&key, m, text, &question, &answer).await,
            None => String::new(),
        };
        let waited = {
            let mut asks = self.asks.lock().unwrap();
            let Some(up) = asks.get_mut(&key) else { return };
            let waited = up.waiting > 0;
            if waited {
                up.answer = Some((answer.clone(), receipt));
            } else {
                asks.remove(&key);
            }
            waited
        };
        if waited {
            self.arrived.notify_waiters();
        } else {
            self.answer_later(&key, &question, &answer);
        }
    }

    /// The person's answer to a teammate's request, with what they saw, signed by this machine, to the session that sent
    /// the request: from the asking session, or this person's terminal if that has gone. Says whether it went.
    async fn send_approval(
        self: &Arc<Self>,
        key: &str,
        request: &Message,
        shown: &str,
        question: &str,
        answer: &Answer,
    ) -> String {
        let (approved, note) = match answer {
            Answer::Approved(n) => (true, n),
            Answer::Denied(n) => (false, n),
            _ => return String::new(),
        };
        let (Some(team), Ok(said)) = (self.team_state(), self.said(answer)) else { return String::new() };
        let approval = Approval {
            about: request.id.clone(),
            asker: request.from.clone(),
            approver: self.person.clone(),
            approved,
            question: question.to_string(),
            request: shown.to_string(),
            note: clip(note, 500),
            at: util::now_ms(),
        };
        let signed = Box::new(Signed::new(approval, &self.key, team.name(), "approval"));
        let live = self.inner.lock().unwrap().live.contains_key(key);
        let from = if live { key.to_string() } else { self.terminal().await };
        match self.send(from, &request.from, &said, Some(request.id.clone()), true, false, Some(signed)).await {
            Ok(_) => format!(" (Sent to {}, signed.)", request.from),
            Err(e) => {
                log(&format!("couldn't send {}'s answer to {}: {e:#}", self.person, request.from));
                format!(" (Couldn't send it to {}: {e:#}.)", request.from)
            }
        }
    }

    /// This person's own session (`<person>/terminal`), registered the way the CLI registers it.
    async fn terminal(self: &Arc<Self>) -> String {
        let call = HookCall {
            agent: "cli".into(),
            event: "stop".into(),
            session_id: "terminal".into(),
            cwd: None,
            pid: None,
            mail: false,
            transcript: None,
            doing: None,
            name: Some("terminal".into()),
        };
        if let Err(e) = self.hook(call).await {
            log(&format!("couldn't register {}/terminal: {e:#}", self.person));
        }
        "cli:terminal".into()
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
            approval: None,
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
    fn take(&self) -> Option<(Answer, String)> {
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
            && let Some((answer, _)) = up.answer
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
/// so nothing typed elsewhere reaches it; it gets keys once clicked, in the note box. Prints the button and the note;
/// nothing if it closed unanswered.
#[cfg(target_os = "macos")]
const PANEL: &str = r#"ObjC.import("Cocoa")
function run([title, question, secs, icon]) {
  const app = $.NSApplication.sharedApplication
  app.setActivationPolicy($.NSApplicationActivationPolicyAccessory)
  let choice = ""
  ObjC.registerSubclass({ name: "Answer", methods: {
    "approve:": { types: ["void", ["id"]], implementation: () => { choice = `approve\n${note.stringValue.js}`; app.stop(null) } },
    "deny:": { types: ["void", ["id"]], implementation: () => { choice = `deny\n${note.stringValue.js}`; app.stop(null) } },
  } })
  const answer = $.Answer.alloc.init
  const label = $.NSTextField.wrappingLabelWithString(question)
  label.preferredMaxLayoutWidth = 330
  // A question too long to show whole ends in "…" where the panel cuts it.
  for (let n = question.length; label.fittingSize.height > 320 && n > 0; n -= 20) label.stringValue = question.slice(0, n).trimEnd() + "…"
  const h = Math.max(label.fittingSize.height, 48)
  // Titled, non-activating: it doesn't take the keyboard from the app in front.
  const panel = $.NSPanel.alloc.initWithContentRectStyleMaskBackingDefer($.NSMakeRect(0, 0, 440, h + 96), 1 | 128, 2, false)
  panel.title = title
  panel.level = 8
  panel.becomesKeyOnlyIfNeeded = true
  panel.collectionBehavior = 1 | 256 // on every Space, and over full-screen apps
  const put = (view, x, y, w, vh) => { view.frame = $.NSMakeRect(x, y, w, vh); panel.contentView.addSubview(view) }
  put(label, 86, 84, 330, h)
  const image = icon && $.NSImage.alloc.initWithContentsOfFile(icon)
  if (image && !image.isNil()) put($.NSImageView.imageViewWithImage(image), 20, h + 28, 48, 48)
  const note = $.NSTextField.alloc.init
  note.placeholderString = "Note (optional)"
  note.target = answer
  note.action = "approve:"
  put(note, 86, 48, 330, 24)
  panel.initialFirstResponder = note
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
  return choice
}"#;

#[cfg(target_os = "macos")]
fn dialog(title: &str, question: &str) -> Option<Command> {
    let mut cmd = Command::new(asker_bin("osascript", &["/usr/bin/osascript"])?);
    let icon = icon().map(|p| p.display().to_string()).unwrap_or_default();
    cmd.args(["-l", "JavaScript", "-e", PANEL, title, &clip(question, 900), &OPEN_SECS.to_string(), &icon]);
    Some(cmd)
}

/// At most `max` characters of `text`, marked where it is cut.
fn clip(text: &str, max: usize) -> String {
    let t = util::printable(text).trim().to_string();
    if t.chars().count() <= max { t } else { format!("{}…", t.chars().take(max).collect::<String>().trim_end()) }
}

/// The program that shows the question: in a release build only the system's own, so an agent can't start this machine's
/// engine with one that answers for its person. (Debug builds take `REASON_NETWORK_<NAME>_BIN`, for tests.)
fn asker_bin(name: &str, system: &[&str]) -> Option<std::path::PathBuf> {
    if cfg!(debug_assertions) {
        return crate::agent::find_bin(name);
    }
    system.iter().map(std::path::PathBuf::from).find(|p| p.is_file())
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
    use super::notify::{markup, notify_send_at};
    let mut cmd =
        notify_send_at(asker_bin("notify-send", &["/usr/bin/notify-send", "/run/current-system/sw/bin/notify-send"])?)?;
    cmd.args(["--app-name=Reason Network", "--wait", "--action=approve=Approve", "--action=deny=Deny"])
        .arg(format!("--expire-time={}", OPEN_SECS * 1000))
        .args(["--".into(), markup(title), markup(question)]);
    Some(cmd)
}

/// The same panel as on macOS: shown on top without taking the keyboard (once clicked into, Return approves and Escape
/// denies); prints the button and the note.
#[cfg(windows)]
fn dialog(title: &str, question: &str) -> Option<Command> {
    const PANEL: &str = r#"$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
Add-Type -WarningAction SilentlyContinue -ReferencedAssemblies System.Windows.Forms -TypeDefinition 'public class AskPanel : System.Windows.Forms.Form { protected override bool ShowWithoutActivation { get { return true; } } }'
$f = New-Object AskPanel
$f.Text = $env:TINCAN_TITLE; $f.TopMost = $true; $f.StartPosition = 'CenterScreen'; $f.FormBorderStyle = 'FixedDialog'
$f.MinimizeBox = $false; $f.MaximizeBox = $false; $f.AutoSize = $true; $f.AutoSizeMode = 'GrowAndShrink'; $f.Padding = 12
$l = New-Object Windows.Forms.FlowLayoutPanel; $l.FlowDirection = 'TopDown'; $l.AutoSize = $true
$q = New-Object Windows.Forms.Label; $q.UseMnemonic = $false; $q.UseCompatibleTextRendering = $false; $q.Text = $env:TINCAN_BODY; $q.AutoSize = $true; $q.MaximumSize = '420,320'
# A question too long to show whole ends in "…" where the panel cuts it.
$s = $q.Text; while ([Windows.Forms.TextRenderer]::MeasureText($q.Text, $q.Font, '420,0', 'WordBreak').Height -gt 320 -and $s) { $s = $s.Substring(0, [Math]::Max(0, $s.Length - 20)).TrimEnd(); $q.Text = $s + [char]0x2026 }
$note = New-Object Windows.Forms.TextBox; $note.Width = 420
$b = New-Object Windows.Forms.FlowLayoutPanel; $b.FlowDirection = 'RightToLeft'; $b.Width = 420; $b.Height = 34
$yes = New-Object Windows.Forms.Button; $yes.Text = 'Approve'; $no = New-Object Windows.Forms.Button; $no.Text = 'Deny'
$b.Controls.AddRange(@($yes, $no)); $l.Controls.AddRange(@($q, $note, $b)); $f.Controls.Add($l)
$f.AcceptButton = $yes; $f.CancelButton = $no; $f.ActiveControl = $note
$script:choice = ''
$yes.Add_Click({ $script:choice = "approve`n" + $note.Text; $f.Close() })
$no.Add_Click({ $script:choice = "deny`n" + $note.Text; $f.Close() })
$t = New-Object Windows.Forms.Timer; $t.Interval = [int]$env:TINCAN_SECS * 1000; $t.Add_Tick({ $f.Close() }); $t.Start()
[Windows.Forms.Application]::Run($f)
$script:choice"#;
    let powershell = asker_bin("powershell", &[r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"])?;
    let mut cmd = super::notify::powershell_at(powershell, PANEL);
    cmd.env("TINCAN_TITLE", title).env("TINCAN_BODY", clip(question, 900)).env("TINCAN_SECS", OPEN_SECS.to_string());
    Some(cmd)
}
