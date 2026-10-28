//! Hand-rolled argument parsing.
//!
//! `clap` is deliberately not a dependency (PLAN P-06): this crate sits next to
//! the wasm build, the flags are few, and a hand-rolled parser makes the
//! behaviour — duplicate flags, `--flag=value`, unknown flags, missing values —
//! explicit and testable instead of inferred from a derive macro.
//!
//! The parser is strict on purpose: an unknown flag or a stray positional is a
//! usage error, not something to ignore. A typo like `--tyle 2` silently
//! rendering at 0 dB is exactly the class of bug a CLI must not have.

use crate::{CliError, Result};

/// The flags a subcommand accepts.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    /// Options that take a value (`--in file` or `--in=file`).
    pub values: &'static [&'static str],
    /// Boolean switches that take no value.
    pub flags: &'static [&'static str],
}

impl Spec {
    /// Build a spec from two static slices.
    pub const fn new(values: &'static [&'static str], flags: &'static [&'static str]) -> Self {
        Spec { values, flags }
    }
}

/// Parsed arguments for one subcommand.
#[derive(Debug, Default)]
pub struct Parsed {
    values: Vec<(&'static str, String)>,
    flags: Vec<&'static str>,
}

impl Parsed {
    /// The value given for `name`, if any.
    pub fn value(&self, name: &'static str) -> Option<&str> {
        self.values
            .iter()
            .find(|(flag, _)| *flag == name)
            .map(|(_, value)| value.as_str())
    }

    /// The required value for `name`; a missing one is a usage error.
    pub fn require(&self, name: &'static str) -> Result<&str> {
        self.value(name)
            .ok_or_else(|| CliError::usage(format!("{name} is required")))
    }

    /// Whether the boolean switch `name` was given.
    pub fn flag(&self, name: &'static str) -> bool {
        self.flags.contains(&name)
    }
}

/// Parse `tokens` against `spec`.
///
/// `-h` / `--help` are handled by the caller so it can print command-specific
/// help; if they reach here they are reported as unknown.
pub fn parse(spec: Spec, tokens: &[String]) -> Result<Parsed> {
    let mut parsed = Parsed::default();
    let mut index = 0usize;

    while index < tokens.len() {
        let token = &tokens[index];
        index += 1;

        let Some(body) = token.strip_prefix("--") else {
            return Err(CliError::usage(format!(
                "unexpected argument '{token}'; every argument is a --flag (see --help)"
            )));
        };
        if body.is_empty() {
            return Err(CliError::usage("'--' is not an argument (see --help)"));
        }

        let (name, inline) = match body.split_once('=') {
            Some((name, value)) => (format!("--{name}"), Some(value.to_string())),
            None => (format!("--{body}"), None),
        };

        if let Some(&flag) = spec.flags.iter().find(|flag| **flag == name) {
            if inline.is_some() {
                return Err(CliError::usage(format!("{flag} takes no value")));
            }
            if parsed.flags.contains(&flag) {
                return Err(CliError::usage(format!("{flag} given more than once")));
            }
            parsed.flags.push(flag);
            continue;
        }

        let Some(&flag) = spec.values.iter().find(|flag| **flag == name) else {
            return Err(CliError::usage(format!(
                "unknown option '{name}' for this command (see --help)"
            )));
        };
        if parsed.values.iter().any(|(seen, _)| *seen == flag) {
            return Err(CliError::usage(format!("{flag} given more than once")));
        }

        let value = match inline {
            Some(value) => value,
            None => {
                let next = tokens.get(index).ok_or_else(|| {
                    CliError::usage(format!("{flag} expects a value (see --help)"))
                })?;
                if next.starts_with("--") {
                    return Err(CliError::usage(format!(
                        "{flag} expects a value, found the option '{next}'"
                    )));
                }
                index += 1;
                next.clone()
            }
        };
        if value.is_empty() {
            return Err(CliError::usage(format!("{flag} expects a non-empty value")));
        }
        parsed.values.push((flag, value));
    }

    Ok(parsed)
}

/// `true` when the caller asked for this command's help.
pub fn wants_help(tokens: &[String]) -> bool {
    tokens
        .iter()
        .any(|token| token == "--help" || token == "-h" || token == "help")
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALUES: &[&str] = &["--in", "--out"];
    const FLAGS: &[&str] = &["--trim", "--json"];

    fn spec() -> Spec {
        Spec::new(VALUES, FLAGS)
    }

    fn parse_text(text: &str) -> Result<Parsed> {
        let tokens: Vec<String> = text.split_whitespace().map(str::to_string).collect();
        parse(spec(), &tokens)
    }

    #[test]
    fn accepts_separate_and_inline_values() {
        let parsed = parse_text("--in a.weba --out=b.wav --trim").expect("parses");
        assert_eq!(parsed.value("--in"), Some("a.weba"));
        assert_eq!(parsed.value("--out"), Some("b.wav"));
        assert!(parsed.flag("--trim"));
        assert!(!parsed.flag("--json"));
    }

    #[test]
    fn rejects_typos_duplicates_and_missing_values() {
        for text in [
            "--in a --in b",
            "--tyle 2",
            "--trim=1",
            "--in --out b",
            "--in",
            "a.weba",
            "--",
        ] {
            assert!(parse_text(text).is_err(), "expected an error for {text:?}");
        }
    }

    #[test]
    fn values_may_begin_with_a_single_dash() {
        let parsed = parse_text("--in a --out -o.wav").expect("negative-looking path");
        assert_eq!(parsed.value("--out"), Some("-o.wav"));
    }

    #[test]
    fn require_names_the_missing_flag() {
        let parsed = parse_text("--out b").expect("parses");
        let Err(CliError::Usage(message)) = parsed.require("--in") else {
            panic!("expected a usage error");
        };
        assert!(message.contains("--in"), "{message}");
    }

    #[test]
    fn help_is_detected_without_being_a_parsed_flag() {
        assert!(wants_help(&["--help".to_string()]));
        assert!(wants_help(&["--in".to_string(), "-h".to_string()]));
        assert!(!wants_help(&["--in".to_string(), "x".to_string()]));
    }
}
