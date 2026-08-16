//! Structured data codecs exposed to configuration Lua.
//!
//! Every decoder enters through the same tracked repository-source boundary.
//! Format-specific policy is applied before values join the shared frozen tree,
//! so a convenient codec cannot bypass construction identity or weaken the
//! value invariants used by manifests and templates.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_saphyr::options::{DuplicateKeyPolicy, MergeKeyPolicy};

use super::*;

fn read_data_source(
    state: &Rc<RefCell<RuntimeState>>,
    declared: &str,
    caller: &str,
    location: &Location,
) -> Result<String> {
    if declared.is_empty()
        || Path::new(declared).is_absolute()
        || declared
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(WombatError::configuration(format!(
            "{caller} requires a safe repository-relative path at {}",
            location.display()
        )));
    }
    let path = state.borrow().root.join(declared);
    validate_source_components(&state.borrow().root, &path)?;
    load_tracked_source(state, &path)
}

pub(super) fn read_toml_data(
    lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
    declared: &str,
    location: Location,
) -> Result<Value> {
    let source = read_data_source(state, declared, "w.toml.decode()", &location)?;
    let value: FrozenValue = toml::from_str(&source).map_err(|error| {
        WombatError::configuration(format!(
            "failed to parse TOML data `{declared}` at {}: {error}",
            location.display()
        ))
    })?;
    Ok(value.to_lua(lua)?)
}

pub(super) fn read_json_data(
    lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
    declared: &str,
    location: Location,
) -> Result<Value> {
    let source = read_data_source(state, declared, "w.json.decode()", &location)?;
    let value: FrozenValue = serde_json::from_str(&source).map_err(|error| {
        WombatError::configuration(format!(
            "failed to parse JSON data `{declared}` at {}: {error}",
            location.display()
        ))
    })?;
    Ok(value.to_lua(lua)?)
}

pub(super) fn read_yaml_data(
    lua: &Lua,
    state: &Rc<RefCell<RuntimeState>>,
    declared: &str,
    location: Location,
) -> Result<Value> {
    let source = read_data_source(state, declared, "w.yaml.decode()", &location)?;
    reject_unsupported_yaml_tags(&source).map_err(|error| {
        WombatError::configuration(format!(
            "failed to parse YAML data `{declared}` at {}: {error}",
            location.display()
        ))
    })?;
    let options = serde_saphyr::options! {
        budget: serde_saphyr::budget! { max_documents: 1 },
        duplicate_keys: DuplicateKeyPolicy::Error,
        merge_keys: MergeKeyPolicy::Error,
        alias_limits: serde_saphyr::alias_limits! {
            max_total_replayed_events: 100_000,
            max_replay_stack_depth: 32,
            max_alias_expansions_per_anchor: 4_096,
        },
        strict_booleans: true,
        no_schema: true,
    };
    let value = serde_saphyr::from_str_with_options::<YamlFrozen>(&source, options)
        .map_err(|error| {
            WombatError::configuration(format!(
                "failed to parse YAML data `{declared}` at {}: {error}",
                location.display()
            ))
        })?
        .0;
    Ok(value.to_lua(lua)?)
}

pub(super) fn encode_toml_data(value: Value) -> Result<String> {
    let frozen = FrozenValue::from_lua(value)?;
    if !matches!(frozen, FrozenValue::Map(_)) {
        return Err(WombatError::configuration(
            "w.toml.encode() requires a string-keyed table at the document root",
        ));
    }
    reject_toml_null(&frozen, "root")?;
    toml::to_string_pretty(&frozen)
        .map_err(|error| WombatError::configuration(format!("failed to encode TOML data: {error}")))
}

pub(super) fn encode_json_data(value: Value) -> Result<String> {
    let frozen = FrozenValue::from_lua(value)?;
    serde_json::to_string_pretty(&frozen)
        .map_err(|error| WombatError::configuration(format!("failed to encode JSON data: {error}")))
}

