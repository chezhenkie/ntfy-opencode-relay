//! Command line parsing for `ntfy-admin`.

use ntfy_relay_control::types::Mode;

pub const USAGE: &str = "usage: ntfy-admin topics [--json]\n       \
ntfy-admin add <folder> [--json]\n       \
ntfy-admin remove <folder> [--json]\n       \
ntfy-admin status [--json]\n       \
ntfy-admin stop [--json]\n       \
ntfy-admin mode <tailscale|nord|lan> [--json]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Topics,
    Add(String),
    Remove(String),
    Status,
    Stop,
    Mode(Mode),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub command: Command,
    pub json: bool,
}

pub fn parse(args: &[String]) -> Result<Invocation, String> {
    let mut json = false;
    let mut words: Vec<&str> = Vec::with_capacity(args.len());
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            value if value.starts_with('-') => {
                return Err(format!("unknown argument: {value}"));
            }
            value => words.push(value),
        }
    }
    let Some(name) = words.first().copied() else {
        return Err("no command".to_string());
    };
    if words.len() > 2 {
        return Err(format!("unexpected argument: {}", words[2]));
    }
    let target = words.get(1).copied();
    let command = match name {
        "topics" | "status" | "stop" if target.is_some() => {
            return Err(format!("{name} takes no argument"));
        }
        "topics" => Command::Topics,
        "status" => Command::Status,
        "stop" => Command::Stop,
        "add" => Command::Add(take(name, target, "a folder")?),
        "remove" => Command::Remove(take(name, target, "a folder")?),
        "mode" => {
            let value = take(name, target, "a mode")?;
            match Mode::parse(&value) {
                Some(mode) => Command::Mode(mode),
                None => return Err(format!("mode must be tailscale, nord, or lan, not {value}")),
            }
        }
        other => return Err(format!("unknown command: {other}")),
    };
    Ok(Invocation { command, json })
}

fn take(name: &str, target: Option<&str>, noun: &str) -> Result<String, String> {
    target
        .map(str::to_string)
        .ok_or_else(|| format!("{name} needs {noun}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|value| value.to_string()).collect()
    }

    fn invocation(list: &[&str]) -> Invocation {
        parse(&args(list)).expect("parses")
    }

    #[test]
    fn each_command_parses() {
        assert_eq!(
            invocation(&["topics"]),
            Invocation {
                command: Command::Topics,
                json: false
            }
        );
        assert_eq!(
            invocation(&["status"]),
            Invocation {
                command: Command::Status,
                json: false
            }
        );
        assert_eq!(
            invocation(&["stop"]),
            Invocation {
                command: Command::Stop,
                json: false
            }
        );
        assert_eq!(
            invocation(&["add", "/srv/app"]).command,
            Command::Add("/srv/app".into())
        );
        assert_eq!(
            invocation(&["remove", "/srv/app"]).command,
            Command::Remove("/srv/app".into())
        );
        assert_eq!(
            invocation(&["mode", "nord"]).command,
            Command::Mode(Mode::Nord)
        );
    }

    #[test]
    fn json_is_accepted_before_or_after_the_command() {
        assert!(invocation(&["--json", "topics"]).json);
        assert!(invocation(&["topics", "--json"]).json);
        assert_eq!(
            invocation(&["--json", "add", "/srv/app"]).command,
            Command::Add("/srv/app".into())
        );
    }

    #[test]
    fn a_path_with_spaces_stays_one_word() {
        let folder = r"C:\work\My Projects";
        assert_eq!(
            invocation(&["add", folder]).command,
            Command::Add(folder.into())
        );
    }

    #[test]
    fn missing_and_extra_arguments_are_rejected() {
        assert!(parse(&args(&["add"]))
            .unwrap_err()
            .contains("needs a folder"));
        assert!(parse(&args(&["mode"]))
            .unwrap_err()
            .contains("needs a mode"));
        assert!(parse(&args(&["topics", "extra"]))
            .unwrap_err()
            .contains("takes no argument"));
        assert!(parse(&args(&["add", "/a", "/b"]))
            .unwrap_err()
            .contains("unexpected argument"));
    }

    #[test]
    fn unknown_words_and_modes_are_rejected() {
        assert!(parse(&args(&["restart"]))
            .unwrap_err()
            .contains("unknown command"));
        assert!(parse(&args(&["--force", "stop"]))
            .unwrap_err()
            .contains("unknown argument"));
        assert!(parse(&args(&["mode", "vpn"]))
            .unwrap_err()
            .contains("mode must be"));
        assert!(parse(&args(&[])).unwrap_err().contains("no command"));
    }
}
