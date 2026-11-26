//! The control engine: capability gate, confirm holds, cooldowns, handlers.
//!
//! Mirrors `relay-control.mjs`. Decisions are data; a decision that runs is
//! handed back to the caller, which executes it against the injected adapter.

use std::collections::HashMap;

use ntfy_relay_core::adapter::{
    Capability, NewSession, ProjectEntry, Session, SessionApi, TargetSink,
};
use ntfy_relay_core::clock::Clock;
use ntfy_relay_core::topic::project_folder;

use super::parse::{
    parse_control, render_ack, session_title, short_session_id, Parsed, NATIVE_COMMANDS,
};
use super::table::{
    Action, ControlError, ControlTable, Handler, CONFIRM_EXPIRED_GRACE_MS, CONFIRM_WINDOW_MS,
};
use crate::routing::PENDING_MS;

/// One numbered `/to` menu. Numbers are only valid against the list that was
/// just pushed, and only inside the pending window.
struct Menu {
    ids: Vec<String>,
    at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub topics: usize,
    pub queue: usize,
    pub target: Option<String>,
    pub asks: usize,
    pub uptime_seconds: Option<u64>,
    /// `Some("refused")` when the subscription token is being rejected.
    pub sse: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub verb: String,
    pub arg: String,
    pub action: Action,
    pub confirmed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Prompt,
    Unknown {
        phrase: bool,
    },
    Disabled {
        verb: String,
        missing: Vec<Capability>,
    },
    Run(Run),
    Ask {
        run: Run,
        text: String,
    },
    Busy {
        verb: String,
    },
    Expired,
    Cancelled,
    None,
}

struct Pending {
    run: Run,
    expires_at: i64,
}

struct Dropped {
    at: i64,
    expired: bool,
}

pub struct Control<'a> {
    table: ControlTable,
    adapter: &'a dyn SessionApi,
    clock: &'a dyn Clock,
    /// Wall clock for hint timestamps. Hints are compared against epoch time
    /// by the resolver, so they must not use the monotonic control clock.
    hint_clock: Option<&'a dyn Fn() -> i64>,
    targets: Option<&'a dyn TargetSink>,
    confirm_ms: i64,
    status: Option<&'a dyn Fn() -> Status>,
    disabled: Vec<(String, Vec<Capability>)>,
    pending: HashMap<String, Pending>,
    last_drop: HashMap<String, Dropped>,
    cooldowns: HashMap<(String, String), i64>,
    menus: HashMap<String, Menu>,
}

impl<'a> Control<'a> {
    pub fn new(table: ControlTable, adapter: &'a dyn SessionApi, clock: &'a dyn Clock) -> Self {
        let capabilities = adapter.capabilities();
        let mut disabled = Vec::new();
        for (verb, action) in table.actions() {
            if !action.enabled {
                continue;
            }
            let missing: Vec<Capability> = action
                .requires
                .iter()
                .copied()
                .filter(|capability| !capabilities.contains(capability))
                .collect();
            if !missing.is_empty() {
                disabled.push((verb.to_string(), missing));
            }
        }
        Self {
            table,
            adapter,
            clock,
            hint_clock: None,
            targets: None,
            confirm_ms: CONFIRM_WINDOW_MS,
            status: None,
            disabled,
            pending: HashMap::new(),
            last_drop: HashMap::new(),
            cooldowns: HashMap::new(),
            menus: HashMap::new(),
        }
    }

    /// The app injects the state store so `/new` and `/to` can set the
    /// one-shot target. Without a sink the commands still resolve and ack, they
    /// just do not persist the hint.
    pub fn with_targets(mut self, targets: &'a dyn TargetSink) -> Self {
        self.targets = Some(targets);
        self
    }

    /// Epoch milliseconds for hint timestamps. Without it the monotonic control
    /// clock is used, which is only correct in tests.
    pub fn with_hint_clock(mut self, hint_clock: &'a dyn Fn() -> i64) -> Self {
        self.hint_clock = Some(hint_clock);
        self
    }

    fn hint_now(&self) -> i64 {
        match self.hint_clock {
            Some(clock) => clock(),
            None => self.clock.now_ms(),
        }
    }

    pub fn with_status(mut self, status: &'a dyn Fn() -> Status) -> Self {
        self.status = Some(status);
        self
    }

    pub fn with_confirm_window(mut self, confirm_ms: i64) -> Self {
        self.confirm_ms = confirm_ms;
        self
    }

    pub fn table(&self) -> &ControlTable {
        &self.table
    }

