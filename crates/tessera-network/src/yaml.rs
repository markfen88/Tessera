//! YAML input without unsafe-libyaml. Netplan output uses JSON, which is a
//! YAML 1.2 subset, avoiding a second serializer and preserving typed values.

use serde_json::Value;
use yaml_rust2::{Yaml, YamlLoader};

use crate::Result;

/// One bounded, JSON-compatible Netplan document, with string mapping keys.
pub fn parse(text: &str) -> Result<Value> {
    let docs = YamlLoader::load_from_str(text).map_err(|e| e.to_string())?;
    if docs.len() != 1 {
        return Err("Expected one Netplan YAML document".into());
    }
    convert(&docs[0], 0)
}

fn convert(value: &Yaml, depth: u8) -> Result<Value> {
    if depth > 64 {
        return Err("Netplan configuration is too deeply nested".into());
    }
    match value {
        Yaml::Null => Ok(Value::Null),
        Yaml::Boolean(v) => Ok(Value::Bool(*v)),
        Yaml::Integer(v) => Ok(Value::Number((*v).into())),
        Yaml::Real(v) => v
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .ok_or_else(|| "Non-finite YAML number".into()),
        Yaml::String(v) => Ok(Value::String(v.clone())),
        Yaml::Array(values) => values
            .iter()
            .map(|v| convert(v, depth + 1))
            .collect::<Result<Vec<_>>>()
            .map(Value::Array),
        Yaml::Hash(values) => {
            let mut out = serde_json::Map::new();
            for (key, value) in values {
                let key = key.as_str().ok_or("Netplan mappings require string keys")?;
                out.insert(key.into(), convert(value, depth + 1)?);
            }
            Ok(Value::Object(out))
        }
        _ => Err("Unsupported YAML value in Netplan configuration".into()),
    }
}

/// Emit lossless typed JSON, accepted by Netplan's YAML parser.
pub fn emit(value: &Value) -> Result<String> {
    serde_json::to_string_pretty(value)
        .map(|s| s + "\n")
        .map_err(|e| e.to_string())
}
