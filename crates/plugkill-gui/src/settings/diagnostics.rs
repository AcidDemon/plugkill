//! The diagnostics panel: the questions that cost an evening (G1 to G3).
//!
//! Every answer comes from what is already on this machine: the status the
//! poll loop fetched, the socket's own permissions, this process's groups,
//! the session environment and a walk of /proc. Nothing here talks to the
//! daemon, and nothing here writes.

use crate::settings::editor::EditorPanel;
use crate::settings::{Panel, PanelData};
use crate::status::{STALL_MS, Status, TrayState, format_uptime};
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Updates between two walks of /proc. The window updates about once a second;
/// a polkit agent started since it opened should turn up without a restart,
/// but not at the price of a walk every second on the GTK thread.
///
/// ponytail: the walk is still on the GTK thread, bounded by this and by
/// `Settings::update` skipping a hidden window, so it costs a burst every 15
/// updates while the window is open and nothing at all while it is not. Move
/// `read_env` onto the poll thread and carry the answer on `Update` if that
/// burst is ever felt.
const RESCAN_EVERY: u64 = 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Ok,
    /// Something to fix, and the check says what.
    Problem,
    /// We could not find out. Said as that, never guessed at.
    Unknown,
}

impl Verdict {
    /// The word in the panel and in the pasted report.
    fn word(self) -> &'static str {
        match self {
            Verdict::Ok => "ok",
            Verdict::Problem => "problem",
            Verdict::Unknown => "unknown",
        }
    }

    /// Which colour the word wears.
    fn css(self) -> &'static str {
        match self {
            Verdict::Ok => "ok",
            Verdict::Problem => "bad",
            Verdict::Unknown => "unsure",
        }
    }
}

/// One line of the panel: a question, a plain verdict, and where it fails the
/// one thing to do about it (G2).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Check {
    pub label: &'static str,
    pub verdict: Verdict,
    pub detail: String,
    pub fix: Option<String>,
}

fn ok(label: &'static str, detail: impl Into<String>) -> Check {
    Check {
        label,
        verdict: Verdict::Ok,
        detail: detail.into(),
        fix: None,
    }
}

