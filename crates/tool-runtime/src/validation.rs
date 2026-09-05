//! 最小 JSON Schema 子集参数校验（§8.3.2 `input_schema`）。
//!
//! 只覆盖内置工具实际声明的能力子集：顶层 object、`required`、
//! `properties` 的基本类型（string/integer/number/boolean/array/object）。
//! 完整 JSON Schema 校验在 SDK / 插件体系落地时替换为标准实现。

use serde_json::Value;

use crate::ToolRuntimeError;

/// 校验 `args` 是否满足 `schema`（子集语义）；不满足返回 [`ToolRuntimeError::InvalidArguments`]。
pub fn validate_arguments(schema: &Value, args: &Value) -> Result<(), ToolRuntimeError> {
    let Some(obj) = args.as_object() else {
        return Err(ToolRuntimeError::InvalidArguments(
            "参数必须是 JSON object".into(),
        ));
    };

    if schema.get("type").and_then(Value::as_str) == Some("object") {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for field in required {
                let name = field.as_str().unwrap_or_default();
                if !obj.contains_key(name) {
                    return Err(ToolRuntimeError::InvalidArguments(format!(
                        "缺少必填参数: {name}"
                    )));
                }
            }
        }
        if let Some(props) = schema.get("properties").and_then(Value::as_object) {
            for (name, value) in obj {
                let Some(prop_schema) = props.get(name) else {
                    return Err(ToolRuntimeError::InvalidArguments(format!(
                        "未知参数: {name}"
                    )));
                };
                check_type(name, prop_schema, value)?;
            }
        }
    }
    Ok(())
}

fn check_type(name: &str, prop_schema: &Value, value: &Value) -> Result<(), ToolRuntimeError> {
    let expected = prop_schema.get("type").and_then(Value::as_str);
    let ok = match expected {
        Some("string") => value.is_string(),
        Some("integer") => value.is_i64() || value.is_u64(),
        Some("number") => value.is_number(),
        Some("boolean") => value.is_boolean(),
        Some("array") => value.is_array(),
        Some("object") => value.is_object(),
        Some("null") => value.is_null(),
        _ => true, // 未声明类型 → 放行（子集语义）
    };
    if ok {
        Ok(())
    } else {
        Err(ToolRuntimeError::InvalidArguments(format!(
            "参数 {name} 类型应为 {expected:?}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": { "type": "string" },
                "max_bytes": { "type": "integer" }
            }
        })
    }

    #[test]
    fn accepts_valid_arguments() {
        validate_arguments(&schema(), &json!({ "path": "src/main.rs" })).unwrap();
        validate_arguments(
            &schema(),
            &json!({ "path": "src/main.rs", "max_bytes": 1024 }),
        )
        .unwrap();
    }

    #[test]
    fn rejects_missing_unknown_and_mistyped() {
        assert!(validate_arguments(&schema(), &json!({})).is_err());
        assert!(validate_arguments(&schema(), &json!({ "path": "a", "extra": 1 })).is_err());
        assert!(validate_arguments(&schema(), &json!({ "path": 42 })).is_err());
        assert!(
            validate_arguments(&schema(), &json!({ "path": "a", "max_bytes": "big" })).is_err()
        );
        assert!(validate_arguments(&schema(), &json!("not-an-object")).is_err());
    }
}
