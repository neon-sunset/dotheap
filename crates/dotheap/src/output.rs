use serde::{
    Serialize, Serializer,
    ser::{SerializeMap, SerializeSeq},
};
use std::io::{self, Write};

#[derive(Clone, Debug)]
pub enum Value {
    Text(String),
    Int(serde_json::Number),
    Id(i64),
    Number(f64),
    Bool(bool),
    Bytes(u64),
    Address(u64),
    Null,
    List(Vec<Value>, usize),
    Record(Vec<(String, Value)>),
}

pub fn text(value: impl Into<String>) -> Value {
    Value::Text(value.into())
}
pub fn int(value: impl Into<serde_json::Number>) -> Value {
    Value::Int(value.into())
}
pub fn bytes(value: u64) -> Value {
    Value::Bytes(value)
}
pub fn address(value: u64) -> Value {
    Value::Address(value)
}
pub fn optional(value: Option<impl Into<String>>) -> Value {
    value.map(text).unwrap_or(Value::Null)
}
pub fn optional_address(value: Option<u64>) -> Value {
    value.map(address).unwrap_or(Value::Null)
}
pub fn record<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Record(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
}
pub fn list(items: impl IntoIterator<Item = Value>) -> Value {
    Value::List(items.into_iter().collect(), 0)
}
pub fn limited<T>(
    items: impl IntoIterator<Item = T>,
    limit: usize,
    mut convert: impl FnMut(T) -> Value,
) -> Value {
    let mut shown = Vec::new();
    let mut omitted = 0;
    for item in items {
        if shown.len() < limit {
            shown.push(convert(item));
        } else {
            omitted += 1;
        }
    }
    Value::List(shown, omitted)
}
pub fn try_limited<T>(
    items: impl IntoIterator<Item = T>,
    limit: usize,
    mut convert: impl FnMut(T) -> heap_core::Result<Value>,
) -> heap_core::Result<Value> {
    let mut shown = Vec::new();
    let mut omitted = 0;
    for item in items {
        if shown.len() < limit {
            shown.push(convert(item)?);
        } else {
            omitted += 1;
        }
    }
    Ok(Value::List(shown, omitted))
}

impl Value {
    pub fn add(&mut self, key: impl Into<String>, value: Value) {
        let Self::Record(fields) = self else {
            panic!("fields belong to records")
        };
        fields.push((key.into(), value));
    }
    pub fn is_scalar(&self) -> bool {
        !matches!(self, Self::List(..) | Self::Record(..))
    }
    pub fn scalar(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Int(n) => grouped(
                n.as_i64()
                    .map(i128::from)
                    .unwrap_or_else(|| i128::from(n.as_u64().unwrap())),
            ),
            Self::Id(n) => n.to_string(),
            Self::Number(n) => significant(*n, 3),
            Self::Bool(b) => if *b { "yes" } else { "no" }.into(),
            Self::Bytes(n) => size(*n),
            Self::Address(n) => hex(*n),
            Self::Null => "-".into(),
            _ => String::new(),
        }
    }
    pub fn text(&self) -> String {
        let mut lines = Vec::new();
        match self {
            Self::Record(fields) => write_record(fields, &mut lines, ""),
            Self::List(items, omitted) => write_list("", items, *omitted, &mut lines, ""),
            _ => lines.push(self.scalar()),
        }
        lines.join("\n")
    }
    pub fn print(&self, json: bool) -> io::Result<()> {
        let mut stdout = io::stdout().lock();
        if json {
            serde_json::to_writer_pretty(&mut stdout, self)?;
        } else {
            stdout.write_all(self.text().as_bytes())?;
        }
        writeln!(stdout)
    }
}

impl Serialize for Value {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Text(s) => serializer.serialize_str(s),
            Self::Int(n) => n.serialize(serializer),
            Self::Id(n) => serializer.serialize_i64(*n),
            Self::Number(n) => serializer.serialize_f64(*n),
            Self::Bool(b) => serializer.serialize_bool(*b),
            Self::Bytes(n) => serializer.serialize_u64(*n),
            Self::Address(n) => serializer.serialize_str(&hex(*n)),
            Self::Null => serializer.serialize_none(),
            Self::List(items, _) => {
                let mut seq = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Self::Record(fields) => {
                let mut map = serializer.serialize_map(None)?;
                for (key, value) in fields {
                    map.serialize_entry(key, value)?;
                    if let Self::List(_, omitted) = value
                        && *omitted > 0
                    {
                        map.serialize_entry(&format!("{key}Omitted"), omitted)?;
                    }
                }
                map.end()
            }
        }
    }
}

