//! Owned values crossing the Lua/Rust boundary.

use std::collections::{BTreeMap, HashSet};

use mlua::{Lua, Table, Value};
use serde::{Deserialize, Serialize};

use crate::{Result, WombatError};

const ARRAY_MARKER: &str = "__wombat_array";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FrozenValue {
    Null,
    Boolean(bool),
    Integer(i64),
    Number(f64),
    String(String),
    Array(Vec<FrozenValue>),
    Map(BTreeMap<String, FrozenValue>),
}

impl FrozenValue {
    pub fn empty_map() -> Self {
        Self::Map(BTreeMap::new())
    }

    pub fn from_lua(value: Value) -> Result<Self> {
        Self::freeze(value, &mut HashSet::new())
    }

    fn freeze(value: Value, active_tables: &mut HashSet<usize>) -> Result<Self> {
        match value {
            Value::Nil => Ok(Self::Null),
            Value::Boolean(value) => Ok(Self::Boolean(value)),
            Value::Integer(value) => Ok(Self::Integer(value)),
            Value::Number(value) if value.is_finite() => Ok(Self::Number(value)),
            Value::Number(_) => Err(WombatError::configuration(
                "Lua numbers crossing the native boundary must be finite",
            )),
            Value::String(value) => value
                .to_str()
                .map(|value| Self::String(value.to_owned()))
                .map_err(WombatError::from),
            Value::Table(table) => Self::freeze_table(table, active_tables),
            Value::LightUserData(value) if value.0.is_null() => Ok(Self::Null),
            other => Err(WombatError::configuration(format!(
                "unsupported Lua {} value; expected nil, boolean, number, string, array, or string-keyed map",
                other.type_name()
            ))),
        }
    }

    fn freeze_table(table: Table, active_tables: &mut HashSet<usize>) -> Result<Self> {
        let pointer = table.to_pointer() as usize;
        if !active_tables.insert(pointer) {
            return Err(WombatError::configuration(
                "cyclic Lua tables cannot cross the native boundary",
            ));
        }

        let pairs = table
            .pairs::<Value, Value>()
            .collect::<mlua::Result<Vec<_>>>()
            .map_err(WombatError::from)?;

        let marked_array = table
            .metatable()
            .is_some_and(|metatable| metatable.raw_get::<bool>(ARRAY_MARKER).unwrap_or(false));
        let result = if marked_array {
            Self::freeze_array_pairs(pairs, active_tables)
        } else if pairs.is_empty() {
            Ok(Self::empty_map())
        } else if pairs
            .iter()
            .all(|(key, _)| matches!(key, Value::Integer(value) if *value > 0))
        {
            Self::freeze_array_pairs(pairs, active_tables)
        } else if pairs.iter().all(|(key, _)| matches!(key, Value::String(_))) {
            let mut values = BTreeMap::new();
            for (key, value) in pairs {
                let Value::String(key) = key else {
                    unreachable!("map keys were checked above");
                };
                let key = key.to_str().map_err(WombatError::from)?.to_owned();
                values.insert(key, Self::freeze(value, active_tables)?);
            }
            Ok(Self::Map(values))
        } else {
            Err(WombatError::configuration(
                "Lua tables crossing the native boundary must be contiguous arrays or string-keyed maps",
            ))
        };

        active_tables.remove(&pointer);
        result
    }

    fn freeze_array_pairs(
        pairs: Vec<(Value, Value)>,
        active_tables: &mut HashSet<usize>,
    ) -> Result<Self> {
        let mut indexed = pairs
            .into_iter()
            .map(|(key, value)| match key {
                Value::Integer(index) if index > 0 => Ok((index, value)),
                _ => Err(WombatError::configuration(
                    "Lua arrays must contain only contiguous positive integer keys",
                )),
            })
            .collect::<Result<Vec<_>>>()?;
        indexed.sort_by_key(|(index, _)| *index);

        for (offset, (index, _)) in indexed.iter().enumerate() {
            let expected = i64::try_from(offset + 1)
                .map_err(|_| WombatError::configuration("Lua array is too large to represent"))?;
            if *index != expected {
                return Err(WombatError::configuration(
                    "sparse Lua arrays cannot cross the native boundary",
                ));
            }
        }

        indexed
            .into_iter()
            .map(|(_, value)| Self::freeze(value, active_tables))
            .collect::<Result<Vec<_>>>()
            .map(Self::Array)
    }