fn problem(label: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Check {
    Check {
        label,
        verdict: Verdict::Problem,
        detail: detail.into(),
        fix: Some(fix.into()),
    }
}

fn unknown(label: &'static str, detail: impl Into<String>) -> Check {
    Check {
        label,
        verdict: Verdict::Unknown,
        detail: detail.into(),
        fix: None,
    }
}

/// Everything the checks are computed from. Gathered off the widgets so the
/// verdicts are a pure function of it and can be tested without a bus, a
/// socket or a display.
#[derive(Debug, Clone, Default, PartialEq)]
struct Facts {
    pub socket: String,
    /// The daemon answered the last status fetch.
    pub reachable: bool,
    /// It answers, but its poll loop has stopped.
    pub stalled: bool,
    /// The group that owns the socket, named where /etc/group names it.
    pub socket_group: Option<String>,
    /// Whether this process holds that group. None when either side could not
    /// be read.
    pub in_socket_group: Option<bool>,
    /// From the config the daemon says it loaded. None when it reports no path
    /// or the file could not be read.
    pub require_auth: Option<bool>,
    /// None when /proc could not be walked, which is "cannot tell".
    pub polkit_agent: Option<bool>,
    pub seat: Option<String>,
    pub session: Option<String>,
    /// The last status the poll loop got, for the fields it carries.
    pub status: Option<Status>,
}

/// The panel, top to bottom (G1). Pure: same facts, same lines.
fn checks(f: &Facts) -> Vec<Check> {
    vec![
        daemon(f),
        socket_group(f),
        require_auth(f),
        polkit_agent(f),
        seat(f),
        last_poll(f),
        uptime(f),
        config_path(f),
        daemon_version(f),
        ok("GUI version", env!("CARGO_PKG_VERSION")),
    ]
}

fn daemon(f: &Facts) -> Check {
    const LABEL: &str = "Daemon";
    if f.stalled {
        return problem(
            LABEL,
            format!("{} answers, but the poll loop has stopped", f.socket),
            "look at why it stopped: journalctl -u plugkill -n 50",
        );
    }
    if f.reachable {
        ok(LABEL, format!("answering on {}", f.socket))
    } else {
        problem(
            LABEL,
            format!("no answer on {}", f.socket),
            "start it: sudo systemctl start plugkill",
        )
    }
}

fn socket_group(f: &Facts) -> Check {
    const LABEL: &str = "Socket group";
    let Some(group) = &f.socket_group else {
        return unknown(LABEL, format!("cannot read {}", f.socket));
    };
    match f.in_socket_group {
        Some(true) => ok(LABEL, format!("you are in {group}, which owns the socket")),
        Some(false) => problem(
            LABEL,
            format!("you are not in {group}, which owns the socket"),
            format!("sudo usermod -aG {group} $USER, then log out and back in"),
        ),
        None => unknown(
            LABEL,
            format!("{group} owns the socket; your own groups are unreadable"),
        ),
    }
}

fn require_auth(f: &Facts) -> Check {
    const LABEL: &str = "Authentication";
    match f.require_auth {
        // Both settings are the maintainer's to make, so neither is a problem.
        Some(true) => ok(
            LABEL,
            "require_auth is on: disarm, learn, reload and allow ask polkit",
        ),
        Some(false) => ok(
            LABEL,
            "require_auth is off: every command runs unchallenged",
        ),
        None => unknown(LABEL, "the loaded config could not be read"),
    }
}

fn polkit_agent(f: &Facts) -> Check {
    const LABEL: &str = "Polkit agent";
    match (f.polkit_agent, f.require_auth) {
        (Some(true), _) => ok(LABEL, "one is running for your user"),
        // Without require_auth nothing ever prompts, so a missing agent costs
        // nothing and is not reported as a fault.
        (Some(false), Some(true)) => problem(
            LABEL,
            "none found, and require_auth is on, so gated commands will be refused",
            "start your desktop's polkit agent, or run gated commands as root",
        ),
        (Some(false), _) => ok(
            LABEL,
            "none found; nothing asks for one while require_auth is off",
        ),
        (None, _) => unknown(LABEL, "/proc could not be walked to look for one"),
    }
}

fn seat(f: &Facts) -> Check {
    const LABEL: &str = "Session seat";
    match (&f.seat, &f.session) {
        (Some(seat), _) => ok(LABEL, format!("this session is on {seat}")),
        (None, Some(id)) => problem(
            LABEL,
            format!("session {id} has no seat, so polkit has nowhere to prompt"),
            "run gated commands as root, or from a seated session",
        ),
        (None, None) => unknown(LABEL, "no logind session here: XDG_SESSION_ID is unset"),
    }
}

fn last_poll(f: &Facts) -> Check {
    const LABEL: &str = "Last poll";
    let Some(status) = &f.status else {
        return unknown(LABEL, "the daemon has not answered");
    };
    match status.last_poll_ms_ago {
        // The daemon stops counting while it is disarmed, so it reports none.
        None => unknown(LABEL, "the daemon reports none"),
        Some(ms) if ms > STALL_MS => problem(
            LABEL,
            format!(
                "{} ago, past the {} s a pass should ever take",
                format_uptime(ms / 1000),
                STALL_MS / 1000
            ),
            "look at why it stopped: journalctl -u plugkill -n 50",
        ),
        Some(ms) => ok(LABEL, format!("{ms} ms ago")),
    }
}

fn uptime(f: &Facts) -> Check {
    const LABEL: &str = "Uptime";
    match &f.status {
        Some(s) => ok(LABEL, format_uptime(s.uptime_secs)),
        None => unknown(LABEL, "the daemon has not answered"),
    }
}

fn config_path(f: &Facts) -> Check {
    const LABEL: &str = "Config path";
    match f.status.as_ref().map(|s| s.config_path.as_str()) {
        Some(path) if !path.is_empty() => ok(LABEL, path),
        // A daemon older than C1 does not report one, and a guess here is
        // exactly the guess this field exists to remove.
        Some(_) => unknown(LABEL, "this daemon does not report the file it loaded"),
        None => unknown(LABEL, "the daemon has not answered"),
    }
}

fn daemon_version(f: &Facts) -> Check {
    const LABEL: &str = "Daemon version";
    match f.status.as_ref().map(|s| s.version.as_str()) {
        Some(v) if !v.is_empty() => ok(LABEL, v),
        Some(_) => unknown(LABEL, "this daemon does not report its version"),
        None => unknown(LABEL, "the daemon has not answered"),
    }
}

/// The whole panel as text, for pasting into a bug report (G3).
fn report(checks: &[Check]) -> String {
    let mut out = String::from("plugkill diagnostics\n");
    for check in checks {
        out.push_str(&format!(
            "[{}] {}: {}\n",
            check.verdict.word(),
            check.label,
            check.detail
        ));
        if let Some(fix) = &check.fix {
            out.push_str(&format!("    fix: {fix}\n"));
        }
    }
    out
}

/// The gids this process holds, as /proc/self/status lists them: its real and
/// effective gid, then its supplementary groups.
fn parse_gids(status: &str) -> Vec<u32> {
    let mut gids = Vec::new();
    for line in status.lines() {
        let fields = match line.split_once(':') {
            // Gid is real, effective, saved and fs; the first two are what a
            // file check uses.
            Some(("Gid", rest)) => rest.split_whitespace().take(2).collect::<Vec<_>>(),
            Some(("Groups", rest)) => rest.split_whitespace().collect(),
            _ => continue,
        };
        gids.extend(fields.iter().filter_map(|f| f.parse::<u32>().ok()));
    }
    gids
}

/// The name of a gid, out of /etc/group. Only the name is wanted: it is what
/// the fix line has to tell a person to type.
fn group_name(etc_group: &str, gid: u32) -> Option<String> {
    etc_group.lines().find_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        // name:password:gid:members
        let found: u32 = fields.nth(1)?.parse().ok()?;
        (found == gid).then(|| name.to_string())
    })
}

