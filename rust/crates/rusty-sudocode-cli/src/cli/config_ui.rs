//! Interactive `/config` tree browser.
//!
//! Uses `FieldSchema` from `runtime::config_schema` as SSOT to drive
//! a tree-like navigation UI. Each level renders a DialPad/FuzzySelect
//! showing fields + current values; selecting a leaf dispatches to the
//! appropriate InputSlot variant for editing.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use runtime::config_schema::{self, ConfigInputKind, FieldSchema, FieldType};
use runtime::model_capabilities;

use crate::cli::args::load_sudocode_config_for_current_dir;
use crate::render::{ansi_fg, theme, DIM, RESET};
use crate::repl_ui::{self, OutputSender, QuestionOptionView, QuestionPromptView, UiCommandSender};
use crate::{LiveCli, SlashSelectionHandler};

// ── public entry point ──────────────────────────────────────────────

/// Build the top-level `/config` tree handler.
///
/// Returns a `SlashSelectionHandler` that shows a DialPad with two entries:
/// `[settings.json]` and `[sudocode.json]`.
pub(crate) fn build_config_tree_handler(
    ui: &UiCommandSender,
    settings_path: PathBuf,
    sudocode_path: PathBuf,
) -> SlashSelectionHandler {
    let items = vec!["settings.json".to_string(), "sudocode.json".to_string()];
    let question = QuestionPromptView {
        title: Some("Config".to_string()),
        description: Some("Select config file to browse".to_string()),
        index: 0,
        total: 1,
        prompt: "Config file".to_string(),
        options: vec![
            QuestionOptionView {
                label: format!("settings.json  {DIM}{}{RESET}", settings_path.display()),
                value: "settings.json".to_string(),
                description: None,
                recommended: false,
                is_navigable: true,
            },
            QuestionOptionView {
                label: format!("sudocode.json  {DIM}{}{RESET}", sudocode_path.display()),
                value: "sudocode.json".to_string(),
                description: None,
                recommended: false,
                is_navigable: true,
            },
        ],
        allow_custom_input: false,
        custom_input_hint: None,
        force_fuzzy_select: false,
        back_value: None,
    };
    ui.show_question(question);

    let ui2 = ui.clone();
    let picker_paths = (settings_path.clone(), sudocode_path.clone());
    SlashSelectionHandler(Box::new(move |answer: &str, _cli, _out| {
        let resolved = resolve_answer(answer, &items);
        match resolved.as_str() {
            "settings.json" => Some(show_schema_level(
                &ui2,
                config_schema::SETTINGS_SCHEMA,
                &settings_path,
                "settings.json",
                Vec::new(),
                None,
                picker_paths.clone(),
            )),
            "sudocode.json" => Some(show_schema_level(
                &ui2,
                config_schema::SUDOCODE_SCHEMA,
                &sudocode_path,
                "sudocode.json",
                Vec::new(),
                None,
                picker_paths.clone(),
            )),
            _ => None,
        }
    }))
}

// ── schema level navigation ─────────────────────────────────────────

/// Sentinel value used as the DialPad option for "← Back".
const BACK_VALUE: &str = "\x00__back__";

