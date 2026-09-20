//! Answering one question: may this caller run this action?
//!
//! The daemon asks before it runs a gated command. The real answer comes from
//! polkit over the system bus, with the caller's logind session as the subject.
//! It sits behind a trait so tests can answer without a bus or an agent.

/// Polkit action ids for the gated commands.
pub const ACTION_DISARM: &str = "net.acidnetworks.plugkill.disarm";
pub const ACTION_LEARN: &str = "net.acidnetworks.plugkill.learn";
pub const ACTION_RELOAD: &str = "net.acidnetworks.plugkill.reload";
/// Granting a runtime allowance: pairing a device, or promoting the last
/// violation. Revoking one is never gated.
pub const ACTION_ALLOW: &str = "net.acidnetworks.plugkill.allow";

/// Said where polkit is not the authority, which is every platform but Linux.
const REASON_NO_POLKIT: &str = "polkit is not available on this platform";

/// The answer. Anything but `Allowed` is a refusal carrying a reason (B6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Authorization {
    Allowed,
    Refused(String),
}

impl Authorization {
    /// Text for the journal and for the error the client sees.
    pub fn reason(&self) -> &str {
        match self {
            Authorization::Allowed => "allowed",
            Authorization::Refused(reason) => reason,
        }
    }
}

/// Answers whether a caller may run an action. Both the uid and the pid come
/// from `SO_PEERCRED`, never from anything the client said (A2).
pub trait Authority: Send + Sync {
    fn check(&self, action: &str, caller_uid: u32, caller_pid: u32) -> Authorization;
}

/// The real thing: polkit on the system bus.
pub struct PolkitAuthority;

impl Authority for PolkitAuthority {
    fn check(&self, action: &str, caller_uid: u32, caller_pid: u32) -> Authorization {
        #[cfg(target_os = "linux")]
        {
            linux::check(action, caller_uid, caller_pid)
        }
        // The check is compiled Linux-only, so every gated command is refused
        // off Linux (D4). On FreeBSD the peer-credentials sockopt carries no
        // pid either, so there would be no subject to ask about.
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (action, caller_uid, caller_pid);
            no_polkit()
        }
    }
}

