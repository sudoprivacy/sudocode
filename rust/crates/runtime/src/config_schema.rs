//! SSOT field-schema registry for `settings.json` and `sudocode.json`.
//!
//! Every known configuration key is defined exactly once here.
//! `config_validate` imports these tables for validation;
//! the CLI config UI imports them for interactive editing.

/// Expected JSON type for a config field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    String,
    Bool,
    Object,
    StringArray,
    Number,
}

impl FieldType {
    /// Human-readable label for error messages.
    pub fn label(self) -> &'static str {
        match self {
            Self::String => "a string",
            Self::Bool => "a boolean",
            Self::Object => "an object",
            Self::StringArray => "an array of strings",
            Self::Number => "a number",
        }
    }

    /// Check whether a JSON value conforms to this type.
    pub fn matches(self, value: &crate::json::JsonValue) -> bool {
        match self {
            Self::String => value.as_str().is_some(),
            Self::Bool => value.as_bool().is_some(),
            Self::Object => value.as_object().is_some(),
            Self::StringArray => value
                .as_array()
                .is_some_and(|arr| arr.iter().all(|v| v.as_str().is_some())),
            Self::Number => value.as_i64().is_some(),
        }
    }
}

/// Schema for a single configuration field.
///
/// Drives both validation (`config_validate`) and interactive editing (CLI config UI).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldSchema {
    /// JSON key name (e.g. `"model"`, `"enabled"`).
    pub key: &'static str,
    /// Expected JSON value type.
    pub field_type: FieldType,
    /// Static enum options (e.g. permission modes). `None` = free-form.
    pub options: Option<&'static [&'static str]>,
    /// When `true`, options are fetched at runtime (e.g. model list from SSOT).
    pub is_dynamic_options: bool,
    /// Human-readable description shown in the config UI.
    pub description: &'static str,
    /// Whether this field is deprecated. When `true`, `deprecated_replacement`
    /// should name the replacement key path.
    pub is_deprecated: bool,
    /// Replacement key for deprecated fields (e.g. `"permissions.defaultMode"`).
    pub deprecated_replacement: &'static str,
    /// Child fields for Object types — enables tree navigation in the UI.
    pub children: Option<&'static [FieldSchema]>,
}

impl FieldSchema {
    /// Convenience constructor for a simple leaf field.
    const fn leaf(key: &'static str, field_type: FieldType, description: &'static str) -> Self {
        Self {
            key,
            field_type,
            options: None,
            is_dynamic_options: false,
            description,
            is_deprecated: false,
            deprecated_replacement: "",
            children: None,
        }
    }

    /// Convenience constructor for an enum field (string with known options).
    const fn enumerated(
        key: &'static str,
        options: &'static [&'static str],
        description: &'static str,
    ) -> Self {
        Self {
            key,
            field_type: FieldType::String,
            options: Some(options),
            is_dynamic_options: false,
            description,
            is_deprecated: false,
            deprecated_replacement: "",
            children: None,
        }
    }

    /// Convenience constructor for an object with known children.
    const fn object(
        key: &'static str,
        children: &'static [FieldSchema],
        description: &'static str,
    ) -> Self {
        Self {
            key,
            field_type: FieldType::Object,
            options: None,
            is_dynamic_options: false,
            description,
            is_deprecated: false,
            deprecated_replacement: "",
            children: Some(children),
        }
    }

    /// Convenience constructor for a deprecated field.
    const fn deprecated(
        key: &'static str,
        field_type: FieldType,
        replacement: &'static str,
    ) -> Self {
        Self {
            key,
            field_type,
            options: None,
            is_dynamic_options: false,
            description: "deprecated",
            is_deprecated: true,
            deprecated_replacement: replacement,
            children: None,
        }
    }
}

// ── settings.json child schemas ──────────────────────────────────────

const HOOKS_CHILDREN: &[FieldSchema] = &[
    FieldSchema::leaf(
        "PreToolUse",
        FieldType::StringArray,
        "Commands run before a tool call",
    ),
    FieldSchema::leaf(
        "PostToolUse",
        FieldType::StringArray,
        "Commands run after a tool call",
    ),
    FieldSchema::leaf(
        "PostToolUseFailure",
        FieldType::StringArray,
        "Commands run after a failed tool call",
    ),
];

const PERMISSION_MODE_OPTIONS: &[&str] =
    &["plan", "read-only", "workspace-write", "danger-full-access"];