/// Whether a process name is a polkit authentication agent.
///
/// /proc rather than the bus: the agent GNOME ships lives inside the shell and
/// owns no bus name of its own, so a name check would call it missing. polkitd
/// is the daemon being asked, not an agent, and polkit-agent-helper-1 is what
/// polkitd spawns mid-prompt, so neither counts.
fn is_agent_comm(comm: &str) -> bool {
    let comm = comm.trim();
    if comm == "polkitd" || comm.starts_with("polkit-agent-he") {
        return false;
    }
    comm.contains("polkit") || comm.contains("policykit") || comm == "gnome-shell"
}

/// What does not come from the daemon. Read fresh every `RESCAN_EVERY`
/// updates, because an agent can start while the window is open.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Env {
    socket_group: Option<String>,
    in_socket_group: Option<bool>,
    polkit_agent: Option<bool>,
    seat: Option<String>,
    session: Option<String>,
}

fn read_env(socket: &Path) -> Env {
    let gid = std::fs::metadata(socket).ok().map(|m| m.gid());
    let mine = std::fs::read_to_string("/proc/self/status")
        .ok()
        .map(|s| parse_gids(&s));
    Env {
        socket_group: gid.map(|gid| {
            std::fs::read_to_string("/etc/group")
                .ok()
                .and_then(|text| group_name(&text, gid))
                .unwrap_or_else(|| format!("gid {gid}"))
        }),
        in_socket_group: gid.zip(mine).map(|(gid, mine)| mine.contains(&gid)),
        polkit_agent: polkit_agent_running(),
        seat: env_var("XDG_SEAT"),
        session: env_var("XDG_SESSION_ID"),
    }
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Whether this user runs a polkit agent. None where /proc cannot be walked,
/// which is an answer of "cannot tell" rather than "no".
fn polkit_agent_running() -> Option<bool> {
    let me = std::fs::metadata("/proc/self").ok()?.uid();
    let mut found = false;
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let path = entry.path();
        // Another user's agent cannot prompt for this session.
        if std::fs::metadata(&path).map(|m| m.uid()).ok() != Some(me) {
            continue;
        }
        if std::fs::read_to_string(path.join("comm")).is_ok_and(|c| is_agent_comm(&c)) {
            found = true;
            break;
        }
    }
    Some(found)
}

/// The three labels one check writes to. Its name is not among them: the list
/// of questions is fixed, so `new` sets that once.
type Line = (gtk::Label, gtk::Label, gtk::Label);