    /// Actions disabled at construction by an unmet capability. The app logs
    /// one line per entry; the engine never logs.
    pub fn disabled_actions(&self) -> impl Iterator<Item = (&str, &[Capability])> {
        self.disabled
            .iter()
            .map(|(verb, missing)| (verb.as_str(), missing.as_slice()))
    }

    /// Exact startup log lines for the disabled actions, one per action.
    pub fn disabled_warnings(&self) -> Vec<String> {
        self.disabled
            .iter()
            .map(|(verb, missing)| {
                let names: Vec<&str> = missing
                    .iter()
                    .map(|capability| capability.as_str())
                    .collect();
                format!(
                    "control action {verb} disabled: missing capability {}",
                    names.join(",")
                )
            })
            .collect()
    }

    pub fn resolve(&mut self, text: &str, topic: &str) -> Decision {
        match parse_control(text, &self.table) {
            Parsed::Prompt => Decision::Prompt,
            Parsed::Unknown { phrase } => Decision::Unknown { phrase },
            Parsed::Confirm { verb } => self.resolve_confirm(&verb, topic),
            Parsed::Action { verb, arg, phrase } => {
                let Some(action) = self.table.action(&verb).cloned() else {
                    return if phrase {
                        Decision::Unknown { phrase }
                    } else {
                        Decision::Prompt
                    };
                };
                if !action.enabled {
                    return if phrase {
                        Decision::Unknown { phrase }
                    } else {
                        Decision::Prompt
                    };
                }
                if let Some((_, missing)) = self.disabled.iter().find(|(name, _)| name == &verb) {
                    return Decision::Disabled {
                        verb,
                        missing: missing.clone(),
                    };
                }
                if !action.confirm {
                    return Decision::Run(Run {
                        verb,
                        arg,
                        action,
                        confirmed: false,
                    });
                }

                let now = self.clock.now_ms();
                if let Some(pending) = self.pending.get(topic) {
                    if now <= pending.expires_at {
                        return Decision::Busy { verb };
                    }
                    self.pending.remove(topic);
                    self.last_drop.insert(
                        topic.to_string(),
                        Dropped {
                            at: now,
                            expired: true,
                        },
                    );
                }
                self.last_drop.remove(topic);
                let run = Run {
                    verb,
                    arg,
                    action,
                    confirmed: false,
                };
                self.pending.insert(
                    topic.to_string(),
                    Pending {
                        run: run.clone(),
                        expires_at: now + self.confirm_ms,
                    },
                );
                let target = if run.arg.is_empty() {
                    format!("/{}", run.verb)
                } else {
                    format!("/{} {}", run.verb, run.arg)
                };
                let seconds = (self.confirm_ms as f64 / 1000.0).round() as i64;
                let text = format!("confirm {target}? answer /yes or /no within {seconds} s");
                Decision::Ask { run, text }
            }
        }
    }

    fn resolve_confirm(&mut self, verb: &str, topic: &str) -> Decision {
        let now = self.clock.now_ms();
        if let Some(pending) = self.pending.remove(topic) {
            if now > pending.expires_at {
                self.last_drop.insert(
                    topic.to_string(),
                    Dropped {
                        at: now,
                        expired: true,
                    },
                );
                return Decision::Expired;
            }
            if verb == "no" {
                return Decision::Cancelled;
            }
            return Decision::Run(Run {
                confirmed: true,
                ..pending.run
            });
        }
        if verb == "yes" {
            if let Some(dropped) = self.last_drop.get(topic) {
                if dropped.expired && now - dropped.at <= CONFIRM_EXPIRED_GRACE_MS {
                    return Decision::Expired;
                }
            }
        }
        Decision::None
    }

    pub async fn execute(
        &mut self,
        run: Run,
        entry: &ProjectEntry,
    ) -> Result<String, ControlError> {
        let now = self.clock.now_ms();
        let key = (entry.topic.clone(), run.verb.clone());
        if run.action.cooldown_ms > 0 {
            if let Some(&last) = self.cooldowns.get(&key) {
                let elapsed = now - last;
                if elapsed < run.action.cooldown_ms as i64 {
                    let remaining = run.action.cooldown_ms as i64 - elapsed;
                    let wait = (remaining as f64 / 1000.0).ceil() as i64;
                    return Err(ControlError::new(format!(
                        "{} is on cooldown, retry in {wait} s",
                        run.verb
                    )));
                }
            }
        }
        self.cooldowns.insert(key, now);

        let action = run.action.clone();
        let output = self.dispatch(&action, &run.arg, entry).await?;
        let mut values = HashMap::new();
        values.insert("project".to_string(), project_name(entry));
        values.insert("arg".to_string(), run.arg.clone());
        values.insert("verb".to_string(), run.verb.clone());
        for (key, value) in output.values {
            values.insert(key, value);
        }
        let text = match &action.ack {
            Some(template) => render_ack(template, &values),
            None => output.text,
        };
        if text.is_empty() {
            Ok(format!("{} done", run.verb))
        } else {
            Ok(text)
        }
    }

