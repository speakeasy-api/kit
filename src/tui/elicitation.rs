//! Bounded, local-only ACP forms. No answer is sent before the review page.
use agent_client_protocol::schema::v2 as wire;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use serde_json::{Map, Value};

use super::editor::Editor;
use std::sync::Arc;

const MAX_TEXT: usize = 4096;

#[derive(Debug)]
pub(super) struct Form {
    message: String,
    fields: Vec<Field>,
    require_answer: Vec<AnswerRequirement>,
    pages: Vec<usize>,
    other_focus: bool,
    page: usize,
    scroll: u16,
    error: Option<String>,
    editor_width: usize,
    clipboard_identity: Arc<()>,
}

// The marker is private immutable ownership, not shared mutable state. A
// clipboard operation retains it, so a replacement form can never reuse its
// identity. Focused field and app route epoch fence navigation and intervening edits.
#[derive(Clone, Debug)]
pub(super) struct ClipboardTarget {
    identity: Arc<()>,
    field: usize,
}
impl PartialEq for ClipboardTarget {
    fn eq(&self, other: &Self) -> bool {
        self.field == other.field && Arc::ptr_eq(&self.identity, &other.identity)
    }
}
impl Eq for ClipboardTarget {}

#[derive(Debug)]
struct AnswerRequirement {
    selection: usize,
    custom: usize,
}

#[derive(Debug)]
struct Field {
    name: String,
    schema: Value,
    required: bool,
    kind: String,
    options: Vec<Choice>,
    cursor: usize,
    value: Value,
    editor: Editor,
}

#[derive(Debug)]
struct Choice {
    value: String,
    label: String,
    description: String,
}

// Keep wire values intact; strip terminal and directional controls only at display.
fn display(text: &str) -> String {
    text.chars().filter(|c| (!c.is_control() || matches!(c, '\n' | '\t')) && !matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')).collect()
}

fn annotation(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn keys(v: &Value, allowed: &[&str]) -> Result<(), String> {
    let object = v.as_object().ok_or("Expected a schema object")?;
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!("Unsupported schema keyword: {key}"));
        }
    }
    Ok(())
}

fn bounded(v: &Value) -> Result<(), String> {
    match v {
        Value::String(s) if s.chars().count() > MAX_TEXT => {
            return Err("Text exceeds 4096 characters".into());
        }
        Value::Array(a) => {
            for v in a {
                bounded(v)?;
            }
        }
        Value::Object(o) => {
            for (k, v) in o {
                if k.chars().count() > MAX_TEXT {
                    return Err("Field name too long".into());
                }
                bounded(v)?;
            }
        }
        _ => {}
    }
    Ok(())
}

impl Field {
    fn insert(&mut self, text: &str) {
        // Normalize paste exactly like the composer before applying the form limit.
        let mut normalized = Editor::default();
        normalized.insert_str(text);
        let remaining = MAX_TEXT.saturating_sub(self.editor.text().chars().count());
        let text: String = normalized.text().chars().take(remaining).collect();
        if !text.is_empty() {
            self.editor.insert_str(&text);
            self.sync_text();
        }
    }

    fn sync_text(&mut self) {
        self.value = if self.editor.text().is_empty() {
            Value::Null
        } else if self.kind == "string" {
            Value::String(self.editor.text().to_owned())
        } else {
            self.value.clone()
        };
    }

    fn edit(&mut self, key: KeyEvent, width: usize) -> bool {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let command = key.modifiers.contains(KeyModifiers::SUPER);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let previous = self.editor.text().to_owned();
        match key.code {
            KeyCode::Enter if shift || alt || control => self.insert("\n"),
            KeyCode::Char('j') if control => self.insert("\n"),
            KeyCode::Tab => self.insert("    "),
            KeyCode::Backspace if command => self.editor.delete_to_line_start(),
            KeyCode::Backspace if alt || control => self.editor.delete_word_back(),
            KeyCode::Backspace => self.editor.backspace(),
            KeyCode::Delete if alt || control => self.editor.delete_word_forward(),
            KeyCode::Delete => self.editor.delete_forward(),
            KeyCode::Char('w') if control => self.editor.delete_word_back(),
            KeyCode::Char('u') if control => self.editor.delete_to_line_start(),
            KeyCode::Char('k') if control => self.editor.delete_to_line_end(),
            KeyCode::Char('d') if alt => self.editor.delete_word_forward(),
            KeyCode::Char('b') if alt => self.editor.move_word_left(),
            KeyCode::Char('f') if alt => self.editor.move_word_right(),
            KeyCode::Char('a') if control => self.editor.move_line_start(),
            KeyCode::Char('e') if control => self.editor.move_line_end(),
            KeyCode::Left if command || control => self.editor.move_line_start(),
            KeyCode::Right if command || control => self.editor.move_line_end(),
            KeyCode::Left if alt => self.editor.move_word_left(),
            KeyCode::Right if alt => self.editor.move_word_right(),
            KeyCode::Left => self.editor.move_left(),
            KeyCode::Right => self.editor.move_right(),
            KeyCode::Home => self.editor.move_line_start(),
            KeyCode::End => self.editor.move_line_end(),
            KeyCode::Up => {
                self.editor.move_row_up(width);
            }
            KeyCode::Down => {
                self.editor.move_row_down(width);
            }
            KeyCode::Char(c) if !control && !command => self.insert(&c.to_string()),
            _ => return false,
        }
        if previous != self.editor.text() {
            self.sync_text();
        }
        true
    }

