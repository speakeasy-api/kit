//! Lossless, schema-independent editing of user configuration. No migrations or initialization.
use std::{io, path::Path};
use toml_edit::{DocumentMut, Item, Key, Table, Value};

fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error.to_string())
}

fn keys(key: &str) -> io::Result<Vec<Key>> {
    Key::parse(key).map_err(invalid)
}

fn parse(contents: &str) -> io::Result<DocumentMut> {
    contents
        .strip_prefix('\u{feff}')
        .unwrap_or(contents)
        .parse()
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid config TOML: {error}"),
            )
        })
}

/// Read a value as TOML, or the original document when no key is supplied.
/// Missing files and keys return NotFound. Reading never creates or rewrites anything.
pub fn get(path: &Path, key: Option<&str>) -> io::Result<String> {
    let key = key.map(keys).transpose()?;
    let contents = crate::config_files::read_to_string(path)?;
    let document = parse(&contents)?;
    let Some(keys) = key else { return Ok(contents) };
    let mut item = document.as_item();
    for key in keys {
        item = item
            .get(key.get())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "config key not found"))?;
    }
    // A keyed value is reusable TOML, not the surrounding assignment's comments.
    // Tables retain their document representation, including nested table headers.
    let mut item = item.clone();
    if let Item::Table(mut table) = item {
        // A selected dotted table is now the root, with no parent to emit its values.
        table.set_dotted(false);
        return Ok(DocumentMut::from(table).to_string().trim().to_owned());
    }
    if let Some(value) = item.as_value_mut() {
        value.decor_mut().clear();
    }
    Ok(item.to_string().trim().to_owned())
}

/// Set any dotted/quoted TOML key. Valid TOML values retain their type; otherwise
/// ordinary text is a string. Quote text with TOML quotes to force a string.
/// Invalid quoted/array/table or numeric-looking values are errors, not strings.
pub fn set(path: &Path, key: &str, value: &str) -> io::Result<()> {
    let keys = keys(key)?;
    let value = parse_value(value)?;
    update(path, true, |document| {
        put(document.as_item_mut(), &keys, Some(value))
    })
}

fn parse_value(input: &str) -> io::Result<Value> {
    let trimmed = input.trim();
    match trimmed.parse::<Value>() {
        Ok(value) => Ok(value),
        Err(error) => {
            if trimmed.starts_with(['"', '\'', '[', '{'])
                || trimmed.starts_with(|c: char| c.is_ascii_digit())
                || trimmed.starts_with(['+', '-'])
                    && trimmed.chars().nth(1).is_some_and(|c| c.is_ascii_digit())
            {
                Err(invalid(error))
            } else {
                Ok(Value::from(input))
            }
        }
    }
}

/// Remove a key or entire table. Missing files/keys are no-ops.
pub fn unset(path: &Path, key: &str) -> io::Result<()> {
    let keys = keys(key)?;
    update(path, false, |document| {
        put(document.as_item_mut(), &keys, None)
    })
}

// Apply TUI defaults together so a failed parse/edit cannot partially save a selection.
pub(crate) fn set_strings(path: &Path, values: &[(&str, Option<&str>)]) -> io::Result<()> {
    let values = values
        .iter()
        .map(|(key, value)| Ok((keys(key)?, value.map(Value::from))))
        .collect::<io::Result<Vec<_>>>()?;
    update(
        path,
        values.iter().any(|(_, value)| value.is_some()),
        |document| {
            let mut changed = false;
            for (keys, value) in values {
                changed |= put(document.as_item_mut(), &keys, value)?;
            }
            Ok(changed)
        },
    )
}

fn put(item: &mut Item, keys: &[Key], value: Option<Value>) -> io::Result<bool> {
    let (key, rest) = keys
        .split_first()
        .ok_or_else(|| invalid("empty config key"))?;
    let inline = item.is_inline_table();
    let Some(table) = item.as_table_like_mut() else {
        return if value.is_none() {
            Ok(false)
        } else {
            Err(invalid("config path crosses a non-table value"))
        };
    };
    if rest.is_empty() {
        if let Some(mut value) = value {
            if let Some(old) = table.get(key.get()).and_then(Item::as_value) {
                *value.decor_mut() = old.decor().clone();
            } else if let Some(old) = table.get(key.get()).and_then(Item::as_table) {
                let decor = old.decor().clone();
                // Header-leading comments belong before the new assignment, not its value.
                if let Some(prefix) = decor.prefix()
                    && let Some(mut key) = table.key_mut(key.get())
                {
                    key.leaf_decor_mut().set_prefix(prefix.clone());
                }
                if let Some(suffix) = decor.suffix() {
                    value.decor_mut().set_suffix(suffix.clone());
                }
            }
            if let Some(item) = table.get_mut(key.get()) {
                *item = Item::Value(value);
            } else {
                table.insert(key.get(), Item::Value(value));
            }
        } else {
            return Ok(table.remove(key.get()).is_some());
        }
        return Ok(true);
    }
    if !table.contains_key(key.get()) {
        if value.is_none() {
            return Ok(false);
        }
        let child = if inline {
            Item::Value(Value::InlineTable(Default::default()))
        } else {
            let mut table = Table::new();
            table.set_implicit(true);
            Item::Table(table)
        };
        table.insert(key.get(), child);
    }
    let child = table
        .get_mut(key.get())
        .ok_or_else(|| invalid("config key could not be inserted"))?;
    put(child, rest, value)
}

fn update(
    path: &Path,
    create: bool,
    edit: impl FnOnce(&mut DocumentMut) -> io::Result<bool>,
) -> io::Result<()> {
    let path = crate::config_files::resolve_for_write(path)?;
    let contents = match crate::resilient_fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if !create {
                return Ok(());
            }
            String::new()
        }
        Err(error) => return Err(error),
    };
    let mut document = parse(&contents)?;
    // Rendering normalizes some source formatting (notably CRLF), so an absent
    // unset must return before rendering rather than relying on byte comparison.
    if !edit(&mut document)? {
        return Ok(());
    }
    let output = format!(
        "{}{}",
        if contents.starts_with('\u{feff}') {
            "\u{feff}"
        } else {
            ""
        },
        document
    );
    // Validate the rendered document before touching disk, including table/dotted transitions.
    parse(&output)?;
    if output == contents {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| invalid("config path has no parent"))?;
    crate::resilient_fs::create_dir_all(parent)?;
    crate::resilient_fs::replace(&path, output.as_bytes())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests;