    async fn dispatch(
        &mut self,
        action: &Action,
        arg: &str,
        entry: &ProjectEntry,
    ) -> Result<HandlerOutput, ControlError> {
        match action.handler {
            Handler::SessionCreate => self.create_session(action, arg, entry).await,
            Handler::SessionList => self.list_sessions(arg, entry).await,
            Handler::SessionPin => self.select_target(arg, entry).await,
            Handler::SessionAuto => self.clear_target(entry).await,
            Handler::RelayHelp => Ok(HandlerOutput::text(self.help_text())),
            Handler::RelayStatus => Ok(HandlerOutput::text(self.status_text())),
        }
    }

    async fn create_session(
        &self,
        action: &Action,
        arg: &str,
        entry: &ProjectEntry,
    ) -> Result<HandlerOutput, ControlError> {
        let title = if arg.is_empty() { None } else { Some(arg) };
        let session = self
            .adapter
            .create_session(NewSession { entry, title })
            .await
            .map_err(adapter_error)?;
        if session.id.is_empty() {
            return Err(ControlError::new("adapter returned no session id"));
        }
        if action.target {
            if let Some(targets) = self.targets {
                targets.set_pending(&entry.topic, Some(&session.id), self.hint_now());
            }
        }
        let short = short_session_id(&session.id);
        let title = session_title(&session.id, session.title.as_deref());
        let text = format!("new session {short} {title} in {}", project_name(entry));
        let mut output = HandlerOutput::text(text);
        output.values.insert("id".to_string(), short.clone());
        output.values.insert("session".to_string(), short);
        output.values.insert("title".to_string(), title);
        Ok(output)
    }

    async fn list_sessions(
        &self,
        arg: &str,
        entry: &ProjectEntry,
    ) -> Result<HandlerOutput, ControlError> {
        let count = parse_count(arg)?;
        let sessions = self
            .adapter
            .list_sessions(entry, 20)
            .await
            .map_err(adapter_error)?;
        if sessions.is_empty() {
            let mut output = HandlerOutput::text("no sessions".to_string());
            output.values.insert("count".to_string(), "0".to_string());
            return Ok(output);
        }
        let shown: Vec<&Session> = sessions.iter().take(count).collect();
        let lines: Vec<String> = shown
            .iter()
            .enumerate()
            .map(|(index, session)| {
                format!(
                    "{} {} {}",
                    index + 1,
                    if session.running { "running" } else { "idle" },
                    session_title(&session.id, session.title.as_deref())
                )
            })
            .collect();
        let mut output = HandlerOutput::text(lines.join("\n"));
        output
            .values
            .insert("count".to_string(), shown.len().to_string());
        Ok(output)
    }