    fn new(name: String, schema: Value, required: bool) -> Result<Self, String> {
        let kind = annotation(&schema, "type");
        let mut allowed = vec!["type", "title", "description", "default", "_meta"];
        match kind.as_str() {
            "string" => allowed.extend(["minLength", "maxLength", "enum", "oneOf"]),
            "number" | "integer" => allowed.extend(["minimum", "maximum"]),
            "boolean" => {}
            "array" => allowed.extend(["items", "minItems", "maxItems"]),
            _ => return Err(format!("Unsupported field type: {kind}")),
        }
        keys(&schema, &allowed)?;
        for key in ["title", "description"] {
            if !schema[key].is_null() && !schema[key].is_string() {
                return Err(format!("Expected string {key}"));
            }
        }
        for key in ["minLength", "maxLength", "minItems", "maxItems"] {
            if !schema[key].is_null() && schema[key].as_u64().is_none() {
                return Err(format!("Expected nonnegative integer {key}"));
            }
        }
        for key in ["minimum", "maximum"] {
            if !schema[key].is_null()
                && (schema[key].as_f64().is_none()
                    || (kind == "integer" && schema[key].as_i64().is_none()))
            {
                return Err(format!("Invalid numeric bound {key}"));
            }
        }
        let default = &schema["default"];
        if !default.is_null()
            && !match kind.as_str() {
                "string" => default.is_string(),
                "number" => default.is_number(),
                "integer" => default.as_i64().is_some(),
                "boolean" => default.is_boolean(),
                "array" => default.is_array(),
                _ => false,
            }
        {
            return Err("Default has wrong type".into());
        }
        for (min, max) in [
            ("minLength", "maxLength"),
            ("minItems", "maxItems"),
            ("minimum", "maximum"),
        ] {
            if let (Some(a), Some(b)) = (schema[min].as_f64(), schema[max].as_f64())
                && a > b
            {
                return Err(format!("Invalid bounds for {name}"));
            }
        }
        if kind == "integer"
            && let (Some(min), Some(max)) = (schema["minimum"].as_i64(), schema["maximum"].as_i64())
            && min > max
        {
            return Err("Invalid integer bounds".into());
        }
        if schema["minLength"]
            .as_u64()
            .is_some_and(|n| n > MAX_TEXT as u64)
        {
            return Err("Minimum length exceeds form limit".into());
        }
        let choices = if kind == "array" {
            &schema["items"]
        } else {
            &schema
        };
        if kind == "array" {
            keys(choices, &["type", "enum", "anyOf", "_meta"])?;
            if choices["type"] != "string"
                && !(choices["type"].is_null() && choices["anyOf"].is_array())
            {
                return Err("Only string multiselect items are supported".into());
            }
        }
        let titled_key = if kind == "array" { "anyOf" } else { "oneOf" };
        for key in ["enum", titled_key] {
            if !choices[key].is_null() && !choices[key].is_array() {
                return Err(format!("Expected option array {key}"));
            }
        }
        if !choices["enum"].is_null() && !choices[titled_key].is_null() {
            return Err("Ambiguous enum schema".into());
        }
        let mut options = Vec::new();
        if let Some(values) = choices["enum"].as_array() {
            for value in values {
                let value = value.as_str().ok_or("Expected string option")?.to_owned();
                options.push(Choice {
                    label: value.clone(),
                    value,
                    description: String::new(),
                });
            }
        } else if let Some(values) = choices[titled_key].as_array() {
            for value in values {
                keys(value, &["const", "title", "description", "_meta"])?;
                options.push(Choice {
                    value: value["const"]
                        .as_str()
                        .ok_or("Expected string constant")?
                        .to_owned(),
                    label: value["title"]
                        .as_str()
                        .ok_or("Expected option title")?
                        .to_owned(),
                    description: annotation(value, "description"),
                });
            }
        }
        if options.len() > 64
            || ((kind == "array" || !choices["enum"].is_null() || !choices[titled_key].is_null())
                && options.is_empty())
        {
            return Err("Enums require 1–64 options".into());
        }
        for (i, option) in options.iter().enumerate() {
            if options[..i].iter().any(|o| o.value == option.value) {
                return Err("Duplicate option value".into());
            }
        }
        if schema["minItems"]
            .as_u64()
            .is_some_and(|n| n > options.len() as u64)
        {
            return Err("Minimum selections exceeds available options".into());
        }
        let value = schema.get("default").cloned().unwrap_or(Value::Null);
        let text = match &value {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => String::new(),
        };
        let cursor = options.iter().position(|o| value == o.value).unwrap_or(0);
        let field = Self {
            name,
            schema,
            required,
            kind,
            options,
            cursor,
            value,
            editor: {
                let mut editor = Editor::default();
                for c in text.chars() {
                    editor.insert_char(c);
                }
                editor
            },
        };
        if !field.value.is_null() {
            field.validate()?;
        }
        Ok(field)
    }

    fn answer(&self) -> Result<Value, String> {
        if self.options.is_empty() {
            match self.kind.as_str() {
                "string" => {
                    return Ok(
                        if self.editor.text().is_empty() && !self.required && self.value.is_null() {
                            Value::Null
                        } else {
                            Value::String(self.editor.text().to_owned())
                        },
                    );
                }
                "number" | "integer" => {
                    if self.editor.text().is_empty() {
                        return Ok(Value::Null);
                    }
                    let value: Value = serde_json::from_str(self.editor.text())
                        .map_err(|_| "Enter a valid number")?;
                    if !value.is_number() || (self.kind == "integer" && value.as_i64().is_none()) {
                        return Err("Enter a valid number of the requested type".into());
                    }
                    return Ok(value);
                }
                _ => {}
            }
        }
        Ok(self.value.clone())
    }