pub fn hex(value: u64) -> String {
    format!("0x{value:x}")
}
pub fn size(value: u64) -> String {
    let n = value as f64;
    if value < 1024 {
        format!("{value} B")
    } else if value < 1 << 20 {
        format!("{:.1} KB", n / 1024.)
    } else if value < 1 << 30 {
        format!("{:.1} MB", n / 1_048_576.)
    } else {
        format!("{:.2} GB", n / 1_073_741_824.)
    }
}
fn significant(value: f64, digits: usize) -> String {
    let mut buffer = [0u8; 64];
    let count = unsafe {
        libc::snprintf(
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            c"%.*g".as_ptr(),
            digits as i32,
            value,
        )
    };
    String::from_utf8_lossy(&buffer[..(count.max(0) as usize).min(buffer.len() - 1)]).into_owned()
}
pub fn grouped(value: i128) -> String {
    let digits = value.unsigned_abs().to_string();
    let mut result = if value < 0 { "-".into() } else { String::new() };
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            result.push(',');
        }
        result.push(ch);
    }
    result
}
fn label(key: &str) -> String {
    let chars: Vec<_> = key.chars().collect();
    let mut result = String::new();
    for (index, &ch) in chars.iter().enumerate() {
        let next_lower = chars.get(index + 1).is_some_and(|c| c.is_lowercase());
        if index > 0
            && ch.is_uppercase()
            && (chars[index - 1].is_lowercase()
                || chars[index - 1].is_numeric()
                || chars[index - 1].is_uppercase() && next_lower)
        {
            result.push(' ');
        }
        if ch.is_uppercase() && next_lower {
            result.extend(ch.to_lowercase());
        } else {
            result.push(ch);
        }
    }
    result
}
fn write_record(fields: &[(String, Value)], lines: &mut Vec<String>, indent: &str) {
    for (key, value) in fields {
        match value {
            Value::Null => {}
            Value::Record(inner) => {
                lines.push(format!("{indent}{}:", label(key)));
                write_record(inner, lines, &format!("{indent}  "));
            }
            Value::List(items, omitted) => write_list(key, items, *omitted, lines, indent),
            _ => lines.push(format!("{indent}{}: {}", label(key), value.scalar())),
        }
    }
}
fn write_list(key: &str, items: &[Value], omitted: usize, lines: &mut Vec<String>, indent: &str) {
    let key = label(key);
    let more = if omitted > 0 {
        format!(" ({omitted} more; raise --limit)")
    } else {
        String::new()
    };
    let joined = items
        .iter()
        .map(Value::scalar)
        .collect::<Vec<_>>()
        .join(", ");
    if !items.is_empty()
        && items.iter().all(|v| matches!(v, Value::Text(_)))
        && joined.chars().count() > 100
    {
        if !key.is_empty() {
            lines.push(format!("{indent}{key}:{more}"));
        }
        lines.extend(
            items
                .iter()
                .map(|item| format!("{indent}  - {}", item.scalar())),
        );
    } else if items.iter().all(Value::is_scalar) {
        lines.push(format!(
            "{indent}{}{}{more}",
            if key.is_empty() {
                String::new()
            } else {
                format!("{key}: ")
            },
            if items.is_empty() { "none" } else { &joined }
        ));
    } else {
        if !key.is_empty() {
            lines.push(format!("{indent}{key}:{more}"));
        }
        let records: Vec<_> = items
            .iter()
            .filter_map(|v| {
                if let Value::Record(r) = v {
                    Some(r.clone())
                } else {
                    None
                }
            })
            .collect();
        if records.len() == items.len()
            && records
                .iter()
                .all(|r| r.iter().all(|(k, v)| k == "children" || v.is_scalar()))
        {
            let rows = if records
                .iter()
                .any(|r| r.iter().any(|(k, _)| k == "children"))
            {
                tree(items, omitted)
            } else {
                records
            };
            table(&rows, lines, &format!("{indent}  "));
        } else {
            for item in items {
                match item {
                    Value::Record(fields) => {
                        let mut block = Vec::new();
                        write_record(fields, &mut block, &format!("{indent}    "));
                        if let Some(first) = block.first() {
                            lines.push(format!("{indent}  - {}", &first[indent.len() + 4..]));
                        }
                        lines.extend(block.into_iter().skip(1));
                    }
                    Value::List(items, omitted) => {
                        write_list("", items, *omitted, lines, &format!("{indent}  "))
                    }
                    _ => lines.push(format!("{indent}  - {}", item.scalar())),
                }
            }
        }
    }
}
fn tree(items: &[Value], omitted: usize) -> Vec<Vec<(String, Value)>> {
    let mut rows = Vec::new();
    let mut pending = vec![(items, omitted, String::new(), true, 0usize, "-".to_owned())];
    while let Some((items, omitted, prefix, top, index, mut last_key)) = pending.pop() {
        if index == items.len() {
            if omitted > 0 {
                rows.push(vec![(
                    last_key,
                    text(format!(
                        "{prefix}{}({omitted} more; raise --limit)",
                        if top { "" } else { "└─ " }
                    )),
                )]);
            }
            continue;
        }
        let last = index + 1 == items.len() && omitted == 0;
        let Value::Record(record) = &items[index] else {
            continue;
        };
        let mut fields: Vec<_> = record
            .iter()
            .filter(|(key, _)| key != "children")
            .cloned()
            .collect();
        if let Some(i) = fields
            .iter()
            .position(|(_, v)| matches!(v, Value::Text(_)))
            .or_else(|| fields.len().checked_sub(1))
        {
            last_key = fields[i].0.clone();
            fields[i].1 = text(format!(
                "{prefix}{}{}",
                if top {
                    ""
                } else if last {
                    "└─ "
                } else {
                    "├─ "
                },
                fields[i].1.scalar()
            ));
        }
        rows.push(fields);
        pending.push((items, omitted, prefix.clone(), top, index + 1, last_key));
        if let Some((_, Value::List(children, more))) =
            record.iter().find(|(key, _)| key == "children")
        {
            let next_prefix = if top {
                String::new()
            } else {
                format!("{prefix}{}", if last { "   " } else { "│  " })
            };
            pending.push((children, *more, next_prefix, false, 0, "-".into()));
        }
    }
    rows
}
fn table(records: &[Vec<(String, Value)>], lines: &mut Vec<String>, indent: &str) {
    let mut keys = Vec::new();
    for record in records {
        for (key, _) in record {
            if !keys.contains(key) {
                keys.push(key.clone());
            }
        }
    }
    keys.retain(|key| {
        records
            .iter()
            .any(|r| r.iter().any(|(k, v)| k == key && !matches!(v, Value::Null)))
    });
    let cells: Vec<Vec<String>> = records
        .iter()
        .map(|record| {
            keys.iter()
                .enumerate()
                .map(|(col, key)| {
                    let value = record
                        .iter()
                        .find(|(k, _)| k == key)
                        .map(|(_, v)| v.scalar())
                        .unwrap_or_else(|| "-".into());
                    if col + 1 < keys.len() && value.chars().count() > 80 {
                        let chars: Vec<_> = value.chars().collect();
                        format!(
                            "{}…{}",
                            chars[..39].iter().collect::<String>(),
                            chars[chars.len() - 40..].iter().collect::<String>()
                        )
                    } else {
                        value
                    }
                })
                .collect()
        })
        .collect();
    let numeric: Vec<_> = keys
        .iter()
        .map(|key| {
            records.iter().all(|record| {
                matches!(
                    record.iter().find(|(k, _)| k == key).map(|(_, v)| v),
                    None | Some(
                        Value::Int(_)
                            | Value::Id(_)
                            | Value::Number(_)
                            | Value::Bytes(_)
                            | Value::Null
                    )
                )
            })
        })
        .collect();
    let labels: Vec<_> = keys.iter().map(|key| label(key)).collect();
    let widths: Vec<_> = labels
        .iter()
        .enumerate()
        .map(|(col, label)| {
            cells
                .iter()
                .map(|row| row[col].chars().count())
                .max()
                .unwrap_or(0)
                .max(label.chars().count())
        })
        .collect();
    for values in std::iter::once(&labels).chain(&cells) {
        let parts: Vec<_> = values
            .iter()
            .enumerate()
            .map(|(col, value)| {
                let pad = " ".repeat(widths[col].saturating_sub(value.chars().count()));
                if numeric[col] {
                    format!("{pad}{value}")
                } else if col + 1 == values.len() {
                    value.clone()
                } else {
                    format!("{value}{pad}")
                }
            })
            .collect();
        lines.push(format!("{indent}{}", parts.join("  ")));
    }
}