const PERMISSIONS_CHILDREN: &[FieldSchema] = &[
    FieldSchema::enumerated(
        "defaultMode",
        PERMISSION_MODE_OPTIONS,
        "Default permission mode",
    ),
    FieldSchema::leaf("allow", FieldType::StringArray, "Allowed tool patterns"),
    FieldSchema::leaf("deny", FieldType::StringArray, "Denied tool patterns"),
    FieldSchema::leaf("ask", FieldType::StringArray, "Tools that always prompt"),
];

const PLUGINS_CHILDREN: &[FieldSchema] = &[
    FieldSchema::leaf("enabled", FieldType::Object, "Plugin enable/disable map"),
    FieldSchema::leaf(
        "externalDirectories",
        FieldType::StringArray,
        "External plugin directories",
    ),
    FieldSchema::leaf(
        "installRoot",
        FieldType::String,
        "Plugin install root directory",
    ),
    FieldSchema::leaf("registryPath", FieldType::String, "Plugin registry path"),
    FieldSchema::leaf("bundledRoot", FieldType::String, "Bundled plugins root"),
    FieldSchema::leaf(
        "maxOutputTokens",
        FieldType::Number,
        "Max output tokens for plugins",
    ),
];

const FILESYSTEM_MODE_OPTIONS: &[&str] = &["readonly", "readwrite"];

const SANDBOX_CHILDREN: &[FieldSchema] = &[
    FieldSchema::leaf("enabled", FieldType::Bool, "Enable sandbox isolation"),
    FieldSchema::leaf(
        "namespaceRestrictions",
        FieldType::Bool,
        "Enable namespace restrictions",
    ),
    FieldSchema::leaf(
        "networkIsolation",
        FieldType::Bool,
        "Isolate network access",
    ),
    FieldSchema::enumerated(
        "filesystemMode",
        FILESYSTEM_MODE_OPTIONS,
        "Filesystem isolation mode",
    ),
    FieldSchema::leaf(
        "allowedMounts",
        FieldType::StringArray,
        "Allowed filesystem mounts",
    ),
];

const OAUTH_CHILDREN: &[FieldSchema] = &[
    FieldSchema::leaf("clientId", FieldType::String, "OAuth client ID"),
    FieldSchema::leaf("authorizeUrl", FieldType::String, "OAuth authorization URL"),
    FieldSchema::leaf("tokenUrl", FieldType::String, "OAuth token endpoint URL"),
    FieldSchema::leaf("callbackPort", FieldType::Number, "OAuth callback port"),
    FieldSchema::leaf(
        "manualRedirectUrl",
        FieldType::String,
        "Manual redirect URL for headless auth",
    ),
    FieldSchema::leaf("scopes", FieldType::StringArray, "OAuth scopes"),
];

// ── settings.json top-level schema ──────────────────────────────────

/// All known fields in `settings.json`.
pub const SETTINGS_SCHEMA: &[FieldSchema] = &[
    FieldSchema::leaf("$schema", FieldType::String, "JSON schema URL"),
    {
        let mut f = FieldSchema::leaf("model", FieldType::String, "Active model identifier");
        f.is_dynamic_options = true;
        f
    },
    FieldSchema::object("hooks", HOOKS_CHILDREN, "Lifecycle hook commands"),
    FieldSchema::object("permissions", PERMISSIONS_CHILDREN, "Permission rules"),
    FieldSchema::deprecated(
        "permissionMode",
        FieldType::String,
        "permissions.defaultMode",
    ),
    FieldSchema::object("mcpServers", &[], "MCP server configurations"),
    FieldSchema::object("oauth", OAUTH_CHILDREN, "OAuth configuration"),
    FieldSchema::deprecated("enabledPlugins", FieldType::Object, "plugins.enabled"),
    FieldSchema::object("plugins", PLUGINS_CHILDREN, "Plugin configuration"),
    FieldSchema::object("sandbox", SANDBOX_CHILDREN, "Sandbox isolation settings"),
    FieldSchema::leaf("env", FieldType::Object, "Environment variable overrides"),
    FieldSchema::leaf("aliases", FieldType::Object, "Command aliases"),
    FieldSchema::leaf(
        "thinking",
        FieldType::Bool,
        "Enable extended thinking for supported models (default: true)",
    ),
    FieldSchema::leaf(
        "providerFallbacks",
        FieldType::Object,
        "Provider fallback chain",
    ),
    FieldSchema::leaf(
        "trustedRoots",
        FieldType::StringArray,
        "Trusted project root paths",
    ),
];

