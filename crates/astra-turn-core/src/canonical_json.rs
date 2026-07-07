use serde_json::Value;

pub(crate) fn to_string(value: &Value) -> String {
    let mut out = String::new();
    write_value(value, &mut out);
    out
}

pub(crate) fn to_vec(value: &Value) -> Vec<u8> {
    to_string(value).into_bytes()
}

fn write_value(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => out.push_str(&number.to_string()),
        Value::String(string) => out.push_str(
            &serde_json::to_string(string).expect("serde_json can serialize string values"),
        ),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(
                    &serde_json::to_string(key).expect("serde_json can serialize object keys"),
                );
                out.push(':');
                write_value(&map[*key], out);
            }
            out.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn object_keys_are_sorted_recursively() {
        assert_eq!(
            to_string(&json!({"outer":{"b":2,"a":1}})),
            to_string(&json!({"outer":{"a":1,"b":2}}))
        );
    }

    #[test]
    fn array_order_is_preserved() {
        assert_ne!(to_string(&json!([1, 2, 3])), to_string(&json!([3, 2, 1])));
    }

    #[test]
    fn bytes_match_string_encoding() {
        let value = json!({"b":"two","a":[true,null]});
        assert_eq!(to_vec(&value), to_string(&value).into_bytes());
    }
}
