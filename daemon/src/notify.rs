//! Desktop notifications (org.freedesktop.Notifications) with actions, so
//! incoming requests can be accepted straight from the notification popup.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use anyhow::Result;
use futures_util::StreamExt;
use log::debug;
use zbus::message::Type as MsgType;
use zbus::{Connection, MatchRule, MessageStream, Variant};

use crate::daemon::{self, Daemon, Direction, Transfer, TransferState};
use crate::util;

const DEST: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

/// How notification bodies must be written for the running server.
#[derive(Clone, Copy, Debug, PartialEq)]
enum BodyFormat {
    Plain,
    /// The server parses markup: escape &, < and >.
    Markup,
    /// Quickshell-based shells (Dux) render plain bodies as Markdown, which
    /// turns underscores in file names into italics; send HTML instead.
    Html,
}

fn escape_markup(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn format_body(format: BodyFormat, body: &str) -> String {
    match format {
        BodyFormat::Plain => body.to_owned(),
        BodyFormat::Markup => escape_markup(body),
        BodyFormat::Html if body.is_empty() => String::new(),
        BodyFormat::Html => format!(
            "<span>{}</span>",
            escape_markup(body).replace('\n', "<br/>")
        ),
    }
}

#[derive(Clone)]
enum About {
    Consent(String),
    Finished(String),
}

pub struct Notifier {
    conn: Connection,
    about: Mutex<HashMap<u32, About>>,
    by_transfer: Mutex<HashMap<String, u32>>,
    daemon: Weak<Daemon>,
    body_format: Mutex<Option<BodyFormat>>,
}

impl Notifier {
    pub async fn start(daemon: Weak<Daemon>) -> Result<Arc<Self>> {
        let conn = Connection::session().await?;
        let n = Arc::new(Self {
            conn,
            about: Mutex::default(),
            by_transfer: Mutex::default(),
            daemon,
            body_format: Mutex::default(),
        });
        let weak = Arc::downgrade(&n);
        let rule = MatchRule::builder()
            .msg_type(MsgType::Signal)
            .interface(DEST)
            .path(PATH)
            .build()?;
        let mut stream = MessageStream::for_match_rule(rule, &n.conn, None).await?;
        tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                let Some(n) = weak.upgrade() else { break };
                match msg
                    .header()
                    .member()
                    .map(|m| m.as_str().to_owned())
                    .as_deref()
                {
                    Some("ActionInvoked") => {
                        if let Ok((id, action)) = msg.body().deserialize::<(u32, String)>() {
                            n.on_action(id, &action);
                        }
                    }
                    Some("NotificationClosed") => {
                        if let Ok((id, _reason)) = msg.body().deserialize::<(u32, u32)>() {
                            n.forget(id);
                        }
                    }
                    _ => {}
                }
            }
        });
        Ok(n)
    }

    /// Ask the server once how it treats bodies (retried until it answers).
    async fn body_format(&self) -> BodyFormat {
        if let Some(f) = *self.body_format.lock().unwrap() {
            return f;
        }
        let caps = self
            .conn
            .call_method(Some(DEST), PATH, Some(DEST), "GetCapabilities", &())
            .await;
        let info = self
            .conn
            .call_method(Some(DEST), PATH, Some(DEST), "GetServerInformation", &())
            .await;
        let (Ok(caps), Ok(info)) = (caps, info) else {
            return BodyFormat::Plain;
        };
        let caps: Vec<String> = caps.body().deserialize().unwrap_or_default();
        let name = info
            .body()
            .deserialize::<(String, String, String, String)>()
            .map(|i| i.0)
            .unwrap_or_default();
        let format = if !caps.iter().any(|c| c == "body-markup") {
            BodyFormat::Plain
        } else if name.eq_ignore_ascii_case("quickshell") {
            BodyFormat::Html
        } else {
            BodyFormat::Markup
        };
        debug!("Notifications: server {name:?}, bodies as {format:?}");
        *self.body_format.lock().unwrap() = Some(format);
        format
    }

    async fn show(
        &self,
        replaces: u32,
        icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        critical: bool,
    ) -> Option<u32> {
        let body = format_body(self.body_format().await, body);
        let mut hints: HashMap<&str, Variant<'_>> = HashMap::new();
        hints.insert("urgency", Variant::from(if critical { 2u8 } else { 1u8 }));
        hints.insert("category", Variant::from("transfer"));
        let actions: Vec<&str> = actions.to_vec();
        let timeout: i32 = if critical { 0 } else { -1 };
        let reply = self
            .conn
            .call_method(
                Some(DEST),
                PATH,
                Some(DEST),
                "Notify",
                &(
                    "Quick Share",
                    replaces,
                    icon,
                    summary,
                    body.as_str(),
                    actions,
                    hints,
                    timeout,
                ),
            )
            .await;
        match reply {
            Ok(m) => m.body().deserialize::<u32>().ok(),
            Err(e) => {
                debug!("Notification failed: {e}");
                None
            }
        }
    }

    async fn close(&self, id: u32) {
        let _ = self
            .conn
            .call_method(Some(DEST), PATH, Some(DEST), "CloseNotification", &(id,))
            .await;
    }

    fn forget(&self, id: u32) {
        if let Some(About::Consent(tid) | About::Finished(tid)) =
            self.about.lock().unwrap().remove(&id)
        {
            let mut by = self.by_transfer.lock().unwrap();
            if by.get(&tid) == Some(&id) {
                by.remove(&tid);
            }
        }
    }

    fn on_action(&self, id: u32, action: &str) {
        let about = self.about.lock().unwrap().get(&id).cloned();
        let Some(d) = self.daemon.upgrade() else {
            return;
        };
        match about {
            Some(About::Consent(tid)) => {
                let _ = d.decide(&tid, action == "accept" || action == "default");
            }
            Some(About::Finished(tid)) => {
                let Some(t) = d.transfer(&tid) else { return };
                match action {
                    "copy" => {
                        if let Some(text) = &t.text {
                            util::copy_to_clipboard(text);
                        }
                    }
                    "folder" => daemon::open_transfer(&t, true),
                    _ => daemon::open_transfer(&t, false),
                }
            }
            None => {}
        }
    }

    pub async fn incoming_request(&self, t: &Transfer) {
        let size = if t.total_bytes > 0 {
            format!(" ({})", util::human_size(t.total_bytes))
        } else {
            String::new()
        };
        let summary = format!("{} wants to share", t.peer_name);
        // Quick Share's verification code; the sending phone shows the same.
        let body = match t.pin.as_deref() {
            Some(pin) => format!(
                "{}{size}\nCode {pin} — check that your phone shows the same.",
                t.summary()
            ),
            None => format!("{}{size}", t.summary()),
        };
        if let Some(id) = self
            .show(
                0,
                "document-send",
                &summary,
                &body,
                &["accept", "Accept", "decline", "Decline"],
                true,
            )
            .await
        {
            self.about
                .lock()
                .unwrap()
                .insert(id, About::Consent(t.id.clone()));
            self.by_transfer.lock().unwrap().insert(t.id.clone(), id);
        }
    }

    pub async fn close_for_transfer(&self, tid: &str) {
        let id = self.by_transfer.lock().unwrap().remove(tid);
        if let Some(id) = id {
            self.about.lock().unwrap().remove(&id);
            self.close(id).await;
        }
    }

    pub async fn transfer_finished(&self, t: &Transfer) {
        let replaces = self.by_transfer.lock().unwrap().remove(&t.id).unwrap_or(0);
        if replaces != 0 {
            self.about.lock().unwrap().remove(&replaces);
        }
        let shown = match (t.direction, t.state) {
            (Direction::Incoming, TransferState::Completed) => {
                if let Some(text) = &t.text {
                    util::copy_to_clipboard(text);
                    let is_url = util::looks_like_url(text);
                    let summary = format!(
                        "{} shared {}",
                        t.peer_name,
                        if is_url { "a link" } else { "text" }
                    );
                    let body = format!(
                        "{}\n(copied to clipboard)",
                        text.chars().take(300).collect::<String>()
                    );
                    let actions: &[&str] = if is_url {
                        &["default", "Open link"]
                    } else {
                        &["copy", "Copy again"]
                    };
                    self.show(replaces, "edit-paste", &summary, &body, actions, false)
                        .await
                } else {
                    let dir = t
                        .saved_paths
                        .first()
                        .and_then(|p| {
                            std::path::Path::new(p)
                                .parent()
                                .map(|d| d.display().to_string())
                        })
                        .unwrap_or_default();
                    let summary = format!("Received {} from {}", t.summary(), t.peer_name);
                    let body = format!("Saved to {dir}");
                    self.show(
                        replaces,
                        "folder-download",
                        &summary,
                        &body,
                        &["default", "Open", "folder", "Show in folder"],
                        false,
                    )
                    .await
                }
            }
            (Direction::Incoming, TransferState::Failed) => {
                let body = t.error.clone().unwrap_or_default();
                self.show(
                    replaces,
                    "dialog-error",
                    &format!("Receiving from {} failed", t.peer_name),
                    &body,
                    &[],
                    false,
                )
                .await
            }
            (Direction::Incoming, TransferState::Cancelled)
                if replaces != 0 || t.bytes_done > 0 =>
            {
                self.show(
                    replaces,
                    "dialog-information",
                    &format!("{} cancelled the transfer", t.peer_name),
                    "",
                    &[],
                    false,
                )
                .await
            }
            (Direction::Outgoing, TransferState::Completed) => {
                self.show(
                    0,
                    "document-send",
                    &format!("Sent {} to {}", t.summary(), t.peer_name),
                    "",
                    &[],
                    false,
                )
                .await
            }
            (Direction::Outgoing, TransferState::Rejected) => {
                self.show(
                    0,
                    "dialog-information",
                    &format!("{} declined", t.peer_name),
                    &t.summary(),
                    &[],
                    false,
                )
                .await
            }
            (Direction::Outgoing, TransferState::Failed) => {
                let body = t.error.clone().unwrap_or_default();
                self.show(
                    0,
                    "dialog-error",
                    &format!("Couldn't send to {}", t.peer_name),
                    &body,
                    &[],
                    false,
                )
                .await
            }
            _ => {
                if replaces != 0 {
                    self.close(replaces).await;
                }
                None
            }
        };
        if let Some(id) = shown {
            self.about
                .lock()
                .unwrap()
                .insert(id, About::Finished(t.id.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bodies() {
        let body = "Screenshot_2026_a<b>.jpg & more\nline two";
        assert_eq!(format_body(BodyFormat::Plain, body), body);
        assert_eq!(
            format_body(BodyFormat::Markup, body),
            "Screenshot_2026_a&lt;b&gt;.jpg &amp; more\nline two"
        );
        assert_eq!(
            format_body(BodyFormat::Html, body),
            "<span>Screenshot_2026_a&lt;b&gt;.jpg &amp; more<br/>line two</span>"
        );
        assert_eq!(format_body(BodyFormat::Html, ""), "");
    }
}