pub(super) fn encode_yaml_data(value: Value) -> Result<String> {
    let frozen = FrozenValue::from_lua(value)?;
    let options = serde_saphyr::ser_options! {
        empty_as_braces: true,
        indent_step: 2,
        compact_list_indent: false,
        tagged_enums: false,
        quote_all: false,
        yaml_12: false,
    };
    let encoded = serde_saphyr::to_string_with_options(&frozen, options).map_err(|error| {
        WombatError::configuration(format!("failed to encode YAML data: {error}"))
    })?;
    Ok(format!("{}\n", encoded.trim_end_matches(['\r', '\n'])))
}

fn reject_toml_null(value: &FrozenValue, path: &str) -> Result<()> {
    match value {
        FrozenValue::Null => Err(WombatError::configuration(format!(
            "w.toml.encode() cannot encode null at `{path}` because TOML has no null value"
        ))),
        FrozenValue::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                reject_toml_null(value, &format!("{path}[{}]", index + 1))?;
            }
            Ok(())
        }
        FrozenValue::Map(values) => {
            for (key, value) in values {
                reject_toml_null(value, &format!("{path}.{key}"))?;
            }
            Ok(())
        }
        FrozenValue::Boolean(_)
        | FrozenValue::Integer(_)
        | FrozenValue::Number(_)
        | FrozenValue::String(_) => Ok(()),
    }
}

// `serde-saphyr` deliberately drops tags for typeless Serde visitors. Wombat
// rejects semantics its frozen tree cannot represent, so inspect parser events
// before deserializing instead of silently changing a tagged document's meaning.
fn reject_unsupported_yaml_tags(source: &str) -> std::result::Result<(), String> {
    use serde_saphyr::granit_parser::{Event, Parser};

    for next in Parser::new_from_str(source) {
        let (event, span) = next.map_err(|error| error.to_string())?;
        let tag = match event {
            Event::Scalar(_, _, _, tag)
            | Event::SequenceStart(_, _, tag)
            | Event::MappingStart(_, _, tag) => tag,
            _ => None,
        };
        if let Some(tag) = tag
            && tag.core_suffix().is_none()
        {
            let marker = span.tag_start.unwrap_or(span.start);
            return Err(format!(
                "unsupported YAML tag `{tag}` at {}:{}; Wombat accepts only core null, bool, int, float, string, sequence, and map tags",
                marker.line(),
                marker.col() + 1
            ));
        }
    }
    Ok(())
}

struct YamlFrozen(FrozenValue);

impl<'de> Deserialize<'de> for YamlFrozen {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(YamlFrozenVisitor)
    }
}

struct YamlFrozenVisitor;

impl<'de> Visitor<'de> for YamlFrozenVisitor {
    type Value = YamlFrozen;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a Wombat-compatible YAML value")
    }

    fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
        Ok(YamlFrozen(FrozenValue::Null))
    }

    fn visit_none<E>(self) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_unit()
    }

    fn visit_bool<E>(self, value: bool) -> std::result::Result<Self::Value, E> {
        Ok(YamlFrozen(FrozenValue::Boolean(value)))
    }

    fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E> {
        Ok(YamlFrozen(FrozenValue::Integer(value)))
    }

    fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        i64::try_from(value)
            .map(FrozenValue::Integer)
            .map(YamlFrozen)
            .map_err(|_| {
                E::custom(format!(
                    "integer `{value}` exceeds Wombat's signed 64-bit range"
                ))
            })
    }

    fn visit_f64<E>(self, value: f64) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        if value.is_finite() {
            Ok(YamlFrozen(FrozenValue::Number(value)))
        } else {
            Err(E::custom(
                "YAML numbers crossing the native boundary must be finite",
            ))
        }
    }

    fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E> {
        Ok(YamlFrozen(FrozenValue::String(value.to_owned())))
    }

    fn visit_borrowed_str<E>(self, value: &'de str) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_str(value)
    }

    fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E> {
        Ok(YamlFrozen(FrozenValue::String(value)))
    }

    fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<YamlFrozen>()? {
            values.push(value.0);
        }
        Ok(YamlFrozen(FrozenValue::Array(values)))
    }

    fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = BTreeMap::new();
        while let Some((key, value)) = map.next_entry::<String, YamlFrozen>()? {
            values.insert(key, value.0);
        }
        Ok(YamlFrozen(FrozenValue::Map(values)))
    }
}
