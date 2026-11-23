//! The command line as Node's `util.parseArgs` read it (strict, positionals allowed), with its
//! messages: `--name value`, `--name=value`, grouped short flags, `--` ending the options.

#[derive(Debug, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one field per command-line flag, as parseArgs returns them"
)]
pub(crate) struct Args {
    pub(crate) port: Option<String>,
    /// Every `--allow-host`, in order; `None` when none was given.
    pub(crate) allow_hosts: Option<Vec<String>>,
    pub(crate) json: bool,
    pub(crate) help: bool,
    pub(crate) version: bool,
    /// The desktop app starts without a window, as from a login item.
    pub(crate) background: bool,
    pub(crate) positionals: Vec<String>,
}

#[derive(Clone, Copy)]
enum Kind {
    Flag,
    Value,
}

const OPTIONS: &[(&str, Option<char>, Kind)] = &[
    ("port", None, Kind::Value),
    ("allow-host", None, Kind::Value),
    ("json", None, Kind::Flag),
    ("help", Some('h'), Kind::Flag),
    ("version", Some('v'), Kind::Flag),
    ("background", None, Kind::Flag),
];

fn unknown(option: &str) -> String {
    format!(
        "Unknown option '{option}'. To specify a positional argument starting with a '-', place it at the end of the command after '--', as in '-- \"{option}\""
    )
}

impl Args {
    fn set(&mut self, name: &str, value: Option<String>) {
        match (name, value) {
            ("port", value) => self.port = value,
            ("allow-host", Some(value)) => self.allow_hosts.get_or_insert_default().push(value),
            ("json", _) => self.json = true,
            ("help", _) => self.help = true,
            ("version", _) => self.version = true,
            ("background", _) => self.background = true,
            _ => {}
        }
    }

    /// # Errors
    /// The parse error's message, as `parseArgs` throws it.
    pub(crate) fn parse(arguments: &[String]) -> Result<Self, String> {
        let mut args = Self::default();
        let mut rest = arguments.iter();
        while let Some(argument) = rest.next() {
            if argument == "--" {
                args.positionals.extend(rest.by_ref().cloned());
                break;
            }
            if let Some(long) = argument.strip_prefix("--") {
                let (name, inline) = long
                    .split_once('=')
                    .map_or((long, None), |(name, value)| (name, Some(value.to_owned())));
                let Some(&(name, _, kind)) = OPTIONS.iter().find(|option| option.0 == name) else {
                    return Err(unknown(&format!("--{name}")));
                };
                let value = match (kind, inline) {
                    (Kind::Flag, Some(_)) => {
                        return Err(format!("Option '--{name}' does not take an argument"));
                    }
                    (Kind::Flag, None) => None,
                    (Kind::Value, Some(value)) => Some(value),
                    (Kind::Value, None) => Some(value_after(&mut rest, &format!("--{name}"))?),
                };
                args.set(name, value);
            } else if argument.len() > 1 && argument.starts_with('-') {
                let flags: Vec<char> = argument.chars().skip(1).collect();
                for (index, flag) in flags.iter().enumerate() {
                    let Some(&(name, _, kind)) =
                        OPTIONS.iter().find(|option| option.1 == Some(*flag))
                    else {
                        return Err(unknown(&format!("-{flag}")));
                    };
                    match kind {
                        Kind::Flag => args.set(name, None),
                        Kind::Value if index + 1 == flags.len() => {
                            let value = value_after(&mut rest, &format!("-{flag}"))?;
                            args.set(name, Some(value));
                        }
                        Kind::Value => {
                            let value: String = flags.iter().skip(index + 1).collect();
                            args.set(name, Some(value));
                            break;
                        }
                    }
                }
            } else {
                args.positionals.push(argument.clone());
            }
        }
        Ok(args)
    }
}

fn value_after<'a>(
    rest: &mut impl Iterator<Item = &'a String>,
    option: &str,
) -> Result<String, String> {
    match rest.next() {
        None => Err(format!("Option '{option} <value>' argument missing")),
        Some(value) if value.starts_with('-') => Err(format!(
            "Option '{option}' argument is ambiguous.\nDid you forget to specify the option argument for '{option}'?\nTo specify an option argument starting with a dash use '{option}=-XYZ'."
        )),
        Some(value) => Ok(value.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Result<Args, String> {
        let arguments: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
        Args::parse(&arguments)
    }

    #[test]
    fn options_take_values_inline_or_next_and_repeat() {
        let args = parse("serve --port 4400 --allow-host a.ts.net --allow-host=b.ts.net").unwrap();
        assert_eq!(args.port.as_deref(), Some("4400"));
        assert_eq!(
            args.allow_hosts,
            Some(vec!["a.ts.net".to_owned(), "b.ts.net".to_owned()])
        );
        assert_eq!(args.positionals, ["serve"]);
        let args = parse("-hv -- --json").unwrap();
        assert!(args.help && args.version && !args.json);
        assert_eq!(args.positionals, ["--json"]);
    }

    #[test]
    fn errors_read_as_parse_args_wrote_them() {
        assert_eq!(
            parse("--bogus").unwrap_err(),
            "Unknown option '--bogus'. To specify a positional argument starting with a '-', place it at the end of the command after '--', as in '-- \"--bogus\""
        );
        assert_eq!(
            parse("url --port").unwrap_err(),
            "Option '--port <value>' argument missing"
        );
        assert_eq!(
            parse("--json=1").unwrap_err(),
            "Option '--json' does not take an argument"
        );
        assert!(parse("--port --json").unwrap_err().contains("is ambiguous"));
        assert!(parse("-x").unwrap_err().starts_with("Unknown option '-x'."));
    }
}