    fn validate(&self) -> Result<Value, String> {
        let value = self.answer()?;
        if value.is_null() {
            return if self.required {
                Err("An answer is required".into())
            } else {
                Ok(value)
            };
        }
        let (size, min, max) = match self.kind.as_str() {
            "string" => {
                let s = value.as_str().ok_or("Expected text")?;
                if !self.options.is_empty() && !self.options.iter().any(|o| o.value == s) {
                    return Err("Select a listed option".into());
                }
                (Some(s.chars().count() as u64), "minLength", "maxLength")
            }
            "array" => {
                let a = value.as_array().ok_or("Expected selections")?;
                for (i, v) in a.iter().enumerate() {
                    if !self
                        .options
                        .iter()
                        .any(|o| v == &Value::String(o.value.clone()))
                        || a[..i].contains(v)
                    {
                        return Err("Invalid or duplicate selection".into());
                    }
                }
                (Some(a.len() as u64), "minItems", "maxItems")
            }
            "boolean" => {
                if !value.is_boolean() {
                    return Err("Choose yes or no".into());
                }
                (None, "", "")
            }
            "integer" => {
                let n = value.as_i64().ok_or("Expected an integer")?;
                if self.schema["minimum"].as_i64().is_some_and(|min| n < min)
                    || self.schema["maximum"].as_i64().is_some_and(|max| n > max)
                {
                    return Err("Integer is outside the allowed range".into());
                }
                (None, "", "")
            }
            _ => {
                let n = value.as_f64().ok_or("Expected a number")?;
                if self.schema["minimum"].as_f64().is_some_and(|min| n < min)
                    || self.schema["maximum"].as_f64().is_some_and(|max| n > max)
                {
                    return Err("Number is outside the allowed range".into());
                }
                (None, "", "")
            }
        };
        if let Some(size) = size
            && (self.schema[min].as_u64().is_some_and(|n| size < n)
                || self.schema[max].as_u64().is_some_and(|n| size > n))
        {
            return Err("Answer does not meet the length or selection limits".into());
        }
        Ok(value)
    }

    fn selected(&self, option: &Choice) -> bool {
        self.value == option.value
            || self
                .value
                .as_array()
                .is_some_and(|a| a.contains(&Value::String(option.value.clone())))
    }

    fn toggle(&mut self) {
        if self.kind == "boolean" {
            self.value = Value::Bool(!self.value.as_bool().unwrap_or(false));
        } else if let Some(option) = self.options.get(self.cursor) {
            let value = Value::String(option.value.clone());
            if self.kind == "array" {
                let mut values = self.value.as_array().cloned().unwrap_or_default();
                if let Some(index) = values.iter().position(|v| *v == value) {
                    values.remove(index);
                } else {
                    values.push(value);
                }
                self.value = Value::Array(values);
            } else {
                self.value = value;
            }
        }
    }
}

fn answer_requirements(schema: &Value, fields: &[Field]) -> Result<Vec<AnswerRequirement>, String> {
    let Some(meta) = schema.get("_meta").filter(|meta| !meta.is_null()) else {
        return Ok(Vec::new());
    };
    let meta = meta.as_object().ok_or("Expected metadata object")?;
    let Some(questions) = meta.get("kit/questions") else {
        return Ok(Vec::new());
    };
    let questions = questions
        .as_object()
        .ok_or("Expected question metadata object")?;
    let Some(groups) = questions.get("require_answer") else {
        return Ok(Vec::new());
    };
    let groups = groups
        .as_array()
        .ok_or("Expected answer requirement array")?;
    if groups.len() > fields.len() {
        return Err("Too many answer requirements".into());
    }
    let mut requirements: Vec<AnswerRequirement> = Vec::new();
    for group in groups {
        keys(group, &["selection", "custom"])?;
        let selection = group["selection"]
            .as_str()
            .ok_or("Expected selection field name")?;
        let custom = group["custom"]
            .as_str()
            .ok_or("Expected custom field name")?;
        let selection = fields
            .iter()
            .position(|field| field.name == selection)
            .ok_or("Unknown selection field")?;
        let custom = fields
            .iter()
            .position(|field| field.name == custom)
            .ok_or("Unknown custom field")?;
        if fields[selection].options.is_empty()
            || fields[custom].kind != "string"
            || !fields[custom].options.is_empty()
            || requirements
                .iter()
                .any(|rule| rule.selection == selection || rule.custom == custom)
        {
            return Err("Invalid or repeated answer requirement".into());
        }
        requirements.push(AnswerRequirement { selection, custom });
    }
    Ok(requirements)
}

impl Form {
    fn custom_field(&self, selection: usize) -> Option<usize> {
        self.require_answer
            .iter()
            .find(|r| r.selection == selection)
            .map(|r| r.custom)
    }
    fn focused_field(&self) -> Option<usize> {
        let selection = *self.pages.get(self.page)?;
        Some(if self.other_focus {
            self.custom_field(selection).unwrap_or(selection)
        } else {
            selection
        })
    }
    fn validate_page(&self, page: usize) -> Result<(), (usize, String)> {
        let selection = self.pages[page];
        self.fields[selection]
            .validate()
            .map_err(|e| (selection, e))?;
        if let Some(custom) = self.custom_field(selection) {
            let text = self.fields[custom].validate().map_err(|e| (custom, e))?;
            let selected = self.fields[selection]
                .options
                .iter()
                .any(|o| self.fields[selection].selected(o));
            if !selected && !text.as_str().is_some_and(|s| !s.trim().is_empty()) {
                return Err((
                    custom,
                    "Select an option or enter a non-blank custom answer.".into(),
                ));
            }
        }
        Ok(())
    }
    pub(super) fn clipboard_target(&self) -> Option<ClipboardTarget> {
        self.focused_field()
            .and_then(|index| self.fields.get(index))
            .filter(|field| {
                field.options.is_empty()
                    && matches!(field.kind.as_str(), "string" | "number" | "integer")
            })
            .and_then(|_| {
                Some(ClipboardTarget {
                    identity: Arc::clone(&self.clipboard_identity),
                    field: self.focused_field()?,
                })
            })
    }

    #[cfg(test)]
    pub(super) fn new(request: wire::CreateElicitationRequest) -> Result<Self, String> {
        Self::from_value(serde_json::to_value(request).map_err(|e| e.to_string())?)
    }