pub struct DiagnosticsPanel {
    root: gtk::Box,
    /// One per check, in `checks` order and never resized: the list of
    /// questions is fixed, only the answers move.
    lines: Vec<Line>,
    /// The class each verdict label wears now, so it can be taken off again.
    classes: RefCell<Vec<&'static str>>,
    socket: PathBuf,
    /// The one reader of the daemon's config file (C1, C2). Asking it here
    /// is a field read; reading and parsing the file a second time would not
    /// be.
    editor: Rc<EditorPanel>,
    status: RefCell<Option<Status>>,
    env: RefCell<Env>,
    ticks: Cell<u64>,
    /// What the copy button puts on the clipboard.
    last: RefCell<Vec<Check>>,
}

impl DiagnosticsPanel {
    pub fn new(socket: PathBuf, editor: Rc<EditorPanel>) -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 10);

        let title = gtk::Label::builder()
            .label("DIAGNOSTICS")
            .xalign(0.0)
            .build();
        title.add_css_class("section");
        root.append(&title);

        let well = gtk::Box::new(gtk::Orientation::Vertical, 6);
        well.add_css_class("well");
        let blank = Facts::default();
        let mut lines = Vec::new();
        for check in checks(&blank) {
            let tile = gtk::Box::new(gtk::Orientation::Vertical, 2);
            tile.add_css_class("tile");

            let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let label = gtk::Label::builder().label(check.label).xalign(0.0).build();
            label.add_css_class("tile-name");
            // A fixed column, so the labels beside the longest word still line
            // up with the labels beside the shortest.
            let verdict = gtk::Label::builder().xalign(0.0).width_chars(8).build();
            verdict.add_css_class("verdict");
            let detail = gtk::Label::builder().xalign(0.0).wrap(true).build();
            detail.set_hexpand(true);
            head.append(&verdict);
            head.append(&label);
            head.append(&detail);
            tile.append(&head);

            let fix = gtk::Label::builder().xalign(0.0).wrap(true).build();
            fix.add_css_class("fix");
            tile.append(&fix);

            well.append(&tile);
            lines.push((verdict, detail, fix));
        }
        root.append(&well);

        let foot = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        foot.add_css_class("foot");
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        foot.append(&spacer);
        let copy = gtk::Button::with_label("Copy for a bug report");
        foot.append(&copy);
        root.append(&foot);

        let panel = Rc::new(Self {
            root,
            lines,
            classes: RefCell::new(vec!["unsure"; checks(&blank).len()]),
            socket,
            status: RefCell::new(None),
            editor,
            env: RefCell::new(Env::default()),
            ticks: Cell::new(0),
            last: RefCell::new(Vec::new()),
        });
        {
            let panel = panel.clone();
            copy.connect_clicked(move |_| panel.copy());
        }
        panel
    }

    fn copy(&self) {
        if let Some(display) = gtk::gdk::Display::default() {
            display.clipboard().set_text(&report(&self.last.borrow()));
        }
    }

    fn facts(&self, data: &PanelData) -> Facts {
        if let Some(status) = data.status {
            self.status.replace(Some(status.clone()));
        }
        let status = self.status.borrow().clone();

        let ticks = self.ticks.get();
        self.ticks.set(ticks + 1);
        if ticks.is_multiple_of(RESCAN_EVERY) {
            self.env.replace(read_env(&self.socket));
        }
        let env = self.env.borrow().clone();

        Facts {
            socket: self.socket.display().to_string(),
            reachable: !matches!(data.state, TrayState::Down { stalled: false }),
            stalled: matches!(data.state, TrayState::Down { stalled: true }),
            socket_group: env.socket_group,
            in_socket_group: env.in_socket_group,
            require_auth: self.editor.require_auth(),
            polkit_agent: env.polkit_agent,
            seat: env.seat,
            session: env.session,
            status,
        }
    }
}