// ── sudocode.json schema ────────────────────────────────────────────

/// All known fields in `sudocode.json`.
pub const SUDOCODE_SCHEMA: &[FieldSchema] = &[
    FieldSchema::leaf(
        "auth_modes",
        FieldType::Object,
        "Authentication mode configurations",
    ),
    FieldSchema::leaf("models", FieldType::Object, "Model alias definitions"),
    FieldSchema::object(
        "web_search",
        &[
            FieldSchema::leaf("provider", FieldType::String, "Search provider name"),
            FieldSchema::leaf("apiUrl", FieldType::String, "Search API base URL"),
            FieldSchema::leaf("apiKey", FieldType::String, "Search API key"),
        ],
        "Web search provider configuration",
    ),
];

// ── lookup helpers ──────────────────────────────────────────────────

/// Find a field schema by key in a flat schema slice.
pub fn find_field<'a>(schema: &'a [FieldSchema], key: &str) -> Option<&'a FieldSchema> {
    schema.iter().find(|f| f.key == key)
}

/// Collect all non-deprecated key names from a schema slice.
pub fn known_keys(schema: &[FieldSchema]) -> Vec<&'static str> {
    schema
        .iter()
        .filter(|f| !f.is_deprecated)
        .map(|f| f.key)
        .collect()
}

/// Collect deprecated field entries from a schema slice.
pub fn deprecated_fields(schema: &[FieldSchema]) -> Vec<&FieldSchema> {
    schema.iter().filter(|f| f.is_deprecated).collect()
}

/// Determine the UI input kind for a field based on its schema and current value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigInputKind {
    /// Toggle immediately, no UI needed.
    BoolToggle,
    /// Show DialPad with the given static options.
    Enum(Vec<String>),
    /// Show FuzzySelect — options fetched dynamically at runtime.
    DynamicList,
    /// Show TextInput.
    Text,
    /// Show TextInput for number entry.
    NumberInput,
    /// Open in $EDITOR (complex object/array).
    Editor,
}