    pub(super) fn from_value(request: Value) -> Result<Self, String> {
        let encoded = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
        if encoded.len() > 128 * 1024 {
            return Err("Form exceeds 128 KiB".into());
        }
        bounded(&request)?;
        if !request["message"].is_string() {
            return Err("Expected request message".into());
        }
        if request["mode"] != "form" {
            return Err("Only form elicitation is supported".into());
        }
        let schema = &request["requestedSchema"];
        keys(
            schema,
            &[
                "type",
                "title",
                "description",
                "properties",
                "required",
                "_meta",
            ],
        )?;
        if schema["type"] != "object" {
            return Err("Expected object schema".into());
        }
        let properties = schema["properties"]
            .as_object()
            .ok_or("Expected properties")?;
        if properties.len() > 32 {
            return Err("Form exceeds 32 fields".into());
        }
        if !schema["required"].is_null() && !schema["required"].is_array() {
            return Err("Expected required array".into());
        }
        for key in ["title", "description"] {
            if !schema[key].is_null() && !schema[key].is_string() {
                return Err(format!("Expected string {key}"));
            }
        }
        let required = schema["required"].as_array().cloned().unwrap_or_default();
        for name in &required {
            if !name.as_str().is_some_and(|n| properties.contains_key(n)) {
                return Err("Unknown required field".into());
            }
        }
        let mut names = Vec::new();
        let order = &schema["_meta"]["kit/questions"]["order"];
        if !order.is_null() && !order.is_array() {
            return Err("Expected question order array".into());
        }
        if let Some(order) = order.as_array() {
            for name in order {
                let name = name.as_str().ok_or("Invalid question order")?;
                if !properties.contains_key(name) || names.contains(&name.to_owned()) {
                    return Err("Invalid question order".into());
                }
                names.push(name.to_owned());
            }
        }
        for name in properties.keys() {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        let fields = names
            .into_iter()
            .map(|name| {
                let required = required.contains(&Value::String(name.clone()));
                Field::new(name.clone(), properties[&name].clone(), required)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let require_answer = answer_requirements(schema, &fields)?;
        let pages = (0..fields.len())
            .filter(|index| !require_answer.iter().any(|r| r.custom == *index))
            .collect();
        Ok(Self {
            pages,
            other_focus: false,
            message: [
                annotation(&request, "message"),
                annotation(schema, "title"),
                annotation(schema, "description"),
            ]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
            fields,
            require_answer,
            page: 0,
            scroll: 0,
            error: None,
            editor_width: 40,
            clipboard_identity: Arc::new(()),
        })
    }

    pub(super) fn paste(&mut self, text: &str) {
        if let Some(field) = self
            .focused_field()
            .and_then(|index| self.fields.get_mut(index))
            && field.options.is_empty()
            && matches!(field.kind.as_str(), "string" | "number" | "integer")
        {
            field.insert(text);
            self.error = None;
        }
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> Option<wire::CreateElicitationResponse> {
        if key.kind == KeyEventKind::Release
            || (key.kind == KeyEventKind::Repeat && key.code == KeyCode::Enter)
        {
            return None;
        }
        if key.code == KeyCode::Esc {
            return Some(wire::CreateElicitationResponse::new(
                wire::ElicitationAction::Cancel,
            ));
        }
        if key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Some(wire::CreateElicitationResponse::new(
                wire::ElicitationAction::Decline,
            ));
        }
        if key.code == KeyCode::Tab
            && self
                .pages
                .get(self.page)
                .and_then(|i| self.custom_field(*i))
                .is_some()
        {
            self.other_focus = !self.other_focus;
            return None;
        }
        if let Some(field) = self
            .focused_field()
            .and_then(|index| self.fields.get_mut(index))
            && field.options.is_empty()
            && matches!(field.kind.as_str(), "string" | "number" | "integer")
            && field.edit(key, self.editor_width)
        {
            self.error = None;
            return None;
        }
        match key.code {
            KeyCode::BackTab => {
                self.other_focus = false;
                self.page = self.page.saturating_sub(1);
                self.scroll = 0;
                self.error = None;
            }
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(5),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(5),
            KeyCode::Enter => {
                let reviewing = self.page == self.pages.len();
                let pages = if reviewing {
                    0..self.pages.len()
                } else {
                    self.page..self.page + 1
                };
                for page in pages {
                    if let Err((field, error)) = self.validate_page(page) {
                        self.page = page;
                        self.other_focus = self.custom_field(self.pages[page]) == Some(field);
                        self.scroll = 0;
                        self.error = Some(error);
                        return None;
                    }
                }
                if reviewing {
                    let mut content = Map::new();
                    for field in &self.fields {
                        if let Ok(value) = field.validate()
                            && !value.is_null()
                        {
                            content.insert(field.name.clone(), value);
                        }
                    }
                    return serde_json::from_value(Value::Object(Map::from_iter([
                        ("action".into(), Value::String("accept".into())),
                        ("content".into(), Value::Object(content)),
                    ])))
                    .ok();
                }
                self.page += 1;
                self.other_focus = false;
                self.scroll = 0;
                self.error = None;
            }
            _ => {
                if let Some(field) = self
                    .focused_field()
                    .and_then(|index| self.fields.get_mut(index))
                {
                    match key.code {
                        KeyCode::Up => {
                            field.cursor = field.cursor.saturating_sub(1);
                            self.scroll = 0;
                        }
                        KeyCode::Down => {
                            field.cursor =
                                (field.cursor + 1).min(field.options.len().saturating_sub(1));
                            self.scroll = 0;
                        }
                        KeyCode::Left if field.kind == "boolean" => {
                            field.value = Value::Bool(false)
                        }
                        KeyCode::Right if field.kind == "boolean" => {
                            field.value = Value::Bool(true)
                        }
                        KeyCode::Char(' ')
                            if !field.options.is_empty() || field.kind == "boolean" =>
                        {
                            field.toggle()
                        }
                        KeyCode::Delete => {
                            field.value = Value::Null;
                            field.editor.clear();
                        }
                        _ => {}
                    }
                }
            }
        }
        None
    }

    #[cfg(test)]
    pub(super) fn render(&mut self, frame: &mut Frame) {
        self.render_in(frame, frame.area());
    }

    pub(super) fn render_in(&mut self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(super::theme::accent());
        let inner = block.inner(area);
        self.editor_width = usize::from(inner.width).max(1);
        frame.render_widget(block, area);
        let parts = Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).split(inner);
        let mut text = format!("{}\n\n", self.message);
        if let Some(&selection) = self.pages.get(self.page) {
            let field = &self.fields[selection];
            let title = annotation(&field.schema, "title");
            text.push_str(&format!(
                "Question {}/{} — {} {}\n{}\n",
                self.page + 1,
                self.pages.len(),
                if title.is_empty() { "Answer" } else { &title },
                if field.required || self.custom_field(selection).is_some() {
                    "(required)"
                } else {
                    "(optional)"
                },
                annotation(&field.schema, "description")
            ));
            for key in [
                "minLength",
                "maxLength",
                "minimum",
                "maximum",
                "minItems",
                "maxItems",
            ] {
                if !field.schema[key].is_null() {
                    text.push_str(&format!("{key}: {}  ", field.schema[key]));
                }
            }
            text.push('\n');
            if !field.options.is_empty() {
                let custom = self.custom_field(selection);
                text.push_str(if self.other_focus {
                    "Focus: Other (Tab: choices)\n"
                } else if custom.is_some() {
                    "Focus: choices (Tab: Other)\n"
                } else {
                    "Focus: choices\n"
                });
                if let Some(custom) = custom {
                    let other = &self.fields[custom];
                    text.push_str(&format!(
                        "Other: {}\n",
                        annotation(&other.schema, "description")
                    ));
                    for key in ["minLength", "maxLength"] {
                        if !other.schema[key].is_null() {
                            text.push_str(&format!("Other {key}: {}\n", other.schema[key]));
                        }
                    }
                    if other.required {
                        text.push_str("Other text is required.\n");
                    }
                }
                let choice_area = if let Some(custom) = custom {
                    let areas = Layout::vertical([Constraint::Min(1), Constraint::Length(4)])
                        .split(parts[0]);
                    let other = &self.fields[custom];
                    let editor_area = Layout::vertical([Constraint::Length(1), Constraint::Min(0)])
                        .split(areas[1]);
                    frame.render_widget(
                        Paragraph::new(if self.other_focus {
                            "> Other"
                        } else {
                            "  Other"
                        }),
                        editor_area[0],
                    );
                    let (lines, (row, column)) = other.editor.wrapped(self.editor_width);
                    let first =
                        row.saturating_sub(usize::from(editor_area[1].height).saturating_sub(1));
                    frame.render_widget(
                        Paragraph::new(display(&lines.join("\n"))).scroll((first as u16, 0)),
                        editor_area[1],
                    );
                    if self.other_focus && editor_area[1].height > 0 && editor_area[1].width > 0 {
                        frame.set_cursor_position((
                            editor_area[1].x + (column as u16).min(editor_area[1].width - 1),
                            editor_area[1].y + (row - first) as u16,
                        ));
                    }
                    areas[0]
                } else {
                    parts[0]
                };
                let option = &field.options[field.cursor];
                text.push_str(&format!(
                    "Highlighted: {}\n{}\n",
                    option.label, option.description
                ));
                let regions =
                    Layout::vertical([Constraint::Percentage(60), Constraint::Percentage(40)])
                        .split(choice_area);
                render_text(frame, regions[0], text, &mut self.scroll);
                let height = usize::from(regions[1].height).max(1);
                let start = field.cursor.saturating_sub(height - 1);
                let lines = field
                    .options
                    .iter()
                    .enumerate()
                    .skip(start)
                    .take(height)
                    .map(|(i, o)| {
                        format!(
                            "{} [{}] {}",
                            if i == field.cursor && !self.other_focus {
                                ">"
                            } else {
                                " "
                            },
                            if field.selected(o) { "x" } else { " " },
                            o.label
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                frame.render_widget(Paragraph::new(display(&lines)), regions[1]);
            } else if field.kind != "boolean" {
                let regions =
                    Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).split(parts[0]);
                render_text(frame, regions[0], text, &mut self.scroll);
                let (lines, (row, column)) = field.editor.wrapped(self.editor_width);
                let first = row.saturating_sub(usize::from(regions[1].height).saturating_sub(1));
                frame.render_widget(
                    Paragraph::new(display(&lines.join("\n"))).scroll((first as u16, 0)),
                    regions[1],
                );
                if regions[1].height > 0 && regions[1].width > 0 {
                    frame.set_cursor_position((
                        regions[1].x + (column as u16).min(regions[1].width - 1),
                        regions[1].y + row.saturating_sub(first) as u16,
                    ));
                }
            } else {
                text.push_str(&format!(
                    "\nAnswer: {}",
                    if field.kind == "boolean" {
                        match field.value.as_bool() {
                            Some(true) => "Yes",
                            Some(false) => "No",
                            None => "Not answered (← No / → Yes)",
                        }
                    } else {
                        field.editor.text()
                    }
                ));
                render_text(frame, parts[0], text, &mut self.scroll);
            }
        } else {
            text.push_str("Review answers — BackTab to modify; Enter to submit\n\n");
            for (index, &selection) in self.pages.iter().enumerate() {
                let field = &self.fields[selection];
                let value = field.answer().unwrap_or(Value::Null);
                let mut display = if value.is_null() {
                    "(omitted)".into()
                } else if !field.options.is_empty() {
                    field
                        .options
                        .iter()
                        .filter(|o| field.selected(o))
                        .map(|o| o.label.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                } else {
                    value.to_string()
                };
                if let Some(custom) = self.custom_field(selection) {
                    let custom = self.fields[custom].editor.text();
                    if !custom.trim().is_empty() {
                        if display == "(omitted)" || display.is_empty() {
                            display = custom.to_owned();
                        } else {
                            display.push_str(&format!("; Other: {custom}"));
                        }
                    }
                }
                let title = annotation(&field.schema, "title");
                let label = if title.is_empty() {
                    format!("Answer {}", index + 1)
                } else {
                    title
                };
                text.push_str(&format!("{label}: {display}\n"));
            }
            render_text(frame, parts[0], text, &mut self.scroll);
        }
        frame.render_widget(Paragraph::new(format!("{}\nEnter: next/submit • Shift+Tab: back • Space: select • Tab: focus\nEsc: cancel • Ctrl+D: decline • PgUp/PgDn: scroll", self.error.as_deref().unwrap_or(""))).wrap(Wrap { trim: false }), parts[1]);
    }
}

fn render_text(frame: &mut Frame, area: ratatui::layout::Rect, text: String, scroll: &mut u16) {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;

    // Hard-wrap graphemes ourselves: ratatui's rendered line count is unstable.
    // This also provides an exact scroll bound without enabling extra features.
    let text = display(&text).replace('\t', "    ");
    let width = usize::from(area.width).max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut column = 0;
    for grapheme in text.graphemes(true) {
        if grapheme == "\n" {
            lines.push(std::mem::take(&mut line));
            column = 0;
        } else {
            let size = grapheme.width();
            if column > 0 && column + size > width {
                lines.push(std::mem::take(&mut line));
                column = 0;
            }
            line.push_str(grapheme);
            column += size;
        }
    }
    lines.push(line);
    let max = lines.len().saturating_sub(usize::from(area.height));
    *scroll = (*scroll).min(max.min(u16::MAX as usize) as u16);
    let paragraph = Paragraph::new(lines.join("\n"));
    frame.render_widget(paragraph.scroll((*scroll, 0)), area);
}

#[cfg(test)]
#[allow(clippy::disallowed_macros, clippy::disallowed_methods)]
mod tests {
    use super::*;
    use serde_json::json;
    fn form(properties: Value, required: Value) -> Result<Form, String> {
        Form::new(serde_json::from_value(json!({"mode":"form", "scope":"session", "sessionId":"test", "message":"Please answer", "requestedSchema":{"type":"object", "properties":properties, "required":required}})).unwrap())
    }
    fn press(form: &mut Form, code: KeyCode) -> Option<wire::CreateElicitationResponse> {
        form.key(KeyEvent::new(code, KeyModifiers::NONE))
    }
    #[test]
    fn navigating_empty_and_whitespace_defaults_preserves_answers() {
        for value in ["", "   "] {
            let mut form = form(
                json!({"input":{"type":"string","default":value}}),
                json!([]),
            )
            .unwrap();
            for key in [KeyCode::Left, KeyCode::Right, KeyCode::Home, KeyCode::End] {
                press(&mut form, key);
                assert_eq!(form.fields[0].answer().unwrap(), json!(value));
            }
        }
        let mut form = form(json!({"input":{"type":"string"}}), json!([])).unwrap();
        form.paste("   ");
        assert_eq!(form.fields[0].answer().unwrap(), json!("   "));
    }

    #[test]
    fn text_fields_match_composer_editing_keys_and_cursor() {
        let keys = [
            (KeyCode::Backspace, KeyModifiers::SHIFT),
            (KeyCode::Backspace, KeyModifiers::ALT),
            (KeyCode::Backspace, KeyModifiers::CONTROL),
            (KeyCode::Backspace, KeyModifiers::SUPER),
            (KeyCode::Delete, KeyModifiers::NONE),
            (KeyCode::Delete, KeyModifiers::ALT),
            (KeyCode::Left, KeyModifiers::NONE),
            (KeyCode::Right, KeyModifiers::NONE),
            (KeyCode::Left, KeyModifiers::ALT),
            (KeyCode::Right, KeyModifiers::ALT),
            (KeyCode::Left, KeyModifiers::CONTROL),
            (KeyCode::Right, KeyModifiers::SUPER),
            (KeyCode::Home, KeyModifiers::NONE),
            (KeyCode::End, KeyModifiers::NONE),
            (KeyCode::Char('w'), KeyModifiers::CONTROL),
            (KeyCode::Char('u'), KeyModifiers::CONTROL),
            (KeyCode::Char('k'), KeyModifiers::CONTROL),
            (KeyCode::Char('b'), KeyModifiers::ALT),
            (KeyCode::Char('f'), KeyModifiers::ALT),
            (KeyCode::Char('d'), KeyModifiers::ALT),
            (KeyCode::Char('a'), KeyModifiers::CONTROL),
            (KeyCode::Char('e'), KeyModifiers::CONTROL),
            (KeyCode::Char('j'), KeyModifiers::CONTROL),
            (KeyCode::Char('界'), KeyModifiers::NONE),
            (KeyCode::Enter, KeyModifiers::SHIFT),
        ];
        for (code, modifiers) in keys {
            let mut form = form(json!({"input":{"type":"string"}}), json!([])).unwrap();
            let mut app =
                crate::tui::app::App::new(".".into(), "test".into(), "test".into(), String::new());
            form.paste("first café\nlast word");
            app.editor.insert_str("first café\nlast word");
            form.fields[0].editor.move_left();
            app.editor.move_left();
            let key = KeyEvent::new(code, modifiers);
            form.key(key);
            app.handle_key(key);
            assert_eq!(form.fields[0].editor.text(), app.editor.text(), "{key:?}");
            assert_eq!(
                form.fields[0].editor.cursor(),
                app.editor.cursor(),
                "{key:?}"
            );
        }
    }

    #[test]
    fn text_paste_inserts_at_cursor_and_normalizes_like_composer() {
        let mut form = form(json!({"input":{"type":"string"}}), json!([])).unwrap();
        form.paste("ab");
        press(&mut form, KeyCode::Left);
        form.paste("é\r\n\t界");
        assert_eq!(form.fields[0].editor.text(), "aé\n    界b");
        press(&mut form, KeyCode::Backspace);
        assert_eq!(form.fields[0].editor.text(), "aé\n    b");
        press(&mut form, KeyCode::Enter);
        let result = serde_json::to_value(press(&mut form, KeyCode::Enter).unwrap()).unwrap();
        assert_eq!(result["content"]["input"], "aé\n    b");
    }

    #[test]
    fn review_is_required_and_edits_are_preserved() {
        let mut f = form(
            json!({"name":{"type":"string", "minLength":2}}),
            json!(["name"]),
        )
        .unwrap();
        assert!(press(&mut f, KeyCode::Enter).is_none());
        assert_eq!(f.page, 0);
        f.paste("Ada");
        assert!(press(&mut f, KeyCode::Enter).is_none());
        assert_eq!(f.page, 1);
        press(&mut f, KeyCode::BackTab);
        assert_eq!(f.fields[0].editor.text(), "Ada");
        press(&mut f, KeyCode::Enter);
        let result = serde_json::to_value(press(&mut f, KeyCode::Enter).unwrap()).unwrap();
        assert_eq!(result["content"]["name"], "Ada");
    }
    #[test]
    fn native_titled_multiselect_roundtrip() {
        let field = wire::MultiSelectPropertySchema::titled(vec![wire::EnumOption::new(
            "stable",
            "Friendly label",
        )]);
        let schema = wire::ElicitationSchema::new().property("choice", field, true);
        let request = wire::CreateElicitationRequest::new(
            wire::ElicitationFormMode::new(wire::ElicitationSessionScope::new("test"), schema),
            "Choose",
        );
        let encoded = serde_json::to_value(&request).unwrap();
        assert!(encoded["requestedSchema"]["properties"]["choice"]["items"]["type"].is_null());
        let decoded = serde_json::from_value(encoded).unwrap();
        let mut form = Form::new(decoded).unwrap();
        press(&mut form, KeyCode::Char(' '));
        press(&mut form, KeyCode::Enter);
        let response = serde_json::to_value(press(&mut form, KeyCode::Enter).unwrap()).unwrap();
        assert_eq!(response["content"]["choice"], json!(["stable"]));
    }

    #[test]
    fn raw_constraints_are_not_lost_by_sdk_decoding() {
        let request = json!({"mode":"form", "sessionId":"test", "message":"Answer", "requestedSchema":{"type":"object", "properties":{"x":{"type":"string"}}}});
        for restriction in [
            json!({"const":"fixed"}),
            json!({"const":null}),
            json!({"minLength":-1}),
            json!({"default":false}),
            json!({"enum":"wrong"}),
        ] {
            let mut raw = request.clone();
            raw["requestedSchema"]["properties"]["x"]
                .as_object_mut()
                .unwrap()
                .extend(restriction.as_object().unwrap().clone());
            assert!(Form::from_value(raw).is_err());
        }
        let mut raw = request.clone();
        raw["requestedSchema"]["type"] = json!("array");
        assert!(Form::from_value(raw).is_err());
        let mut raw = request;
        raw["requestedSchema"]["additionalProperties"] = json!(false);
        assert!(Form::from_value(raw).is_err());
    }

    #[test]
    fn display_strips_controls_but_preserves_wire_text() {
        assert_eq!(
            display("safe\u{1b}[2J\u{202e}text\nline"),
            "safe[2Jtext\nline"
        );
        let mut form = form(
            json!({"x":{"type":"string","default":"a\u{1b}b"}}),
            json!([]),
        )
        .unwrap();
        assert_eq!(form.fields[0].validate().unwrap(), json!("a\u{1b}b"));
        press(&mut form, KeyCode::Backspace);
        assert_eq!(form.fields[0].validate().unwrap(), json!("a\u{1b}"));
    }

    #[test]
    fn stable_multiselect_values_and_defaults() {
        let mut f = form(json!({"pick":{"type":"array","items":{"type":"string","anyOf":[{"const":"a","title":"First"},{"const":"b","title":"Second"}]},"default":["b"],"minItems":1}}), json!(["pick"])).unwrap();
        press(&mut f, KeyCode::Char(' '));
        assert_eq!(f.fields[0].validate().unwrap(), json!(["b", "a"]));
        press(&mut f, KeyCode::Enter);
        press(&mut f, KeyCode::BackTab);
        assert_eq!(f.fields[0].validate().unwrap(), json!(["b", "a"]));
    }
    #[test]
    fn unsupported_constraints_and_invalid_defaults_fail_closed() {
        assert!(form(json!({"x":{"type":"string","pattern":"a+"}}), json!([])).is_err());
        assert!(form(json!({"x":{"type":"string","format":"email"}}), json!([])).is_err());
        assert!(
            form(
                json!({"x":{"type":"integer","minimum":3,"default":2}}),
                json!([])
            )
            .is_err()
        );
        assert!(form(json!({"x":{"type":"_future"}}), json!([])).is_err());
    }
    #[test]
    fn grouped_single_choice_keeps_generic_fields_and_inline_text() {
        let mut f = Form::from_value(json!({"mode":"form","message":"Choose","requestedSchema":{
            "type":"object","properties":{
                "custom":{"type":"string","description":"Add context","maxLength":20},
                "generic":{"type":"string"},
                "pick":{"type":"string","oneOf":[{"const":"a","title":"First"}]}
            },"_meta":{"kit/questions":{"order":["pick","custom","generic"],
                "require_answer":[{"selection":"pick","custom":"custom"}]}}
        }}))
        .unwrap();
        assert_eq!(f.pages.len(), 2);
        press(&mut f, KeyCode::Char(' '));
        press(&mut f, KeyCode::Tab);
        f.paste("context");
        let clipboard = f.clipboard_target();
        press(&mut f, KeyCode::Tab);
        assert_ne!(clipboard, f.clipboard_target());
        f.paste("ignored");
        press(&mut f, KeyCode::Enter);
        f.paste("unrelated");
        press(&mut f, KeyCode::Enter);
        press(&mut f, KeyCode::BackTab);
        assert_eq!(f.page, 1);
        press(&mut f, KeyCode::BackTab);
        assert_eq!(f.page, 0);
        press(&mut f, KeyCode::Enter);
        press(&mut f, KeyCode::Enter);
        let result = serde_json::to_value(press(&mut f, KeyCode::Enter).unwrap()).unwrap();
        assert_eq!(
            result["content"],
            json!({"pick":"a","custom":"context","generic":"unrelated"})
        );
    }

    #[test]
    fn answer_requirement_metadata_rejects_unsafe_references() {
        let base = json!({"mode":"form","message":"Choose","requestedSchema":{"type":"object","properties":{
            "pick":{"type":"string","enum":["a"]},"custom":{"type":"string"},"other":{"type":"string","enum":["b"]}
        },"_meta":{"kit/questions":{"require_answer":[]}}}});
        for requirement in [
            json!(null),
            json!({}),
            json!([{}]),
            json!([{"selection":"missing","custom":"custom"}]),
            json!([{"selection":"custom","custom":"pick"}]),
            json!([{"selection":"pick","custom":"other"}]),
            json!([{"selection":"pick","custom":"custom","unknown":true}]),
            json!([{"selection":"pick","custom":"custom"},{"selection":"pick","custom":"custom"}]),
        ] {
            let mut raw = base.clone();
            raw["requestedSchema"]["_meta"]["kit/questions"]["require_answer"] = requirement;
            assert!(Form::from_value(raw).is_err());
        }
        let mut raw = base;
        raw["requestedSchema"]["_meta"]["kit/questions"]["require_answer"] =
            json!([{"selection":"pick","custom":"custom"}]);
        raw["requestedSchema"]["properties"]["pick"]["default"] = json!("a");
        raw["requestedSchema"]["properties"]["custom"]["default"] = json!("   ");
        let mut form = Form::from_value(raw).unwrap();
        for _ in 0..3 {
            press(&mut form, KeyCode::Enter);
        }
        assert!(matches!(
            press(&mut form, KeyCode::Enter).unwrap().action,
            wire::ElicitationAction::Accept(_)
        ));
    }

    #[test]
    fn page_up_moves_immediately_after_overscrolling_prompts_descriptions_and_review() {
        fn render(form: &mut Form) -> String {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 12)).unwrap();
            terminal.draw(|frame| form.render(frame)).unwrap();
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect()
        }
        fn assert_movement(form: &mut Form) {
            for _ in 0..100 {
                press(form, KeyCode::PageDown);
            }
            let bottom = render(form);
            let bottom_offset = form.scroll;
            assert!(bottom_offset > 0);
            press(form, KeyCode::PageUp);
            let above = render(form);
            assert!(form.scroll < bottom_offset);
            assert_ne!(bottom, above);
        }
        let lines = |prefix: &str| {
            (0..40)
                .map(|i| format!("{prefix} line {i}\n"))
                .collect::<String>()
        };
        for (message, description) in [
            (lines("prompt"), String::new()),
            ("Please answer".into(), lines("description")),
        ] {
            let mut form = Form::from_value(json!({"mode":"form","message":message,"requestedSchema":{"type":"object","properties":{"name":{"type":"string","description":description,"default":"answer"}}}})).unwrap();
            assert_movement(&mut form);
        }
        let mut choice = Form::from_value(json!({"mode":"form","message":"Choose an option","requestedSchema":{"type":"object","properties":{"choice":{"type":"string","oneOf":[{"const":"a","title":"Option A","description":lines("option description")}],"default":"a"}}}})).unwrap();
        assert_movement(&mut choice);
        let properties: Map<String, Value> = (0..20)
            .map(|i| {
                (
                    format!("field_{i:02}"),
                    json!({"type":"string","default":format!("answer {i}")}),
                )
            })
            .collect();
        let mut form = Form::from_value(json!({"mode":"form","message":"Review these answers","requestedSchema":{"type":"object","properties":properties}})).unwrap();
        for _ in 0..20 {
            press(&mut form, KeyCode::Enter);
        }
        assert_eq!(form.page, form.fields.len());
        assert_movement(&mut form);
    }

    #[test]
    fn renders_choices_review_and_tiny_terminals() {
        let mut form = form(json!({"pick":{"type":"string","oneOf":[{"const":"first","title":"First choice","description":"A useful description"}],"default":"first"}}), json!(["pick"])).unwrap();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| form.render(frame)).unwrap();
        let visible = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(visible.contains("First choice"));
        assert!(visible.contains("Question 1/1"));
        press(&mut form, KeyCode::Enter);
        terminal.draw(|frame| form.render(frame)).unwrap();
        let visible = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(visible.contains("Review answers"));
        for (width, height) in [(1, 1), (8, 3), (30, 10)] {
            let mut tiny =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            tiny.draw(|frame| form.render(frame)).unwrap();
        }
    }

    #[test]
    fn bounds_optional_and_cancellation() {
        let mut f = form(json!({"x":{"type":"string"}}), json!([])).unwrap();
        f.paste(&"é".repeat(5000));
        assert_eq!(f.fields[0].editor.text().chars().count(), MAX_TEXT);
        f.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        press(&mut f, KeyCode::Enter);
        assert_eq!(
            serde_json::to_value(press(&mut f, KeyCode::Enter).unwrap()).unwrap()["content"],
            json!({})
        );
        assert_eq!(
            serde_json::to_value(press(&mut f, KeyCode::Esc).unwrap()).unwrap()["action"],
            "cancel"
        );
        let decline = f
            .key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
            .unwrap();
        assert_eq!(serde_json::to_value(decline).unwrap()["action"], "decline");
    }
}