impl Panel for DiagnosticsPanel {
    fn title(&self) -> &'static str {
        "Diagnostics"
    }

    fn widget(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }

    fn update(&self, data: &PanelData) {
        let checks = checks(&self.facts(data));
        let mut classes = self.classes.borrow_mut();
        for (i, ((verdict, detail, fix), check)) in self.lines.iter().zip(&checks).enumerate() {
            verdict.set_label(check.verdict.word());
            verdict.remove_css_class(classes[i]);
            verdict.add_css_class(check.verdict.css());
            classes[i] = check.verdict.css();
            detail.set_label(&check.detail);
            fix.set_label(check.fix.as_deref().unwrap_or_default());
            fix.set_visible(check.fix.is_some());
        }
        drop(classes);
        self.last.replace(checks);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find<'a>(checks: &'a [Check], label: &str) -> &'a Check {
        checks
            .iter()
            .find(|c| c.label == label)
            .expect("the panel has that line")
    }

    fn facts() -> Facts {
        Facts {
            socket: "/tmp/plugkill.sock".into(),
            reachable: true,
            socket_group: Some("plugkill".into()),
            in_socket_group: Some(true),
            require_auth: Some(true),
            polkit_agent: Some(true),
            seat: Some("seat0".into()),
            session: Some("3".into()),
            status: Some(Status {
                uptime_secs: 3_700,
                last_poll_ms_ago: Some(420),
                config_path: "/etc/plugkill.toml".into(),
                version: "0.1.0".into(),
                ..Status::default()
            }),
            ..Facts::default()
        }
    }

    #[test]
    fn test_a_healthy_machine_has_nothing_to_fix() {
        let checks = checks(&facts());
        assert!(
            checks.iter().all(|c| c.verdict == Verdict::Ok),
            "{:?}",
            checks
                .iter()
                .filter(|c| c.verdict != Verdict::Ok)
                .collect::<Vec<_>>()
        );
        assert!(checks.iter().all(|c| c.fix.is_none()));
        assert_eq!(find(&checks, "Uptime").detail, "1h 1m");
        assert_eq!(find(&checks, "Last poll").detail, "420 ms ago");
        assert_eq!(find(&checks, "Config path").detail, "/etc/plugkill.toml");
        assert_eq!(find(&checks, "Daemon version").detail, "0.1.0");
        assert_eq!(
            find(&checks, "GUI version").detail,
            env!("CARGO_PKG_VERSION")
        );
    }

    #[test]
    fn test_nothing_answering_names_the_socket_and_says_what_to_run() {
        let f = Facts {
            reachable: false,
            status: None,
            ..facts()
        };
        let checks = checks(&f);
        let daemon = find(&checks, "Daemon");
        assert_eq!(daemon.verdict, Verdict::Problem);
        assert!(daemon.detail.contains("/tmp/plugkill.sock"));
        assert!(daemon.fix.as_deref().unwrap().contains("systemctl start"));
        // Everything the status would have carried is unknown, not invented.
        for label in ["Last poll", "Uptime", "Config path", "Daemon version"] {
            assert_eq!(find(&checks, label).verdict, Verdict::Unknown, "{label}");
        }
    }

    #[test]
    fn test_a_stalled_daemon_is_not_reported_as_absent() {
        let f = Facts {
            stalled: true,
            ..facts()
        };
        let daemon = find(&checks(&f), "Daemon").clone();
        assert_eq!(daemon.verdict, Verdict::Problem);
        assert!(daemon.detail.contains("poll loop has stopped"));
        assert!(daemon.fix.as_deref().unwrap().contains("journalctl"));
    }

    #[test]
    fn test_being_out_of_the_socket_group_says_which_group() {
        let f = Facts {
            in_socket_group: Some(false),
            ..facts()
        };
        let check = find(&checks(&f), "Socket group").clone();
        assert_eq!(check.verdict, Verdict::Problem);
        assert!(
            check
                .fix
                .as_deref()
                .unwrap()
                .contains("usermod -aG plugkill")
        );
    }

    #[test]
    fn test_an_unreadable_socket_is_unknown_not_a_refusal() {
        let f = Facts {
            socket_group: None,
            in_socket_group: None,
            ..facts()
        };
        assert_eq!(
            find(&checks(&f), "Socket group").verdict,
            Verdict::Unknown,
            "cannot tell is said, never guessed"
        );
    }

    #[test]
    fn test_require_auth_is_reported_either_way_and_neither_is_a_fault() {
        for on in [true, false] {
            let f = Facts {
                require_auth: Some(on),
                ..facts()
            };
            assert_eq!(find(&checks(&f), "Authentication").verdict, Verdict::Ok);
        }
        let f = Facts {
            require_auth: None,
            ..facts()
        };
        assert_eq!(
            find(&checks(&f), "Authentication").verdict,
            Verdict::Unknown
        );
    }

    #[test]
    fn test_a_missing_agent_only_matters_while_require_auth_is_on() {
        let on = Facts {
            polkit_agent: Some(false),
            require_auth: Some(true),
            ..facts()
        };
        let check = find(&checks(&on), "Polkit agent").clone();
        assert_eq!(check.verdict, Verdict::Problem);
        assert!(check.fix.is_some());

        let off = Facts {
            polkit_agent: Some(false),
            require_auth: Some(false),
            ..facts()
        };
        assert_eq!(find(&checks(&off), "Polkit agent").verdict, Verdict::Ok);

        let cannot_tell = Facts {
            polkit_agent: None,
            ..facts()
        };
        assert_eq!(
            find(&checks(&cannot_tell), "Polkit agent").verdict,
            Verdict::Unknown
        );
    }

    #[test]
    fn test_a_seatless_session_is_a_problem_and_no_session_is_unknown() {
        let ssh = Facts {
            seat: None,
            session: Some("7".into()),
            ..facts()
        };
        assert_eq!(
            find(&checks(&ssh), "Session seat").verdict,
            Verdict::Problem
        );

        let none = Facts {
            seat: None,
            session: None,
            ..facts()
        };
        assert_eq!(
            find(&checks(&none), "Session seat").verdict,
            Verdict::Unknown
        );
    }

    #[test]
    fn test_a_poll_older_than_the_stall_window_is_a_problem() {
        let mut f = facts();
        f.status.as_mut().unwrap().last_poll_ms_ago = Some(STALL_MS + 1);
        assert_eq!(find(&checks(&f), "Last poll").verdict, Verdict::Problem);

        // A disarmed daemon stops counting, and that is not a fault.
        let mut f = facts();
        f.status.as_mut().unwrap().last_poll_ms_ago = None;
        assert_eq!(find(&checks(&f), "Last poll").verdict, Verdict::Unknown);
    }

    #[test]
    fn test_a_daemon_that_predates_the_fields_says_so_rather_than_guessing() {
        let mut f = facts();
        let status = f.status.as_mut().unwrap();
        status.config_path = String::new();
        status.version = String::new();
        let checks = checks(&f);
        assert_eq!(find(&checks, "Config path").verdict, Verdict::Unknown);
        assert_eq!(find(&checks, "Daemon version").verdict, Verdict::Unknown);
    }

    #[test]
    fn test_the_report_carries_every_line_and_its_fix() {
        let f = Facts {
            reachable: false,
            ..facts()
        };
        let checks = checks(&f);
        let text = report(&checks);
        assert!(text.starts_with("plugkill diagnostics\n"));
        assert_eq!(
            text.lines().filter(|l| l.starts_with('[')).count(),
            checks.len(),
            "one line per check"
        );
        assert!(text.contains("[problem] Daemon: no answer on /tmp/plugkill.sock"));
        assert!(text.contains("    fix: start it: sudo systemctl start plugkill"));
    }

    #[test]
    fn test_gids_come_off_the_status_file() {
        let status = "Name:\tplugkill-gui\nGid:\t1000\t1000\t1000\t1000\nGroups:\t27 100 989 \n";
        assert_eq!(parse_gids(status), vec![1000, 1000, 27, 100, 989]);
        assert!(parse_gids("Name:\tnothing\n").is_empty());
    }

    #[test]
    fn test_a_group_is_named_by_its_gid() {
        let group = "root:x:0:\nplugkill:x:989:acid\nwheel:x:998:acid\n";
        assert_eq!(group_name(group, 989).as_deref(), Some("plugkill"));
        assert_eq!(group_name(group, 0).as_deref(), Some("root"));
        assert!(group_name(group, 1234).is_none());
    }

    #[test]
    fn test_an_agent_is_recognised_by_its_process_name() {
        for comm in [
            "polkit-gnome-au",
            "polkit-kde-auth",
            "polkit-mate-aut",
            "lxpolkit",
            "lxqt-policykit",
            "hyprpolkitagent",
            // GNOME's agent is the shell itself: it owns no name of its own.
            "gnome-shell",
        ] {
            assert!(is_agent_comm(&format!("{comm}\n")), "{comm}");
        }
        // The daemon being asked, and the helper it spawns mid-prompt.
        assert!(!is_agent_comm("polkitd\n"));
        assert!(!is_agent_comm("polkit-agent-he\n"));
        assert!(!is_agent_comm("plugkill\n"));
    }
}