/// Resolve the appropriate input kind for a field schema.
pub fn resolve_input_kind(schema: &FieldSchema) -> ConfigInputKind {
    if schema.field_type == FieldType::Bool {
        return ConfigInputKind::BoolToggle;
    }
    if schema.is_dynamic_options {
        return ConfigInputKind::DynamicList;
    }
    if let Some(opts) = schema.options {
        return ConfigInputKind::Enum(opts.iter().map(|s| (*s).to_string()).collect());
    }
    match schema.field_type {
        FieldType::String => ConfigInputKind::Text,
        FieldType::Number => ConfigInputKind::NumberInput,
        FieldType::Object | FieldType::StringArray => {
            if schema.children.is_some() {
                // Has structured children — navigate into them.
                ConfigInputKind::Editor
            } else {
                ConfigInputKind::Editor
            }
        }
        // Covered above but exhaustiveness requires it.
        FieldType::Bool => ConfigInputKind::BoolToggle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_schema_covers_all_validate_fields() {
        // All field names from config_validate's TOP_LEVEL_FIELDS and DEPRECATED_FIELDS
        // must appear in SETTINGS_SCHEMA.
        let expected_keys = [
            "$schema",
            "model",
            "hooks",
            "permissions",
            "permissionMode",
            "mcpServers",
            "oauth",
            "enabledPlugins",
            "plugins",
            "sandbox",
            "env",
            "aliases",
            "thinking",
            "providerFallbacks",
            "trustedRoots",
        ];
        let schema_keys: Vec<&str> = SETTINGS_SCHEMA.iter().map(|f| f.key).collect();
        for key in &expected_keys {
            assert!(
                schema_keys.contains(key),
                "SETTINGS_SCHEMA missing key: {key}"
            );
        }
    }

    #[test]
    fn settings_schema_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for field in SETTINGS_SCHEMA {
            assert!(
                seen.insert(field.key),
                "duplicate key in SETTINGS_SCHEMA: {}",
                field.key
            );
        }
    }

    #[test]
    fn hooks_children_match_validate() {
        let expected = ["PreToolUse", "PostToolUse", "PostToolUseFailure"];
        let keys: Vec<&str> = HOOKS_CHILDREN.iter().map(|f| f.key).collect();
        for key in &expected {
            assert!(keys.contains(key), "HOOKS_CHILDREN missing: {key}");
        }
    }

    #[test]
    fn permissions_children_match_validate() {
        let expected = ["defaultMode", "allow", "deny", "ask"];
        let keys: Vec<&str> = PERMISSIONS_CHILDREN.iter().map(|f| f.key).collect();
        for key in &expected {
            assert!(keys.contains(key), "PERMISSIONS_CHILDREN missing: {key}");
        }
    }

    #[test]
    fn plugins_children_match_validate() {
        let expected = [
            "enabled",
            "externalDirectories",
            "installRoot",
            "registryPath",
            "bundledRoot",
            "maxOutputTokens",
        ];
        let keys: Vec<&str> = PLUGINS_CHILDREN.iter().map(|f| f.key).collect();
        for key in &expected {
            assert!(keys.contains(key), "PLUGINS_CHILDREN missing: {key}");
        }
    }

    #[test]
    fn sandbox_children_match_validate() {
        let expected = [
            "enabled",
            "namespaceRestrictions",
            "networkIsolation",
            "filesystemMode",
            "allowedMounts",
        ];
        let keys: Vec<&str> = SANDBOX_CHILDREN.iter().map(|f| f.key).collect();
        for key in &expected {
            assert!(keys.contains(key), "SANDBOX_CHILDREN missing: {key}");
        }
    }

    #[test]
    fn oauth_children_match_validate() {
        let expected = [
            "clientId",
            "authorizeUrl",
            "tokenUrl",
            "callbackPort",
            "manualRedirectUrl",
            "scopes",
        ];
        let keys: Vec<&str> = OAUTH_CHILDREN.iter().map(|f| f.key).collect();
        for key in &expected {
            assert!(keys.contains(key), "OAUTH_CHILDREN missing: {key}");
        }
    }

    #[test]
    fn deprecated_fields_have_replacement() {
        for field in SETTINGS_SCHEMA {
            if field.is_deprecated {
                assert!(
                    !field.deprecated_replacement.is_empty(),
                    "deprecated field {} has no replacement",
                    field.key
                );
            }
        }
    }

    #[test]
    fn sudocode_schema_covers_known_keys() {
        let expected = ["auth_modes", "models", "web_search"];
        let keys: Vec<&str> = SUDOCODE_SCHEMA.iter().map(|f| f.key).collect();
        for key in &expected {
            assert!(keys.contains(key), "SUDOCODE_SCHEMA missing key: {key}");
        }
    }

    #[test]
    fn resolve_input_kind_bool() {
        let schema = FieldSchema::leaf("enabled", FieldType::Bool, "test");
        assert_eq!(resolve_input_kind(&schema), ConfigInputKind::BoolToggle);
    }

    #[test]
    fn resolve_input_kind_enum() {
        let schema = FieldSchema::enumerated("mode", &["a", "b"], "test");
        assert!(
            matches!(resolve_input_kind(&schema), ConfigInputKind::Enum(opts) if opts == vec!["a", "b"])
        );
    }

    #[test]
    fn resolve_input_kind_dynamic() {
        let mut schema = FieldSchema::leaf("model", FieldType::String, "test");
        schema.is_dynamic_options = true;
        assert_eq!(resolve_input_kind(&schema), ConfigInputKind::DynamicList);
    }

    #[test]
    fn resolve_input_kind_text() {
        let schema = FieldSchema::leaf("name", FieldType::String, "test");
        assert_eq!(resolve_input_kind(&schema), ConfigInputKind::Text);
    }

    #[test]
    fn resolve_input_kind_number() {
        let schema = FieldSchema::leaf("port", FieldType::Number, "test");
        assert_eq!(resolve_input_kind(&schema), ConfigInputKind::NumberInput);
    }

    #[test]
    fn resolve_input_kind_object() {
        let schema = FieldSchema::leaf("env", FieldType::Object, "test");
        assert_eq!(resolve_input_kind(&schema), ConfigInputKind::Editor);
    }

    #[test]
    fn find_field_returns_match() {
        assert_eq!(
            find_field(SETTINGS_SCHEMA, "model").map(|f| f.key),
            Some("model")
        );
        assert!(find_field(SETTINGS_SCHEMA, "nonexistent").is_none());
    }

    #[test]
    fn known_keys_excludes_deprecated() {
        let keys = known_keys(SETTINGS_SCHEMA);
        assert!(!keys.contains(&"permissionMode"));
        assert!(!keys.contains(&"enabledPlugins"));
        assert!(keys.contains(&"model"));
    }
}