/// Show a DialPad/FuzzySelect for one level of the schema tree.
///
/// `parent_schema` + parent breadcrumb (one level shorter) are used to
/// build the "← Back" option. At the top file-level `parent_schema` is
/// `None` and Back returns to the file picker.
fn show_schema_level(
    ui: &UiCommandSender,
    schema: &'static [FieldSchema],
    file_path: &Path,
    file_label: &str,
    breadcrumb: Vec<String>,
    parent_schema: Option<&'static [FieldSchema]>,
    // Paths needed to rebuild the file picker on "Back" from top level.
    file_picker_paths: (PathBuf, PathBuf),
) -> SlashSelectionHandler {
    let json_content = std::fs::read_to_string(file_path).unwrap_or_else(|_| "{}".to_string());
    let json_root: serde_json::Value =
        serde_json::from_str(&json_content).unwrap_or(serde_json::json!({}));

    // Navigate to the nested object indicated by breadcrumb.
    let current_obj = navigate_json(&json_root, &breadcrumb);

    let visible: Vec<&FieldSchema> = schema.iter().filter(|f| !f.is_deprecated).collect();

    let mut keys: Vec<String> = Vec::new();
    let mut options: Vec<QuestionOptionView> = Vec::new();

    for f in &visible {
        let val_summary = current_obj
            .and_then(|obj| obj.get(f.key))
            .map_or_else(|| "(not set)".to_string(), |v| truncate_json_value(v));
        let is_object = f.children.is_some() && f.field_type == FieldType::Object;
        let hint = if is_object { " \u{25b8}" } else { "" };
        keys.push(f.key.to_string());
        options.push(QuestionOptionView {
            label: format!("{}{hint}  {DIM}{val_summary}{RESET}", f.key),
            value: f.key.to_string(),
            description: Some(f.description.to_string()),
            recommended: false,
            is_navigable: is_object,
        });
    }

    let title_path = if breadcrumb.is_empty() {
        file_label.to_string()
    } else {
        format!("{file_label} > {}", breadcrumb.join(" > "))
    };

    let question = QuestionPromptView {
        title: Some("Config".to_string()),
        description: Some(title_path),
        index: 0,
        total: 1,
        prompt: "Select field".to_string(),
        options,
        allow_custom_input: false,
        custom_input_hint: None,
        force_fuzzy_select: visible.len() > 9,
        back_value: Some(BACK_VALUE.to_string()),
    };
    ui.show_question(question);

    let ui2 = ui.clone();
    let file_path = file_path.to_path_buf();
    let file_label = file_label.to_string();
    let breadcrumb2 = breadcrumb.clone();

    SlashSelectionHandler(Box::new(move |answer: &str, cli, out| {
        let selected = resolve_answer(answer, &keys);

        // ← Back
        if selected == BACK_VALUE {
            return if let Some(parent) = parent_schema {
                // Go up one level.
                let mut parent_bc = breadcrumb2.clone();
                parent_bc.pop();
                Some(show_schema_level(
                    &ui2,
                    parent,
                    &file_path,
                    &file_label,
                    parent_bc,
                    None, // grandparent unknown — will go to file picker
                    file_picker_paths.clone(),
                ))
            } else {
                // At top file-level → back to file picker.
                Some(build_config_tree_handler(
                    &ui2,
                    file_picker_paths.0.clone(),
                    file_picker_paths.1.clone(),
                ))
            };
        }

        let Some(field) = schema.iter().find(|f| f.key == selected) else {
            out.println(&format!("{DIM}Unknown field: {selected}{RESET}"));
            return None;
        };

        // If field has children and is Object, drill in.
        if let Some(children) = field.children {
            if !children.is_empty() {
                let mut next_bc = breadcrumb2.clone();
                next_bc.push(field.key.to_string());
                return Some(show_schema_level(
                    &ui2,
                    children,
                    &file_path,
                    &file_label,
                    next_bc,
                    Some(schema),
                    file_picker_paths.clone(),
                ));
            }
        }

        // Leaf field — dispatch to appropriate editor.
        let input_kind = config_schema::resolve_input_kind(field);
        handle_leaf_edit(
            &ui2,
            field,
            &input_kind,
            &file_path,
            &file_label,
            &breadcrumb2,
            cli,
            out,
        )
    }))
}

// ── leaf field editing ──────────────────────────────────────────────

