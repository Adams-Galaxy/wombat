use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;

use crate::context::HostContext;
use crate::frozen::FrozenValue;
use crate::manifest::{BuildInput, BuildInputKind, BuildInputOrigin};
use crate::{Result, WombatError};

#[derive(Clone, Debug)]
pub(crate) struct InputSpec {
    pub order: u64,
    pub kind: BuildInputKind,
    pub long: Option<String>,
    pub short: Option<char>,
    pub help: Option<String>,
    pub default: Option<FrozenValue>,
    pub choices: Vec<String>,
    pub min: Option<i64>,
    pub max: Option<i64>,
    pub declared_from: String,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedInputs {
    pub manifest: Vec<BuildInput>,
    pub values: BTreeMap<String, FrozenValue>,
    pub help: Option<String>,
}

impl InputSpec {
    pub fn parse(
        order: u64,
        kind: &str,
        options: FrozenValue,
        declared_from: String,
    ) -> Result<Self> {
        let kind = match kind {
            "flag" => BuildInputKind::Flag,
            "choice" => BuildInputKind::Choice,
            "string" => BuildInputKind::String,
            "integer" => BuildInputKind::Integer,
            "target" => BuildInputKind::Target,
            _ => {
                return Err(WombatError::configuration(format!(
                    "unknown input kind `{kind}`"
                )));
            }
        };
        let FrozenValue::Map(mut options) = options else {
            return Err(WombatError::configuration(
                "input options must be a string-keyed table",
            ));
        };
        let long = take_optional_string(&mut options, "long")?;
        if let Some(long) = &long {
            validate_long(long)?;
        }
        let short = take_optional_string(&mut options, "short")?
            .map(|value| {
                let mut characters = value.chars();
                match characters.next().filter(char::is_ascii_alphanumeric) {
                    Some(character) if characters.next().is_none() => Ok(character),
                    _ => Err(WombatError::configuration(
                        "input `short` must be one ASCII letter or digit",
                    )),
                }
            })
            .transpose()?;
        let help = take_optional_string(&mut options, "help")?;
        let default = options.remove("default");
        let choices = take_string_array(&mut options, "values")?.unwrap_or_default();
        let min = take_optional_integer(&mut options, "min")?;
        let max = take_optional_integer(&mut options, "max")?;
        if let Some(key) = options.keys().next() {
            return Err(WombatError::configuration(format!(
                "{kind:?} input does not support option `{key}`"
            )));
        }

        match kind {
            BuildInputKind::Flag => {
                if !choices.is_empty() || min.is_some() || max.is_some() {
                    return Err(invalid_options("flag"));
                }
                if let Some(value) = &default
                    && !matches!(value, FrozenValue::Boolean(_))
                {
                    return Err(WombatError::configuration(
                        "flag input default must be boolean",
                    ));
                }
            }
            BuildInputKind::Choice => {
                if choices.is_empty() || min.is_some() || max.is_some() {
                    return Err(WombatError::configuration(
                        "choice input requires a non-empty string `values` array and does not support min/max",
                    ));
                }
                let unique = choices.iter().collect::<BTreeSet<_>>();
                if unique.len() != choices.len() || choices.iter().any(String::is_empty) {
                    return Err(WombatError::configuration(
                        "choice input values must be non-empty and unique",
                    ));
                }
                if let Some(FrozenValue::String(value)) = &default {
                    if !choices.contains(value) {
                        return Err(WombatError::configuration(format!(
                            "choice input default `{value}` is not one of its values"
                        )));
                    }
                } else if default.is_some() {
                    return Err(WombatError::configuration(
                        "choice input default must be a string",
                    ));
                }
            }
            BuildInputKind::String => {
                if !choices.is_empty() || min.is_some() || max.is_some() {
                    return Err(invalid_options("string"));
                }
                if let Some(value) = &default
                    && !matches!(value, FrozenValue::String(_))
                {
                    return Err(WombatError::configuration(
                        "string input default must be a string",
                    ));
                }
            }
            BuildInputKind::Integer => {
                if !choices.is_empty() {
                    return Err(invalid_options("integer"));
                }
                if min.zip(max).is_some_and(|(min, max)| min > max) {
                    return Err(WombatError::configuration("integer input min exceeds max"));
                }
                if let Some(FrozenValue::Integer(value)) = default {
                    validate_integer(value, min, max)?;
                } else if default.is_some() {
                    return Err(WombatError::configuration(
                        "integer input default must be an integer",
                    ));
                }
            }
            BuildInputKind::Target => {
                if default.is_some() || !choices.is_empty() || min.is_some() || max.is_some() {
                    return Err(WombatError::configuration(
                        "target input defaults to the observed host and does not support default, values, min, or max",
                    ));
                }
            }
        }

        Ok(Self {
            order,
            kind,
            long,
            short,
            help,
            default,
            choices,
            min,
            max,
            declared_from,
        })
    }
}

pub(crate) fn resolve(
    schema: Vec<(String, u64)>,
    specs: &BTreeMap<u64, InputSpec>,
    arguments: &[OsString],
    host: &HostContext,
) -> Result<ResolvedInputs> {
    let mut named = Vec::with_capacity(schema.len());
    let mut longs = BTreeMap::new();
    let mut shorts = BTreeMap::new();
    let mut seen_names = BTreeSet::new();
    let mut seen_descriptors = BTreeSet::new();
    for (name, id) in schema {
        validate_name(&name)?;
        if !seen_names.insert(name.clone()) {
            return Err(WombatError::configuration(format!(
                "duplicate input `{name}`"
            )));
        }
        if !seen_descriptors.insert(id) {
            return Err(WombatError::configuration(format!(
                "Wombat input descriptor `{id}` is reused; create one descriptor per input"
            )));
        }
        let spec = specs.get(&id).ok_or_else(|| {
            WombatError::configuration(format!("input `{name}` is not a Wombat input descriptor"))
        })?;
        let long = spec.long.clone().unwrap_or_else(|| name.replace('_', "-"));
        validate_long(&long)?;
        if longs.insert(long.clone(), name.clone()).is_some() {
            return Err(WombatError::configuration(format!(
                "duplicate project option `--{long}`"
            )));
        }
        if spec.kind == BuildInputKind::Flag {
            let negative = format!("no-{long}");
            if longs.contains_key(&negative) {
                return Err(WombatError::configuration(format!(
                    "project flag `--{long}` conflicts with `--{negative}`"
                )));
            }
        }
        if let Some(short) = spec.short
            && shorts.insert(short, name.clone()).is_some()
        {
            return Err(WombatError::configuration(format!(
                "duplicate project short option `-{short}`"
            )));
        }
        named.push((name, long, spec));
    }
    named.sort_by_key(|(_, _, spec)| spec.order);

    let strings = arguments
        .iter()
        .map(|value| {
            value.to_str().map(str::to_string).ok_or_else(|| {
                WombatError::configuration("project build arguments must be valid UTF-8")
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let help_requested = strings.iter().any(|argument| argument == "--help");
    if help_requested && strings.iter().any(|argument| argument != "--help") {
        return Err(WombatError::configuration(
            "project `--help` cannot be combined with other project arguments",
        ));
    }
    let help = help_requested.then(|| render_help(&named, host));
    let strings = if help_requested { Vec::new() } else { strings };

    let by_name = named
        .iter()
        .map(|(name, long, spec)| (name.clone(), (long.as_str(), *spec)))
        .collect::<BTreeMap<_, _>>();
    let mut supplied = BTreeMap::<String, String>::new();
    let mut index = 0;
    while index < strings.len() {
        let argument = &strings[index];
        if let Some(body) = argument.strip_prefix("--") {
            if body.is_empty() {
                return Err(WombatError::configuration(
                    "unexpected project argument `--`",
                ));
            }
            let (spelling, attached) = body
                .split_once('=')
                .map_or((body, None), |(name, value)| (name, Some(value)));
            let (name, negative) = if let Some(name) = spelling.strip_prefix("no-") {
                let resolved = longs
                    .get(name)
                    .filter(|resolved| by_name[*resolved].1.kind == BuildInputKind::Flag);
                (resolved.cloned(), true)
            } else {
                (longs.get(spelling).cloned(), false)
            };
            let name = name.ok_or_else(|| {
                WombatError::configuration(format!("unknown project option `--{spelling}`"))
            })?;
            let spec = by_name[&name].1;
            let value = if spec.kind == BuildInputKind::Flag {
                if attached.is_some() {
                    return Err(WombatError::configuration(format!(
                        "project flag `--{spelling}` does not take a value"
                    )));
                }
                (!negative).to_string()
            } else {
                if negative {
                    return Err(WombatError::configuration(format!(
                        "project option `--{spelling}` is not a flag"
                    )));
                }
                match attached {
                    Some(value) if !value.is_empty() => value.to_string(),
                    Some(_) => {
                        return Err(WombatError::configuration(format!(
                            "project option `--{spelling}` requires a value"
                        )));
                    }
                    None => {
                        index += 1;
                        strings
                            .get(index)
                            .filter(|value| !value.starts_with('-'))
                            .cloned()
                            .ok_or_else(|| {
                                WombatError::configuration(format!(
                                    "project option `--{spelling}` requires a value"
                                ))
                            })?
                    }
                }
            };
            insert_supplied(&mut supplied, &name, value)?;
        } else if argument.starts_with('-') {
            let body = argument.strip_prefix('-').expect("short option has prefix");
            let mut characters = body.chars();
            let short = characters
                .next()
                .filter(char::is_ascii_alphanumeric)
                .ok_or_else(|| {
                    WombatError::configuration(format!("invalid project option `{argument}`"))
                })?;
            if characters.next().is_some() {
                return Err(WombatError::configuration(format!(
                    "combined or attached short project options are not supported: `{argument}`"
                )));
            }
            let name = shorts.get(&short).cloned().ok_or_else(|| {
                WombatError::configuration(format!("unknown project option `-{short}`"))
            })?;
            let spec = by_name[&name].1;
            let value = if spec.kind == BuildInputKind::Flag {
                "true".to_string()
            } else {
                index += 1;
                strings
                    .get(index)
                    .filter(|value| !value.starts_with('-'))
                    .cloned()
                    .ok_or_else(|| {
                        WombatError::configuration(format!(
                            "project option `-{short}` requires a value"
                        ))
                    })?
            };
            insert_supplied(&mut supplied, &name, value)?;
        } else {
            return Err(WombatError::configuration(format!(
                "unexpected positional project argument `{argument}`"
            )));
        }
        index += 1;
    }

    let mut manifest = Vec::with_capacity(named.len());
    let mut values = BTreeMap::new();
    for (name, _, spec) in named {
        let (value, origin) = if let Some(value) = supplied.remove(&name) {
            (parse_value(spec, &value)?, BuildInputOrigin::CommandLine)
        } else {
            (default_value(spec, host, &name)?, BuildInputOrigin::Default)
        };
        values.insert(name.clone(), value.clone());
        manifest.push(BuildInput {
            name,
            kind: spec.kind,
            value,
            origin,
            declared_from: spec.declared_from.clone(),
        });
    }
    manifest.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(ResolvedInputs {
        manifest,
        values,
        help,
    })
}

fn parse_value(spec: &InputSpec, value: &str) -> Result<FrozenValue> {
    match spec.kind {
        BuildInputKind::Flag => Ok(FrozenValue::Boolean(value == "true")),
        BuildInputKind::Choice => {
            if !spec.choices.iter().any(|choice| choice == value) {
                return Err(WombatError::configuration(format!(
                    "invalid choice `{value}`; expected one of {}",
                    spec.choices.join(", ")
                )));
            }
            Ok(FrozenValue::String(value.to_string()))
        }
        BuildInputKind::String => Ok(FrozenValue::String(value.to_string())),
        BuildInputKind::Integer => {
            let value = value
                .parse::<i64>()
                .map_err(|_| WombatError::configuration(format!("invalid integer `{value}`")))?;
            validate_integer(value, spec.min, spec.max)?;
            Ok(FrozenValue::Integer(value))
        }
        BuildInputKind::Target => Ok(FrozenValue::String(
            crate::context::TargetPlatform::parse_compact(value)?.compact(),
        )),
    }
}

fn default_value(spec: &InputSpec, host: &HostContext, name: &str) -> Result<FrozenValue> {
    if spec.kind == BuildInputKind::Flag {
        return Ok(spec.default.clone().unwrap_or(FrozenValue::Boolean(false)));
    }
    if spec.kind == BuildInputKind::Target {
        return Ok(FrozenValue::String(host.platform.compact()));
    }
    spec.default.clone().ok_or_else(|| {
        WombatError::configuration(format!(
            "required project option `--{}` was not provided",
            spec.long.clone().unwrap_or_else(|| name.replace('_', "-"))
        ))
    })
}

fn render_help(named: &[(String, String, &InputSpec)], host: &HostContext) -> String {
    use std::fmt::Write as _;
    let mut output = String::from(
        "Repository build inputs\n\nUsage: wombat build [WOMBAT OPTIONS] -- [PROJECT OPTIONS]\n\nOptions:\n",
    );
    for (name, long, spec) in named {
        let spelling = match (spec.short, spec.kind) {
            (Some(short), BuildInputKind::Flag) => format!("  -{short}, --{long}, --no-{long}"),
            (None, BuildInputKind::Flag) => format!("      --{long}, --no-{long}"),
            (Some(short), _) => format!("  -{short}, --{long} <{}>", value_label(spec.kind)),
            (None, _) => format!("      --{long} <{}>", value_label(spec.kind)),
        };
        writeln!(output, "{spelling}").expect("writing to string cannot fail");
        if let Some(help) = &spec.help {
            writeln!(output, "          {help}").expect("writing to string cannot fail");
        }
        if spec.kind == BuildInputKind::Choice {
            writeln!(output, "          [values: {}]", spec.choices.join(", "))
                .expect("writing to string cannot fail");
        }
        let default = default_value(spec, host, name).ok();
        if let Some(default) = default {
            writeln!(output, "          [default: {}]", display_value(&default))
                .expect("writing to string cannot fail");
        } else {
            writeln!(output, "          [required]").expect("writing to string cannot fail");
        }
    }
    output
}

fn display_value(value: &FrozenValue) -> String {
    match value {
        FrozenValue::Boolean(value) => value.to_string(),
        FrozenValue::Integer(value) => value.to_string(),
        FrozenValue::String(value) => value.clone(),
        _ => serde_json::to_string(value).expect("frozen values serialize"),
    }
}

const fn value_label(kind: BuildInputKind) -> &'static str {
    match kind {
        BuildInputKind::Flag => "FLAG",
        BuildInputKind::Choice => "CHOICE",
        BuildInputKind::String => "STRING",
        BuildInputKind::Integer => "INTEGER",
        BuildInputKind::Target => "OS/ARCH",
    }
}

fn insert_supplied(values: &mut BTreeMap<String, String>, name: &str, value: String) -> Result<()> {
    if values.insert(name.to_string(), value).is_some() {
        return Err(WombatError::configuration(format!(
            "project option for `{name}` was provided more than once"
        )));
    }
    Ok(())
}

fn validate_integer(value: i64, min: Option<i64>, max: Option<i64>) -> Result<()> {
    if min.is_some_and(|min| value < min) || max.is_some_and(|max| value > max) {
        return Err(WombatError::configuration(format!(
            "integer `{value}` is outside its declared bounds"
        )));
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<()> {
    let mut bytes = name.bytes();
    if !bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(WombatError::configuration(format!(
            "invalid input name `{name}`; use a Lua identifier-style name"
        )));
    }
    Ok(())
}

fn validate_long(value: &str) -> Result<()> {
    if value == "help"
        || value.starts_with("no-")
        || !value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(WombatError::configuration(format!(
            "invalid project long option `{value}`"
        )));
    }
    Ok(())
}

fn take_optional_string(
    map: &mut BTreeMap<String, FrozenValue>,
    key: &str,
) -> Result<Option<String>> {
    match map.remove(key) {
        None => Ok(None),
        Some(FrozenValue::String(value)) if !value.is_empty() => Ok(Some(value)),
        Some(_) => Err(WombatError::configuration(format!(
            "input `{key}` must be a non-empty string"
        ))),
    }
}

fn take_optional_integer(
    map: &mut BTreeMap<String, FrozenValue>,
    key: &str,
) -> Result<Option<i64>> {
    match map.remove(key) {
        None => Ok(None),
        Some(FrozenValue::Integer(value)) => Ok(Some(value)),
        Some(_) => Err(WombatError::configuration(format!(
            "input `{key}` must be an integer"
        ))),
    }
}

fn take_string_array(
    map: &mut BTreeMap<String, FrozenValue>,
    key: &str,
) -> Result<Option<Vec<String>>> {
    match map.remove(key) {
        None => Ok(None),
        Some(FrozenValue::Array(values)) => values
            .into_iter()
            .map(|value| match value {
                FrozenValue::String(value) => Ok(value),
                _ => Err(WombatError::configuration(format!(
                    "input `{key}` must contain only strings"
                ))),
            })
            .collect::<Result<Vec<_>>>()
            .map(Some),
        Some(_) => Err(WombatError::configuration(format!(
            "input `{key}` must be an array"
        ))),
    }
}

fn invalid_options(kind: &str) -> WombatError {
    WombatError::configuration(format!(
        "{kind} input received options supported only by another input kind"
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;

    use crate::context::{Architecture, HostContext, OperatingSystemName, TargetPlatform};
    use crate::frozen::FrozenValue;

    use super::{InputSpec, resolve};

    fn host() -> HostContext {
        HostContext::fixture(TargetPlatform::minimal(
            OperatingSystemName::Macos,
            Architecture::Aarch64,
        ))
    }

    #[test]
    fn parses_flags_choices_and_target_with_stable_values() {
        let specs = BTreeMap::from([
            (
                1,
                InputSpec::parse(
                    1,
                    "flag",
                    FrozenValue::Map([("default".into(), FrozenValue::Boolean(true))].into()),
                    "wombat.lua:1".into(),
                )
                .unwrap(),
            ),
            (
                2,
                InputSpec::parse(
                    2,
                    "choice",
                    FrozenValue::Map(
                        [
                            (
                                "values".into(),
                                FrozenValue::Array(vec![
                                    FrozenValue::String("dark".into()),
                                    FrozenValue::String("light".into()),
                                ]),
                            ),
                            ("default".into(), FrozenValue::String("dark".into())),
                        ]
                        .into(),
                    ),
                    "wombat.lua:2".into(),
                )
                .unwrap(),
            ),
            (
                3,
                InputSpec::parse(3, "target", FrozenValue::empty_map(), "wombat.lua:3".into())
                    .unwrap(),
            ),
        ]);
        let resolved = resolve(
            vec![
                ("enabled".into(), 1),
                ("theme".into(), 2),
                ("target".into(), 3),
            ],
            &specs,
            &[
                OsString::from("--no-enabled"),
                OsString::from("--theme=light"),
                OsString::from("--target"),
                OsString::from("linux/amd64"),
            ],
            &host(),
        )
        .unwrap();
        assert_eq!(resolved.values["enabled"], FrozenValue::Boolean(false));
        assert_eq!(
            resolved.values["theme"],
            FrozenValue::String("light".into())
        );
        assert_eq!(
            resolved.values["target"],
            FrozenValue::String("linux/x86_64".into())
        );
    }
}
