//! The model question contract, shared by the engine and standalone tool runner.

use std::io::{BufRead, Write};

use runtime::{
    QuestionField, QuestionKind, QuestionOption, QuestionPromptAnswer, QuestionPromptRequest,
};
use serde::Deserialize;
use serde_json::{json, Value};

const DEFAULT_ALLOW_CUSTOM_INPUT: bool = true;

pub(crate) fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "title": { "type": "string" },
            "description": { "type": "string" },
            "questions": {
                "type": "array", "minItems": 1,
                "items": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "prompt": { "type": "string" },
                        "kind": { "type": "string", "enum": ["single_select", "multi_select", "text", "boolean"] },
                        "required": { "type": "boolean" },
                        "allowCustomInput": {
                            "type": "boolean", "default": DEFAULT_ALLOW_CUSTOM_INPUT,
                            "description": "Allow an answer outside the options. Defaults to true; set false for a closed choice. Text questions always accept input."
                        },
                        "customInputHint": { "type": "string" },
                        "options": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "label": { "type": "string" },
                                    "value": { "type": "string" },
                                    "description": { "type": "string" },
                                    "recommended": { "type": "boolean" }
                                },
                                "required": ["label", "value"], "additionalProperties": false
                            }
                        }
                    },
                    "required": ["id", "prompt"], "additionalProperties": false
                }
            }
        },
        "required": ["questions"], "additionalProperties": false
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct QuestionInput {
    title: Option<String>,
    description: Option<String>,
    questions: Vec<FieldInput>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FieldInput {
    id: String,
    prompt: String,
    kind: Option<String>,
    required: Option<bool>,
    allow_custom_input: Option<bool>,
    custom_input_hint: Option<String>,
    #[serde(default)]
    options: Vec<OptionInput>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OptionInput {
    label: String,
    value: String,
    description: Option<String>,
    #[serde(default)]
    recommended: bool,
}

/// Parse the sole model input format and resolve question policy before rendering.
pub fn parse_request(input: Value) -> Result<QuestionPromptRequest, String> {
    let input: QuestionInput = serde_json::from_value(input)
        .map_err(|error| format!("invalid tool input JSON: {error}"))?;
    if input.questions.is_empty() {
        return Err("questions must contain at least one question".to_string());
    }
    let fields = input
        .questions
        .into_iter()
        .map(|field| {
            let kind = field
                .kind
                .as_deref()
                .map(str::parse)
                .transpose()?
                .unwrap_or(if field.options.is_empty() {
                    QuestionKind::Text
                } else {
                    QuestionKind::SingleSelect
                });
            Ok(QuestionField {
                id: field.id,
                prompt: field.prompt,
                kind,
                required: field.required.unwrap_or(true),
                allow_custom_input: kind == QuestionKind::Text
                    || field.options.is_empty()
                    || field
                        .allow_custom_input
                        .unwrap_or(DEFAULT_ALLOW_CUSTOM_INPUT),
                custom_input_hint: field.custom_input_hint,
                options: field
                    .options
                    .into_iter()
                    .map(|option| QuestionOption {
                        label: option.label,
                        value: option.value,
                        description: option.description,
                        recommended: option.recommended,
                    })
                    .collect(),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(QuestionPromptRequest {
        presentation: runtime::QuestionPresentation::Question,
        title: input.title,
        description: input
            .description
            .map(|text| runtime::PromptText::Markdown(text.into())),
        fields,
    })
}

/// Return the normalized request and collected answers through the tool contract.
pub fn serialize_result(
    request: &QuestionPromptRequest,
    answers: &[QuestionPromptAnswer],
) -> Result<String, String> {
    serde_json::to_string_pretty(&json!({
        "status": "answered", "title": request.title,
        "description": request.description.as_ref().map(runtime::PromptText::as_str),
        "questions": request.fields.iter().map(|field| json!({
            "id": field.id, "prompt": field.prompt, "kind": field.kind.as_str(),
            "required": field.required, "allowCustomInput": field.allow_custom_input,
            "customInputHint": field.custom_input_hint,
            "options": field.options.iter().map(|option| json!({
                "label": option.label, "value": option.value,
                "description": option.description, "recommended": option.recommended,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "answers": answers.iter().map(|answer| json!({
            "id": answer.id, "value": answer.value, "label": answer.label,
        })).collect::<Vec<_>>(),
    }))
    .map_err(|error| error.to_string())
}

pub(crate) fn run(input: &Value) -> Result<String, String> {
    let request = parse_request(input.clone())?;
    run_with_io(
        &request,
        &mut std::io::stdout().lock(),
        &mut std::io::stdin().lock(),
    )
}

pub(crate) fn run_with_io(
    request: &QuestionPromptRequest,
    out: &mut impl Write,
    reader: &mut impl BufRead,
) -> Result<String, String> {
    let mut collect = || -> std::io::Result<Vec<QuestionPromptAnswer>> {
        if let Some(title) = &request.title {
            writeln!(out, "\n[Question Set] {title}")?;
        }
        if let Some(description) = &request.description {
            writeln!(out, "{}", description.as_str())?;
        }
        let mut answers = Vec::with_capacity(request.fields.len());
        for (index, field) in request.fields.iter().enumerate() {
            writeln!(out, "\n[Question] {}. {}", index + 1, field.prompt)?;
            for (index, option) in field.options.iter().enumerate() {
                writeln!(out, "  {}. {}", index + 1, option.label)?;
            }
            if field.allow_custom_input {
                write!(out, "Your answer (choose an option or type your own): ")?;
            } else {
                write!(out, "Enter choice (1-{}): ", field.options.len())?;
            }
            out.flush()?;
            let mut answer = String::new();
            reader.read_line(&mut answer)?;
            let answer = answer.trim();
            let matched = answer
                .parse::<usize>()
                .ok()
                .and_then(|index| index.checked_sub(1))
                .and_then(|index| field.options.get(index))
                .or_else(|| {
                    field
                        .options
                        .iter()
                        .find(|option| option.label == answer || option.value == answer)
                });
            if matched.is_none() && !field.allow_custom_input {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "answer must match a listed option",
                ));
            }
            answers.push(QuestionPromptAnswer {
                id: field.id.clone(),
                value: matched.map_or_else(|| answer.to_owned(), |option| option.value.clone()),
                label: matched
                    .map(|option| option.label.clone())
                    .or_else(|| (!answer.is_empty()).then(|| answer.to_owned())),
            });
        }
        Ok(answers)
    };
    let answers = collect().map_err(|error| error.to_string())?;
    serialize_result(request, &answers)
}
