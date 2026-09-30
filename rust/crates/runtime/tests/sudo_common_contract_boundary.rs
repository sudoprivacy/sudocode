use std::{env, fs, path::PathBuf, process::Command};

use serde_json::Value;

const EXPECTED_SUDOSTACK_SHA: &str = "6a51190a0e08f673912967716629a2fdb424b85a";

#[test]
fn common_v1_valid_fixtures_are_accepted() {
    assert_sudostack_sha();
    for path in fixture_files("valid") {
        parse_common(&read_json(&path)).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
    }
}

#[test]
fn common_v1_invalid_fixtures_are_rejected() {
    assert_sudostack_sha();
    for path in fixture_files("invalid") {
        assert!(
            parse_common(&read_json(&path)).is_err(),
            "{} should be rejected",
            path.display()
        );
    }
}

#[test]
fn common_v1_roundtrip_fixtures_keep_unknown_optional_fields() {
    assert_sudostack_sha();
    for path in fixture_files("roundtrip") {
        let value = read_json(&path);
        let parsed = parse_common(&value).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
        assert_eq!(parsed, value, "{} changed during roundtrip", path.display());
    }
}

fn parse_common(value: &Value) -> Result<Value, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "expected object".to_string())?;
    let api_version = object
        .get("api_version")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing api_version".to_string())?;
    if api_version != "common.sudo.dev/v1" {
        return Err("unsupported api_version".to_string());
    }
    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing kind".to_string())?;
    reject_secret_like_keys(value)?;
    match kind {
        "ResourceRef" => validate_resource_ref(object)?,
        "ErrorInfo" => validate_error_info(object)?,
        other => return Err(format!("unsupported kind {other}")),
    }
    Ok(value.clone())
}

fn validate_resource_ref(object: &serde_json::Map<String, Value>) -> Result<(), String> {
    let zone_id = required_str(object, "zone_id")?;
    if !is_zone_id(zone_id) {
        return Err("invalid zone_id".to_string());
    }
    let path = required_str(object, "path")?;
    if !path.starts_with('/') {
        return Err("path must start with /".to_string());
    }
    if let Some(version) = object.get("version") {
        if version.as_str().is_none_or(str::is_empty) {
            return Err("version must be a non-empty string".to_string());
        }
    }
    if let Some(size) = object.get("size_bytes") {
        if !size.as_u64().is_some() {
            return Err("size_bytes must be a non-negative integer".to_string());
        }
    }
    if let Some(digest) = object.get("digest").and_then(Value::as_str) {
        if !is_digest(digest) {
            return Err("digest must carry an algorithm prefix".to_string());
        }
    }
    if let Some(media_type) = object.get("media_type") {
        if media_type
            .as_str()
            .is_none_or(|value| !is_media_type(value))
        {
            return Err("media_type must be type/subtype".to_string());
        }
    }
    Ok(())
}

fn validate_error_info(object: &serde_json::Map<String, Value>) -> Result<(), String> {
    let code = required_str(object, "code")?;
    if !code.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        || !code
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
        return Err("code must be upper snake case".to_string());
    }
    if required_str(object, "message")?.is_empty() {
        return Err("message must not be empty".to_string());
    }
    if !object.get("retryable").is_some_and(Value::is_boolean) {
        return Err("retryable must be boolean".to_string());
    }
    if let Some(cause_ref) = object.get("cause_ref") {
        parse_common(cause_ref)?;
    }
    Ok(())
}

fn required_str<'a>(
    object: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Result<&'a str, String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing {key}"))
}

fn is_zone_id(value: &str) -> bool {
    let len = value.chars().count();
    (3..=63).contains(&len)
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn is_digest(value: &str) -> bool {
    let Some((algorithm, body)) = value.split_once(':') else {
        return false;
    };
    !algorithm.is_empty()
        && algorithm
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '+' | '.' | '-'))
        && algorithm
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && !body.is_empty()
        && body.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '+' | '/' | '=' | '-')
        })
}

fn is_media_type(value: &str) -> bool {
    let Some((top, sub)) = value.split_once('/') else {
        return false;
    };
    !top.is_empty()
        && !sub.is_empty()
        && !top.chars().any(char::is_whitespace)
        && !sub.chars().any(char::is_whitespace)
}

fn reject_secret_like_keys(value: &Value) -> Result<(), String> {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if is_secret_like_key(key) {
                    return Err(format!("forbidden inline secret-like field {key}"));
                }
                reject_secret_like_keys(value)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                reject_secret_like_keys(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn is_secret_like_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "secret",
        "credential",
        "password",
        "private_key",
        "private-key",
        "access_token",
        "access-token",
        "refresh_token",
        "refresh-token",
        "id_token",
        "id-token",
        "api_key",
        "api-key",
    ]
    .iter()
    .any(|needle| key.contains(needle))
}

fn fixture_files(kind: &str) -> Vec<PathBuf> {
    let mut files = Vec::new();
    visit(
        &sudostack_repo()
            .join("fixtures")
            .join(kind)
            .join("common/v1"),
        &mut files,
    );
    files.sort();
    files
}

fn visit(dir: &PathBuf, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|err| panic!("{}: {err}", dir.display())) {
        let path = entry.expect("fixture entry").path();
        if path.is_dir() {
            visit(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "json") {
            out.push(path);
        }
    }
}

fn read_json(path: &PathBuf) -> Value {
    serde_json::from_str(&fs::read_to_string(path).expect("fixture should be readable"))
        .expect("fixture should be json")
}

fn sudostack_repo() -> PathBuf {
    if let Ok(path) = env::var("SUDOSTACK_REPO") {
        return PathBuf::from(path);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../..")
        .join("sudostack")
}

fn assert_sudostack_sha() {
    let repo = sudostack_repo();
    let output = Command::new("git")
        .args(["-C"])
        .arg(&repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap_or_else(|err| panic!("{}: failed to read sudostack HEAD: {err}", repo.display()));
    assert!(
        output.status.success(),
        "{}: failed to read sudostack HEAD: {}",
        repo.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    let actual = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_eq!(
        actual, EXPECTED_SUDOSTACK_SHA,
        "SUDOSTACK_REPO must point at the audited sudostack commit"
    );
}