fn handle_leaf_edit(
    ui: &UiCommandSender,
    field: &'static FieldSchema,
    input_kind: &ConfigInputKind,
    file_path: &Path,
    file_label: &str,
    breadcrumb: &[String],
    _cli: &Arc<Mutex<LiveCli>>,
    out: &OutputSender,
) -> Option<SlashSelectionHandler> {
    let json_content = std::fs::read_to_string(file_path).unwrap_or_else(|_| "{}".to_string());
    let json_root: serde_json::Value =
        serde_json::from_str(&json_content).unwrap_or(serde_json::json!({}));
    let current_obj = navigate_json(&json_root, breadcrumb);
    let current_val = current_obj.and_then(|obj| obj.get(field.key));

    match input_kind {
        ConfigInputKind::BoolToggle => {
            let current_bool = current_val.and_then(|v| v.as_bool()).unwrap_or(false);
            let new_val = !current_bool;
            match write_config_value(file_path, breadcrumb, field.key, serde_json::json!(new_val)) {
                Ok(()) => out.println(&format!(
                    "{DIM}{}{} = {new_val}{RESET}",
                    breadcrumb_display(breadcrumb),
                    field.key
                )),
                Err(e) => out.println(&format!(
                    "{}Error writing config: {e}{}",
                    ansi_fg(theme().error),
                    RESET
                )),
            }
            None
        }
        ConfigInputKind::Enum(opts) => {
            let items: Vec<String> = opts.clone();
            let current_str = current_val.and_then(|v| v.as_str()).unwrap_or("");
            let options: Vec<QuestionOptionView> = items
                .iter()
                .map(|o| QuestionOptionView {
                    label: o.clone(),
                    value: o.clone(),
                    description: None,
                    recommended: o == current_str,
                    is_navigable: false,
                })
                .collect();
            let question = QuestionPromptView {
                title: Some("Config".to_string()),
                description: Some(format!(
                    "{file_label} > {}{}",
                    breadcrumb_display(breadcrumb),
                    field.key
                )),
                index: 0,
                total: 1,
                prompt: format!("Select value for {}", field.key),
                options,
                allow_custom_input: false,
                custom_input_hint: None,
                force_fuzzy_select: false,
                back_value: None,
            };
            ui.show_question(question);
            let file_path = file_path.to_path_buf();
            let breadcrumb = breadcrumb.to_vec();
            let field_key = field.key;
            Some(SlashSelectionHandler(Box::new(
                move |answer: &str, _cli, out| {
                    let selected = resolve_answer(answer, &items);
                    match write_config_value(
                        &file_path,
                        &breadcrumb,
                        field_key,
                        serde_json::json!(selected),
                    ) {
                        Ok(()) => out.println(&format!(
                            "{DIM}{}{field_key} = \"{selected}\"{RESET}",
                            breadcrumb_display(&breadcrumb)
                        )),
                        Err(e) => out.println(&format!(
                            "{}Error writing config: {e}{}",
                            ansi_fg(theme().error),
                            RESET
                        )),
                    }
                    None
                },
            )))
        }
        ConfigInputKind::DynamicList => {
            let sudocode_config = load_sudocode_config_for_current_dir();
            let config_keys: Vec<String> = sudocode_config.models.keys().cloned().collect();
            let models = model_capabilities::merge_discovery_ids(&config_keys);
            let current_str = current_val.and_then(|v| v.as_str()).unwrap_or("");
            let options: Vec<QuestionOptionView> = models
                .iter()
                .map(|m| QuestionOptionView {
                    label: m.clone(),
                    value: m.clone(),
                    description: None,
                    recommended: m == current_str,
                    is_navigable: false,
                })
                .collect();
            let question = QuestionPromptView {
                title: Some("Config".to_string()),
                description: Some(format!(
                    "{file_label} > {}{} (current: {current_str})",
                    breadcrumb_display(breadcrumb),
                    field.key
                )),
                index: 0,
                total: 1,
                prompt: format!("Select {}", field.key),
                options,
                allow_custom_input: true,
                custom_input_hint: Some("or type a model name".to_string()),
                force_fuzzy_select: true,
                back_value: None,
            };
            ui.show_question(question);
            let file_path = file_path.to_path_buf();
            let breadcrumb = breadcrumb.to_vec();
            let field_key = field.key;
            let items = models;
            Some(SlashSelectionHandler(Box::new(
                move |answer: &str, _cli, out| {
                    let selected = resolve_answer(answer, &items);
                    match write_config_value(
                        &file_path,
                        &breadcrumb,
                        field_key,
                        serde_json::json!(selected),
                    ) {
                        Ok(()) => out.println(&format!(
                            "{DIM}{}{field_key} = \"{selected}\"{RESET}",
                            breadcrumb_display(&breadcrumb)
                        )),
                        Err(e) => out.println(&format!(
                            "{}Error writing config: {e}{}",
                            ansi_fg(theme().error),
                            RESET
                        )),
                    }
                    None
                },
            )))
        }
        ConfigInputKind::Text | ConfigInputKind::NumberInput => {
            let current_str = current_val
                .map(|v| serde_json::to_string(v).unwrap_or_default())
                .unwrap_or_default();
            out.println(&format!(
                "{DIM}Current {}{} = {current_str}{RESET}",
                breadcrumb_display(breadcrumb),
                field.key
            ));
            out.println(&format!(
                "{DIM}Edit the file directly: {}{RESET}",
                file_path.display()
            ));
            None
        }
        ConfigInputKind::Editor => {
            out.println(&format!(
                "{DIM}Complex value \u{2014} edit the file directly: {}{RESET}",
                file_path.display()
            ));
            None
        }
    }
}

// ── JSON helpers ────────────────────────────────────────────────────

fn navigate_json<'a>(
    root: &'a serde_json::Value,
    breadcrumb: &[String],
) -> Option<&'a serde_json::Value> {
    let mut current = root;
    for key in breadcrumb {
        current = current.get(key.as_str())?;
    }
    Some(current)
}

fn truncate_json_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => {
            if s.len() > 35 {
                format!("\"{}…\"", &s[..35])
            } else {
                format!("\"{s}\"")
            }
        }
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Array(a) => format!("[{} items]", a.len()),
        serde_json::Value::Object(o) => format!("{{{} keys}}", o.len()),
    }
}

/// Build a dot-path prefix for display. Returns `"a.b."` or `""`.
fn breadcrumb_display(breadcrumb: &[String]) -> String {
    if breadcrumb.is_empty() {
        String::new()
    } else {
        format!("{}.", breadcrumb.join("."))
    }
}