    pub fn to_lua(&self, lua: &Lua) -> mlua::Result<Value> {
        match self {
            Self::Null => Ok(Value::NULL),
            Self::Boolean(value) => Ok(Value::Boolean(*value)),
            Self::Integer(value) => Ok(Value::Integer(*value)),
            Self::Number(value) => Ok(Value::Number(*value)),
            Self::String(value) => lua.create_string(value).map(Value::String),
            Self::Array(values) => {
                let table = lua.create_table_with_capacity(values.len(), 0)?;
                for (offset, value) in values.iter().enumerate() {
                    table.raw_set(offset + 1, value.to_lua(lua)?)?;
                }
                mark_lua_array(lua, &table)?;
                Ok(Value::Table(table))
            }
            Self::Map(values) => {
                let table = lua.create_table_with_capacity(0, values.len())?;
                for (key, value) in values {
                    table.raw_set(key.as_str(), value.to_lua(lua)?)?;
                }
                Ok(Value::Table(table))
            }
        }
    }
}

pub(crate) fn mark_lua_array(lua: &Lua, table: &Table) -> mlua::Result<()> {
    let metatable = lua.create_table()?;
    metatable.raw_set(ARRAY_MARKER, true)?;
    metatable.raw_set("__metatable", false)?;
    table.set_metatable(Some(metatable))
}

#[cfg(test)]
mod tests {
    use mlua::Lua;

    use super::FrozenValue;

    #[test]
    fn freezes_maps_in_sorted_order() {
        let lua = Lua::new();
        let value = lua.load("return { z = 1, a = true }").eval().unwrap();
        let frozen = FrozenValue::from_lua(value).unwrap();
        let json = serde_json::to_string(&frozen).unwrap();

        assert_eq!(json, r#"{"a":true,"z":1}"#);
    }

    #[test]
    fn freezes_contiguous_arrays() {
        let lua = Lua::new();
        let value = lua.load("return { 'a', 'b' }").eval().unwrap();
        let frozen = FrozenValue::from_lua(value).unwrap();

        assert_eq!(
            frozen,
            FrozenValue::Array(vec![
                FrozenValue::String("a".into()),
                FrozenValue::String("b".into()),
            ])
        );
    }

    #[test]
    fn rejects_sparse_arrays() {
        let lua = Lua::new();
        let value = lua.load("return { [2] = 'b' }").eval().unwrap();
        let error = FrozenValue::from_lua(value).unwrap_err();

        assert!(error.to_string().contains("sparse Lua arrays"));
    }

    #[test]
    fn rejects_mixed_tables() {
        let lua = Lua::new();
        let value = lua.load("return { 'a', named = true }").eval().unwrap();
        let error = FrozenValue::from_lua(value).unwrap_err();

        assert!(error.to_string().contains("string-keyed maps"));
    }

    #[test]
    fn rejects_cycles() {
        let lua = Lua::new();
        let value = lua
            .load("local value = {}; value.self = value; return value")
            .eval()
            .unwrap();
        let error = FrozenValue::from_lua(value).unwrap_err();

        assert!(error.to_string().contains("cyclic Lua tables"));
    }

    #[test]
    fn thawed_values_do_not_share_tables() {
        let lua = Lua::new();
        let frozen = FrozenValue::Map(
            [(
                "nested".to_string(),
                FrozenValue::Map([("value".to_string(), FrozenValue::Integer(1))].into()),
            )]
            .into(),
        );

        let first = frozen.to_lua(&lua).unwrap();
        let second = frozen.to_lua(&lua).unwrap();

        assert_ne!(first.to_pointer(), second.to_pointer());
    }

    #[test]
    fn thawed_empty_arrays_keep_their_shape_when_frozen_again() {
        let lua = Lua::new();
        let thawed = FrozenValue::Array(Vec::new()).to_lua(&lua).unwrap();

        assert_eq!(
            FrozenValue::from_lua(thawed).unwrap(),
            FrozenValue::Array(Vec::new())
        );
    }
}
