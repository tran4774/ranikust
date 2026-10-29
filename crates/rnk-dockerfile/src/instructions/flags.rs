// Derived from BuildKit, https://github.com/moby/buildkit,
// frontend/dockerfile/instructions/bflag.go at tag v0.34.0.
// Copyright The BuildKit Authors. Licensed under the Apache License, Version 2.0.
// Modified for ranikust: Rust port; flags are declared up front instead of
// through `Add*` calls.

//! Instruction flags (`--from=…`): BuildKit's `BFlags`.

use std::collections::BTreeMap;

use crate::goutil::to_lower;

/// The type of a flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FlagType {
    /// `--x`, `--x=true`, `--x=false`.
    Bool,
    /// `--x=value`; a repeat is an error.
    String,
    /// `--x=value`, repeatable.
    Strings,
}

/// The value of one flag after parsing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FlagValue {
    /// The last value of a bool or string flag (bool: `"true"`/`"false"`).
    pub(crate) value: String,
    /// Every value of a strings flag.
    pub(crate) values: Vec<String>,
    /// The flag was given.
    pub(crate) used: bool,
}

/// Flags of one instruction.
pub(crate) struct Flags {
    defs: Vec<(&'static str, FlagType)>,
    values: BTreeMap<&'static str, FlagValue>,
}

impl Flags {
    /// Declares the flags an instruction accepts, with their defaults.
    pub(crate) fn new(defs: &[(&'static str, FlagType, &str)]) -> Self {
        let mut values = BTreeMap::new();
        for &(name, typ, default) in defs {
            let value = match typ {
                FlagType::Bool => if default == "true" { "true" } else { "false" }.to_owned(),
                FlagType::String => default.to_owned(),
                FlagType::Strings => String::new(),
            };
            values.insert(
                name,
                FlagValue {
                    value,
                    values: Vec::new(),
                    used: false,
                },
            );
        }
        Self {
            defs: defs.iter().map(|&(n, t, _)| (n, t)).collect(),
            values,
        }
    }

    /// BuildKit's `BFlags.Parse`: reads the instruction's flag words.
    pub(crate) fn parse(&mut self, args: &[String]) -> Result<(), String> {
        for a in args {
            if a == "--" {
                // POSIX: `--` ends the options.
                return Ok(());
            }
            let Some(body) = a.strip_prefix("--") else {
                return Err(format!("arg should start with -- : {a}"));
            };
            let (name, value, has_value) = match body.split_once('=') {
                Some((n, v)) => (n, v, true),
                None => (body, "", false),
            };
            let flag_name = format!("--{name}");
            let Some(&(key, typ)) = self.defs.iter().find(|(n, _)| *n == name) else {
                return Err(format!("unknown flag: {flag_name}"));
            };
            let Some(v) = self.values.get_mut(key) else {
                return Err(format!("unknown flag: {flag_name}"));
            };
            if v.used && typ != FlagType::Strings {
                return Err(format!("duplicate flag specified: {flag_name}"));
            }
            v.used = true;
            match typ {
                FlagType::Bool => {
                    // An empty value is only fine without `=`.
                    if has_value && value.is_empty() {
                        return Err(format!("missing a value on flag: {flag_name}"));
                    }
                    v.value = match to_lower(value).as_str() {
                        "true" | "" => "true".to_owned(),
                        "false" => "false".to_owned(),
                        _ => {
                            return Err(format!(
                                "expecting boolean value for flag {flag_name}, not: {value}"
                            ));
                        }
                    };
                }
                FlagType::String => {
                    if !has_value {
                        return Err(format!("missing a value on flag: {flag_name}"));
                    }
                    v.value = value.to_owned();
                }
                FlagType::Strings => {
                    if !has_value {
                        return Err(format!("missing a value on flag: {flag_name}"));
                    }
                    v.values.push(value.to_owned());
                }
            }
        }
        Ok(())
    }

    /// The parsed value of a declared flag.
    pub(crate) fn get(&self, name: &str) -> FlagValue {
        self.values.get(name).cloned().unwrap_or_default()
    }

    /// The string value of a flag.
    pub(crate) fn string(&self, name: &str) -> String {
        self.get(name).value
    }

    /// A bool flag.
    pub(crate) fn bool(&self, name: &str) -> bool {
        self.get(name).value == "true"
    }

    /// A bool flag, `None` when it was not given.
    pub(crate) fn opt_bool(&self, name: &str) -> Option<bool> {
        let v = self.get(name);
        v.used.then_some(v.value == "true")
    }

    /// The values of a strings flag.
    pub(crate) fn strings(&self, name: &str) -> Vec<String> {
        self.get(name).values
    }

    /// Names of the flags that were given, sorted (BuildKit's `Used()`,
    /// whose order is random).
    pub(crate) fn used(&self) -> Vec<String> {
        self.values
            .iter()
            .filter(|(_, v)| v.used)
            .map(|(k, _)| (*k).to_owned())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags() -> Flags {
        Flags::new(&[
            ("link", FlagType::Bool, "false"),
            ("from", FlagType::String, ""),
            ("exclude", FlagType::Strings, ""),
            ("network", FlagType::String, "default"),
        ])
    }

    fn parse(args: &[&str]) -> Result<Flags, String> {
        let mut f = flags();
        let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        f.parse(&args).map(|()| f)
    }

    #[test]
    fn values_and_defaults() {
        let f = parse(&["--link", "--from=a=b", "--exclude=x", "--exclude=y"]).unwrap();
        assert!(f.bool("link"));
        assert_eq!(f.string("from"), "a=b");
        assert_eq!(f.strings("exclude"), ["x", "y"]);
        assert_eq!(f.string("network"), "default");
        assert_eq!(f.used(), ["exclude", "from", "link"]);
        assert_eq!(f.opt_bool("link"), Some(true));
        let f = parse(&["--link=FALSE", "--", "--bogus"]).unwrap();
        assert_eq!(f.opt_bool("link"), Some(false));
    }

    #[test]
    fn errors() {
        assert_eq!(parse(&["x"]).err().unwrap(), "arg should start with -- : x");
        assert_eq!(parse(&["--foo"]).err().unwrap(), "unknown flag: --foo");
        assert_eq!(
            parse(&["--from=a", "--from=b"]).err().unwrap(),
            "duplicate flag specified: --from"
        );
        assert_eq!(
            parse(&["--from"]).err().unwrap(),
            "missing a value on flag: --from"
        );
        assert_eq!(
            parse(&["--link="]).err().unwrap(),
            "missing a value on flag: --link"
        );
        assert!(
            parse(&["--link=yes"])
                .err()
                .unwrap()
                .starts_with("expecting boolean")
        );
    }
}