fn resolve_answer(answer: &str, items: &[String]) -> String {
    answer
        .parse::<usize>()
        .ok()
        .and_then(|idx| items.get(idx.wrapping_sub(1)).cloned())
        .unwrap_or_else(|| answer.to_string())
}

/// Atomically write a value to a config JSON file at the given path.
pub(crate) fn write_config_value(
    file_path: &Path,
    breadcrumb: &[String],
    key: &str,
    value: serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(file_path).unwrap_or_else(|_| "{}".to_string());
    let mut root: serde_json::Value =
        serde_json::from_str(&content).unwrap_or(serde_json::json!({}));

    let mut current = &mut root;
    for segment in breadcrumb {
        if !current.is_object() {
            *current = serde_json::json!({});
        }
        current = current
            .as_object_mut()
            .unwrap()
            .entry(segment.clone())
            .or_insert_with(|| serde_json::json!({}));
    }

    if let Some(obj) = current.as_object_mut() {
        obj.insert(key.to_string(), value);
    }

    let tmp = file_path.with_extension("json.tmp");
    let pretty = serde_json::to_string_pretty(&root)?;
    std::fs::write(&tmp, pretty.as_bytes())?;
    std::fs::rename(&tmp, file_path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_config_value_top_level() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"model": "old"}"#).unwrap();

        write_config_value(&path, &[], "model", serde_json::json!("new")).unwrap();

        let result: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(result["model"], "new");
    }

    #[test]
    fn write_config_value_nested() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"sandbox": {"enabled": false}}"#).unwrap();

        let bc = vec!["sandbox".to_string()];
        write_config_value(&path, &bc, "enabled", serde_json::json!(true)).unwrap();

        let result: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(result["sandbox"]["enabled"], true);
    }

    #[test]
    fn write_config_value_creates_intermediaries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{}").unwrap();

        let bc = vec!["permissions".to_string()];
        write_config_value(&path, &bc, "defaultMode", serde_json::json!("plan")).unwrap();

        let result: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(result["permissions"]["defaultMode"], "plan");
    }

    #[test]
    fn write_config_value_creates_file_from_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.json");
        // File doesn't exist yet — write_config_value handles missing file.
        write_config_value(&path, &[], "model", serde_json::json!("test")).unwrap();

        let result: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(result["model"], "test");
    }

    #[test]
    fn navigate_json_empty_breadcrumb() {
        let root = serde_json::json!({"a": 1});
        let result = navigate_json(&root, &[]);
        assert_eq!(result, Some(&serde_json::json!({"a": 1})));
    }

    #[test]
    fn navigate_json_nested() {
        let root = serde_json::json!({"sandbox": {"enabled": true}});
        let bc = vec!["sandbox".to_string()];
        let result = navigate_json(&root, &bc);
        assert_eq!(result, Some(&serde_json::json!({"enabled": true})));
    }

    #[test]
    fn navigate_json_missing_key() {
        let root = serde_json::json!({"a": 1});
        let bc = vec!["b".to_string()];
        assert!(navigate_json(&root, &bc).is_none());
    }

    #[test]
    fn truncate_json_value_short_string() {
        assert_eq!(
            truncate_json_value(&serde_json::json!("hello")),
            "\"hello\""
        );
    }

    #[test]
    fn truncate_json_value_long_string() {
        let long = "a".repeat(50);
        let result = truncate_json_value(&serde_json::json!(long));
        assert!(result.ends_with("…\""));
        assert!(result.len() < 50);
    }

    #[test]
    fn truncate_json_value_object() {
        let obj = serde_json::json!({"a": 1, "b": 2});
        assert_eq!(truncate_json_value(&obj), "{2 keys}");
    }

    #[test]
    fn truncate_json_value_array() {
        let arr = serde_json::json!([1, 2, 3]);
        assert_eq!(truncate_json_value(&arr), "[3 items]");
    }

    #[test]
    fn resolve_answer_numeric() {
        let items = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(resolve_answer("1", &items), "a");
        assert_eq!(resolve_answer("3", &items), "c");
    }

    #[test]
    fn resolve_answer_text() {
        let items = vec!["a".to_string()];
        assert_eq!(resolve_answer("custom", &items), "custom");
    }

    #[test]
    fn breadcrumb_display_empty() {
        assert_eq!(breadcrumb_display(&[]), "");
    }

    #[test]
    fn breadcrumb_display_nested() {
        let bc = vec!["sandbox".to_string(), "inner".to_string()];
        assert_eq!(breadcrumb_display(&bc), "sandbox.inner.");
    }
}