    /// `/to` lists numbered titles, then takes a number or a title fragment.
    /// The number is only valid against the list this engine last pushed.
    async fn select_target(
        &mut self,
        arg: &str,
        entry: &ProjectEntry,
    ) -> Result<HandlerOutput, ControlError> {
        let now = self.clock.now_ms();
        if arg.is_empty() {
            let sessions = self
                .adapter
                .list_sessions(entry, 5)
                .await
                .map_err(adapter_error)?;
            if sessions.is_empty() {
                return Err(ControlError::new("no sessions"));
            }
            let lines: Vec<String> = sessions
                .iter()
                .enumerate()
                .map(|(index, session)| {
                    format!(
                        "{} {} {}",
                        index + 1,
                        if session.running { "running" } else { "idle" },
                        session_title(&session.id, session.title.as_deref())
                    )
                })
                .collect();
            self.menus.insert(
                entry.topic.clone(),
                Menu {
                    ids: sessions.iter().map(|session| session.id.clone()).collect(),
                    at: now,
                },
            );
            return Ok(HandlerOutput::text(format!(
                "{}\nreply /to <number>",
                lines.join("\n")
            )));
        }

        let session = if let Ok(number) = arg.parse::<usize>() {
            let menu = self
                .menus
                .get(&entry.topic)
                .filter(|menu| now.saturating_sub(menu.at) <= PENDING_MS)
                .ok_or_else(|| ControlError::new("no current list; send /to"))?;
            let id = number
                .checked_sub(1)
                .and_then(|index| menu.ids.get(index))
                .ok_or_else(|| ControlError::new(format!("no entry {number} in the list")))?;
            let sessions = self
                .adapter
                .list_sessions(entry, 20)
                .await
                .map_err(adapter_error)?;
            sessions
                .into_iter()
                .find(|session| &session.id == id)
                .ok_or_else(|| ControlError::new("that session is gone; send /to"))?
        } else {
            let sessions = self
                .adapter
                .list_sessions(entry, 20)
                .await
                .map_err(adapter_error)?;
            let needle = arg.to_ascii_lowercase();
            let matches: Vec<Session> = sessions
                .into_iter()
                .filter(|session| {
                    session_title(&session.id, session.title.as_deref())
                        .to_ascii_lowercase()
                        .contains(&needle)
                })
                .collect();
            match matches.len() {
                0 => return Err(ControlError::new(format!("no session matches {arg}"))),
                1 => matches.into_iter().next().expect("one match"),
                count => {
                    return Err(ControlError::new(format!(
                        "{count} sessions match {arg}; send /to for numbers"
                    )))
                }
            }
        };

        if let Some(targets) = self.targets {
            targets.set_pending(&entry.topic, Some(&session.id), self.hint_now());
        }
        let short = short_session_id(&session.id);
        let title = session_title(&session.id, session.title.as_deref());
        let text = format!("next message -> {title}");
        let mut output = HandlerOutput::text(text);
        output.values.insert("id".to_string(), short.clone());
        output.values.insert("session".to_string(), short);
        output.values.insert("title".to_string(), title);
        Ok(output)
    }

    async fn clear_target(&mut self, entry: &ProjectEntry) -> Result<HandlerOutput, ControlError> {
        if let Some(targets) = self.targets {
            targets.set_pending(&entry.topic, None, self.hint_now());
        }
        Ok(HandlerOutput::text("target cleared".to_string()))
    }

    fn help_text(&self) -> String {
        let mut lines = Vec::new();
        for (verb, action) in self.table.actions() {
            if !action.enabled {
                continue;
            }
            let arg = action
                .arg
                .as_ref()
                .map(|name| format!(" <{name}>"))
                .unwrap_or_default();
            let alias = if action.aliases.is_empty() {
                String::new()
            } else {
                format!(" ({})", action.aliases.join(", "))
            };
            let note = match self.disabled.iter().find(|(name, _)| name == verb) {
                Some((_, missing)) => {
                    let names: Vec<&str> = missing
                        .iter()
                        .map(|capability| capability.as_str())
                        .collect();
                    format!(" - unavailable: needs {}", names.join(", "))
                }
                None if action.confirm => " - asks /yes or /no".to_string(),
                None => String::new(),
            };
            lines.push(format!("/{verb}{arg}{alias}{note}"));
        }
        for command in NATIVE_COMMANDS {
            lines.push(command.help.to_string());
        }
        lines.join("\n")
    }

    fn status_text(&self) -> String {
        let status = self.status.map(|status| status()).unwrap_or_default();
        let mut lines = vec![
            format!("topics {}", status.topics),
            format!("queue {}", status.queue),
            format!("target {}", status.target.as_deref().unwrap_or("none")),
            format!("asks {}", status.asks),
        ];
        if let Some(seconds) = status.uptime_seconds {
            lines.push(format!("uptime {seconds} s"));
        }
        if let Some(sse) = status.sse {
            lines.push(format!("sse {sse}"));
        }
        lines.join("\n")
    }
}

struct HandlerOutput {
    text: String,
    values: HashMap<String, String>,
}

impl HandlerOutput {
    fn text(text: String) -> Self {
        Self {
            text,
            values: HashMap::new(),
        }
    }
}

fn adapter_error(error: ntfy_relay_core::adapter::AdapterError) -> ControlError {
    ControlError::new(error.to_string())
}

fn project_name(entry: &ProjectEntry) -> String {
    let canonical = entry.canonical.to_string_lossy();
    project_folder(&canonical).unwrap_or("project").to_string()
}

fn parse_count(arg: &str) -> Result<usize, ControlError> {
    const FALLBACK: usize = 5;
    const MAX: usize = 20;
    if arg.is_empty() {
        return Ok(FALLBACK);
    }
    let invalid = || ControlError::new(format!("count must be a number from 1 to {MAX}"));
    if arg.len() > 2 || !arg.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    let count: usize = arg.parse().map_err(|_| invalid())?;
    if !(1..=MAX).contains(&count) {
        return Err(invalid());
    }
    Ok(count)
}