// On Linux only the tests reach this.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn no_polkit() -> Authorization {
    Authorization::Refused(REASON_NO_POLKIT.to_string())
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    /// Why a check was refused. Each reason names which failure it was, so
    /// the journal answers "why did that disarm not go through" after the
    /// fact. They live here because polkit does: no other platform produces
    /// one of them.
    const REASON_DENIED: &str = "polkit denied the action";
    const REASON_NO_BUS: &str = "no system bus";
    const REASON_NO_SESSION: &str = "caller has no logind session";
    /// The pid has no session of its own and the uid has no display session
    /// either, so there is no subject to ask about (A4b). Distinct from
    /// `REASON_NO_SESSION`, which is logind or the bus failing to answer.
    const REASON_NO_DISPLAY_SESSION: &str = "caller has no logind session and no display session";
    const REASON_NO_AUTHORITY: &str = "polkit authority did not answer";
    const REASON_MALFORMED: &str = "polkit reply was malformed";
    const REASON_TIMEOUT: &str = "polkit did not answer in time";

    use std::collections::HashMap;
    use std::io;
    use std::time::Duration;
    use zbus::blocking::Connection;
    use zbus::blocking::connection::Builder;
    use zbus::zvariant::{OwnedObjectPath, OwnedValue, Structure, Value};

    /// The call blocks while the person types their password, so about 120 s
    /// (B4). Short of the client's own wait on purpose: zbus starts this clock
    /// after the bus handshake, so an equal value would expire after the client
    /// had already given up and the user would see a transport error instead of
    /// the refusal. zbus only takes this per connection, so we build our own.
    const METHOD_TIMEOUT: Duration = Duration::from_secs(110);

    /// AllowUserInteraction, so the session's agent prompts (B3).
    const ALLOW_USER_INTERACTION: u32 = 1;

    /// logind's error names we act on rather than just report.
    const ERR_NO_SESSION_FOR_PID: &str = "org.freedesktop.login1.NoSessionForPID";
    const ERR_NO_SUCH_USER: &str = "org.freedesktop.login1.NoSuchUser";

    pub(super) fn check(action: &str, caller_uid: u32, caller_pid: u32) -> Authorization {
        let conn = match Builder::system().and_then(|b| b.method_timeout(METHOD_TIMEOUT).build()) {
            Ok(conn) => conn,
            Err(e) => return Authorization::Refused(format!("{REASON_NO_BUS}: {e}")),
        };

        let session_id = match subject_session(&conn, caller_uid, caller_pid) {
            Ok(id) => id,
            Err(refused) => return refused,
        };

        // Subject is the session, not the process: polkit's unix-process subject
        // is (pid, start-time) and has a pid-reuse race with a CVE history (A3).
        let mut subject_details: HashMap<&str, Value<'_>> = HashMap::new();
        subject_details.insert("session-id", Value::from(session_id.as_str()));
        let subject = ("unix-session", subject_details);
        let details: HashMap<&str, &str> = HashMap::new();

        let reply = conn.call_method(
            Some("org.freedesktop.PolicyKit1"),
            "/org/freedesktop/PolicyKit1/Authority",
            Some("org.freedesktop.PolicyKit1.Authority"),
            "CheckAuthorization",
            &(subject, action, details, ALLOW_USER_INTERACTION, ""),
        );

        let reply = match reply {
            Ok(reply) => reply,
            Err(e) => return refusal(REASON_NO_AUTHORITY, &e),
        };

        // (bool is_authorized, bool is_challenge, a{ss} details)
        match reply
            .body()
            .deserialize::<(bool, bool, HashMap<String, String>)>()
        {
            Ok((is_authorized, _, _)) => from_check_reply(is_authorized),
            Err(e) => refusal(REASON_MALFORMED, &e),
        }
    }

    /// The session handed to polkit. A pid started by the systemd user manager
    /// lives in `user@<uid>.service` and has no session of its own, so logind
    /// answers NoSessionForPID and we fall back to where that uid is sitting
    /// (A4, A4a). A caller with neither has no subject and is refused (A4b).
    fn subject_session(conn: &Connection, uid: u32, pid: u32) -> Result<String, Authorization> {
        match route_pid_lookup(session_for_pid(conn, pid)) {
            Ok(path) => {
                // GetSessionByPID pins nothing but the pid, so a pid reused
                // since SO_PEERCRED read it would name a stranger's session.
                let owner =
                    session_uid(conn, path.as_str()).map_err(|e| refusal(REASON_NO_SESSION, &e))?;
                if owner != uid {
                    return Err(Authorization::Refused(REASON_NO_SESSION.to_string()));
                }
                session_id(conn, path.as_str()).map_err(|e| refusal(REASON_NO_SESSION, &e))
            }
            Err(None) => {
                log::info!("pid {pid} has no session, asking the display session of uid {uid}");
                display_session(conn, uid)
            }
            Err(Some(refused)) => Err(refused),
        }
    }

    /// "No session of its own", which falls back, against a real failure, which
    /// refuses. Split from the bus call so a test can reach it.
    fn route_pid_lookup<T>(found: Result<T, zbus::Error>) -> Result<T, Option<Authorization>> {
        match found {
            Ok(found) => Ok(found),
            Err(e) if is_login_error(&e, ERR_NO_SESSION_FOR_PID) => Err(None),
            Err(e) => Err(Some(refusal(REASON_NO_SESSION, &e))),
        }
    }

    /// True for a named logind error, so the caller can act on it instead of
    /// just reporting it.
    fn is_login_error(err: &zbus::Error, name: &str) -> bool {
        matches!(err, zbus::Error::MethodError(n, _, _) if n.as_str() == name)
    }

    fn session_for_pid(conn: &Connection, pid: u32) -> Result<OwnedObjectPath, zbus::Error> {
        let reply = conn.call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "GetSessionByPID",
            &(pid,),
        )?;
        reply.body().deserialize()
    }

    fn session_id(conn: &Connection, path: &str) -> Result<String, zbus::Error> {
        let id = property(conn, path, "org.freedesktop.login1.Session", "Id")?;
        String::try_from(id).map_err(zbus::Error::Variant)
    }

    /// The uid a session belongs to, so the subject can be pinned to the caller.
    fn session_uid(conn: &Connection, path: &str) -> Result<u32, zbus::Error> {
        user_uid(property(
            conn,
            path,
            "org.freedesktop.login1.Session",
            "User",
        )?)
    }

    /// `User` is a (uid, object path) pair.
    fn user_uid(user: OwnedValue) -> Result<u32, zbus::Error> {
        let pair = Structure::try_from(user).map_err(zbus::Error::Variant)?;
        pair.fields()
            .first()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or(zbus::Error::Variant(zbus::zvariant::Error::IncorrectType))
    }

    /// The caller's own display session, from `User.Display` for the uid the
    /// socket read from SO_PEERCRED. The prompt then appears where that user
    /// is sitting.
    fn display_session(conn: &Connection, uid: u32) -> Result<String, Authorization> {
        let no_user = |e: &zbus::Error| {
            if is_login_error(e, ERR_NO_SUCH_USER) {
                Authorization::Refused(REASON_NO_DISPLAY_SESSION.to_string())
            } else {
                refusal(REASON_NO_SESSION, e)
            }
        };

        let reply = conn
            .call_method(
                Some("org.freedesktop.login1"),
                "/org/freedesktop/login1",
                Some("org.freedesktop.login1.Manager"),
                "GetUser",
                &(uid,),
            )
            .map_err(|e| no_user(&e))?;
        let path: OwnedObjectPath = reply
            .body()
            .deserialize()
            .map_err(|e| refusal(REASON_MALFORMED, &e))?;

        let display = property(
            conn,
            path.as_str(),
            "org.freedesktop.login1.User",
            "Display",
        )
        .map_err(|e| no_user(&e))?;
        display_session_id(display)
    }

    /// `Display` is a (session id, object path) pair. An empty id is a user
    /// with no display session at all.
    fn display_session_id(display: OwnedValue) -> Result<String, Authorization> {
        let malformed =
            |what: String| Authorization::Refused(format!("{REASON_MALFORMED}: {what}"));
        let pair = Structure::try_from(display).map_err(|e| malformed(e.to_string()))?;
        let id = pair
            .fields()
            .first()
            .and_then(|v| <&str>::try_from(v).ok())
            .ok_or_else(|| malformed("Display is not a (session id, path) pair".to_string()))?;
        if id.is_empty() {
            return Err(Authorization::Refused(
                REASON_NO_DISPLAY_SESSION.to_string(),
            ));
        }
        Ok(id.to_string())
    }

    fn property(
        conn: &Connection,
        path: &str,
        interface: &str,
        name: &str,
    ) -> Result<OwnedValue, zbus::Error> {
        let reply = conn.call_method(
            Some("org.freedesktop.login1"),
            path,
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &(interface, name),
        )?;
        reply.body().deserialize()
    }

    /// Only an explicit "authorized" is an allow.
    fn from_check_reply(is_authorized: bool) -> Authorization {
        if is_authorized {
            Authorization::Allowed
        } else {
            Authorization::Refused(REASON_DENIED.to_string())
        }
    }

    /// Every bus failure is a refusal (B6). `stage` says which call failed;
    /// a timeout or a reply we cannot read names itself instead.
    fn refusal(stage: &str, err: &zbus::Error) -> Authorization {
        let reason = match err {
            zbus::Error::InputOutput(io) if io.kind() == io::ErrorKind::TimedOut => REASON_TIMEOUT,
            zbus::Error::Variant(_) | zbus::Error::InvalidReply => REASON_MALFORMED,
            _ => stage,
        };
        Authorization::Refused(format!("{reason}: {err}"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::Arc;

        #[test]
        fn authorized_reply_allows_and_anything_else_refuses() {
            assert_eq!(from_check_reply(true), Authorization::Allowed);
            assert_eq!(
                from_check_reply(false),
                Authorization::Refused(REASON_DENIED.to_string())
            );
        }

        /// The client waits `AUTH_READ_TIMEOUT` for an answer, so this call has
        /// to give up first or an unanswered prompt reaches the user as a
        /// transport error instead of "authentication required".
        #[test]
        fn we_give_up_before_the_client_does() {
            assert!(METHOD_TIMEOUT < crate::ipc::AUTH_READ_TIMEOUT);
        }

        #[test]
        fn a_timeout_is_a_refusal_that_says_so() {
            let err = zbus::Error::InputOutput(Arc::new(io::Error::new(
                io::ErrorKind::TimedOut,
                "no reply",
            )));
            let refused = refusal(REASON_NO_AUTHORITY, &err);
            assert_ne!(refused, Authorization::Allowed);
            assert!(refused.reason().starts_with(REASON_TIMEOUT));
        }

        #[test]
        fn an_unreadable_reply_is_a_malformed_refusal() {
            let err = zbus::Error::Variant(zbus::zvariant::Error::IncorrectType);
            assert!(
                refusal(REASON_NO_AUTHORITY, &err)
                    .reason()
                    .starts_with(REASON_MALFORMED)
            );
            assert!(
                refusal(REASON_NO_SESSION, &zbus::Error::InvalidReply)
                    .reason()
                    .starts_with(REASON_MALFORMED)
            );
        }

        /// No bus, no logind: the pieces are built by hand.
        fn method_error(name: &str) -> zbus::Error {
            let reply = zbus::Message::method_call("/p", "M")
                .unwrap()
                .build(&())
                .unwrap();
            zbus::Error::MethodError(name.try_into().unwrap(), None, reply)
        }

        fn display_pair(id: &str) -> OwnedValue {
            let path = OwnedObjectPath::try_from("/org/freedesktop/login1/session/_33").unwrap();
            OwnedValue::try_from(Value::from((id.to_string(), path))).unwrap()
        }

        /// A tray started by the systemd user manager has no session of its
        /// own, and that answer has to reach the display-session fallback
        /// instead of ending the check.
        #[test]
        fn no_session_for_pid_routes_to_the_display_session() {
            assert_eq!(
                route_pid_lookup::<String>(Err(method_error(ERR_NO_SESSION_FOR_PID))),
                Err(None)
            );
            // Anything else from logind is a refusal that names the stage.
            let other = route_pid_lookup::<String>(Err(zbus::Error::Failure("gone".to_string())))
                .unwrap_err()
                .expect("anything but NoSessionForPID refuses");
            assert!(other.reason().starts_with(REASON_NO_SESSION));
            assert_eq!(route_pid_lookup(Ok("3")), Ok("3"));
        }

        /// The literals are what logind sends; a typo here is a fallback that
        /// never fires, so they are pinned by value and not by themselves.
        #[test]
        fn the_logind_error_names_are_the_ones_logind_sends() {
            assert_eq!(
                ERR_NO_SESSION_FOR_PID,
                "org.freedesktop.login1.NoSessionForPID"
            );
            assert_eq!(ERR_NO_SUCH_USER, "org.freedesktop.login1.NoSuchUser");
        }

        /// A session's owner has to be readable, or the pid path cannot be
        /// pinned to the caller and everything is refused.
        #[test]
        fn a_session_user_pair_reads_back_its_uid() {
            let path = OwnedObjectPath::try_from("/org/freedesktop/login1/user/_1000").unwrap();
            let user = OwnedValue::try_from(Value::from((1000u32, path))).unwrap();
            assert_eq!(user_uid(user).unwrap(), 1000);
            assert!(user_uid(OwnedValue::try_from(Value::from(3u32)).unwrap()).is_err());
        }

        #[test]
        fn a_user_with_no_display_session_is_refused_by_name() {
            assert_eq!(
                display_session_id(display_pair("")),
                Err(Authorization::Refused(
                    REASON_NO_DISPLAY_SESSION.to_string()
                ))
            );
            assert_eq!(display_session_id(display_pair("3")), Ok("3".to_string()));
        }

        #[test]
        fn a_display_property_that_is_not_a_pair_is_malformed() {
            let refused =
                display_session_id(OwnedValue::try_from(Value::from(3u32)).unwrap()).unwrap_err();
            assert!(refused.reason().starts_with(REASON_MALFORMED));
        }

        #[test]
        fn a_missing_authority_or_session_keeps_its_stage() {
            let err = zbus::Error::Failure("no such name".to_string());
            assert!(
                refusal(REASON_NO_AUTHORITY, &err)
                    .reason()
                    .starts_with(REASON_NO_AUTHORITY)
            );
            assert!(
                refusal(REASON_NO_SESSION, &err)
                    .reason()
                    .starts_with(REASON_NO_SESSION)
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_polkit_everything_is_refused() {
        let answer = no_polkit();
        assert_ne!(answer, Authorization::Allowed);
        assert_eq!(answer.reason(), REASON_NO_POLKIT);
    }

    #[test]
    fn allowed_reports_itself() {
        assert_eq!(Authorization::Allowed.reason(), "allowed");
    }
}
