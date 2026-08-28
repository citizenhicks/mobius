use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

const RESOURCE_DIRECTORIES: &[&str] =
    &["src/backend/model", "src/backend/sandbox", "src/middleware"];

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("out dir"));

    for directory in RESOURCE_DIRECTORIES {
        let directory = manifest_dir.join(directory);
        println!("cargo:rerun-if-changed={}", directory.display());
        generate_resources(&directory, &manifest_dir, &out_dir);
    }
}

fn generate_resources(directory: &Path, manifest_dir: &Path, out_dir: &Path) {
    let mut paths = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
        .map(|entry| entry.unwrap_or_else(|error| panic!("read resource entry: {error}")))
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "toml")
        })
        .collect::<Vec<_>>();
    paths.sort();

    for path in paths {
        println!("cargo:rerun-if-changed={}", path.display());
        let relative = path
            .strip_prefix(manifest_dir)
            .unwrap_or_else(|_| panic!("resource is outside the package: {}", path.display()));
        let module_name = relative
            .with_extension("")
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("_");
        let provider_manifest = relative.parent() == Some(Path::new("src/backend/model"))
            && relative
                .file_name()
                .is_some_and(|name| name != "provider.toml");
        let output_kind = if provider_manifest {
            "manifest"
        } else {
            "text"
        };
        let output = out_dir.join(format!("{module_name}_{output_kind}.rs"));
        if provider_manifest {
            generate_provider_manifest(&path, &output);
        } else {
            generate_resource_module(&path, &output);
        }
    }
}

fn generate_resource_module(path: &Path, output: &Path) {
    let source = read_resource(path);
    let table = source
        .parse::<toml::Table>()
        .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
    let mut values = BTreeMap::new();
    flatten_values("", &toml::Value::Table(table), &mut values, path);

    let mut generated = String::new();
    for (name, value) in values {
        match value {
            ResourceValue::String(value) => write_string(&mut generated, &name, &value),
            ResourceValue::Integer(value) => {
                writeln!(generated, "pub const {name}: i64 = {value};")
                    .expect("write generated integer");
            }
            ResourceValue::Boolean(value) => {
                writeln!(generated, "pub const {name}: bool = {value};")
                    .expect("write generated boolean");
            }
        }
    }
    write_generated(output, generated);
}

fn read_resource(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn write_generated(output: &Path, generated: String) {
    fs::write(output, generated)
        .unwrap_or_else(|error| panic!("write {}: {error}", output.display()));
}

fn write_string(generated: &mut String, name: &str, value: &str) {
    writeln!(generated, "pub const {name}: &str = {value:?};").expect("write generated text");
}

enum ResourceValue {
    String(String),
    Integer(i64),
    Boolean(bool),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderResource {
    provider: ProviderText,
    auth: Option<AuthText>,
    #[serde(default)]
    reasoning: Vec<ReasoningResource>,
    #[serde(default)]
    models: Vec<ModelResource>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderText {
    label: String,
    description: String,
    default_model: Option<String>,
    tool_discovery: ToolDiscoveryResource,
    custom_endpoint_tool_discovery: Option<ToolDiscoveryResource>,
    #[serde(default)]
    web_search: Vec<SearchMode>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthText {
    label: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReasoningResource {
    id: String,
    label: String,
    description: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelResource {
    id: String,
    label: String,
    description: String,
    context_window: i64,
    #[serde(default)]
    reasoning: Vec<String>,
    default_reasoning: Option<String>,
    tool_discovery: Option<ToolDiscoveryResource>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ToolDiscoveryResource {
    Native,
    Rebuild,
}

impl ToolDiscoveryResource {
    const fn rust_variant(self) -> &'static str {
        match self {
            Self::Native => "Native",
            Self::Rebuild => "Rebuild",
        }
    }
}

#[derive(Clone, Copy, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
enum SearchMode {
    Off,
    Cached,
    Live,
}

impl SearchMode {
    const fn rust_variant(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Cached => "Cached",
            Self::Live => "Live",
        }
    }
}

fn generate_provider_manifest(path: &Path, output: &Path) {
    let source = read_resource(path);
    let resource: ProviderResource = toml::from_str(&source)
        .unwrap_or_else(|error| panic!("parse provider manifest {}: {error}", path.display()));
    validate_provider_manifest(&resource, path);

    let reasoning = resource
        .reasoning
        .iter()
        .map(|preset| (preset.id.as_str(), preset))
        .collect::<BTreeMap<_, _>>();
    let mut generated = String::new();
    write_string(&mut generated, "PROVIDER_LABEL", &resource.provider.label);
    write_string(
        &mut generated,
        "PROVIDER_DESCRIPTION",
        &resource.provider.description,
    );
    writeln!(
        generated,
        "pub const TOOL_DISCOVERY: crate::protocol::ToolDiscoveryMode = crate::protocol::ToolDiscoveryMode::{};",
        resource.provider.tool_discovery.rust_variant()
    )
    .expect("write provider tool discovery");
    writeln!(
        generated,
        "pub const CUSTOM_ENDPOINT_TOOL_DISCOVERY: Option<crate::protocol::ToolDiscoveryMode> = {};",
        tool_discovery_option_literal(resource.provider.custom_endpoint_tool_discovery)
    )
    .expect("write custom endpoint tool discovery");
    if let Some(auth) = &resource.auth {
        write_string(&mut generated, "AUTH_LABEL", &auth.label);
    }

    let has_catalog = resource.provider.default_model.is_some()
        || !resource.provider.web_search.is_empty()
        || !resource.reasoning.is_empty()
        || !resource.models.is_empty();
    if has_catalog {
        writeln!(
            generated,
            "pub const DEFAULT_MODEL: Option<&str> = {};",
            option_literal(resource.provider.default_model.as_deref())
        )
        .expect("write provider default model");

        generated
            .push_str("pub const MODELS: &[crate::backend::model::provider::ModelPreset] = &[\n");
        for model in &resource.models {
            generated.push_str("    crate::backend::model::provider::ModelPreset {\n");
            writeln!(generated, "        id: {:?},", model.id).expect("write model id");
            writeln!(generated, "        label: {:?},", model.label).expect("write model label");
            writeln!(generated, "        description: {:?},", model.description)
                .expect("write model description");
            writeln!(
                generated,
                "        context_window: {},",
                model.context_window
            )
            .expect("write model context window");
            generated.push_str("        reasoning: &[\n");
            for id in &model.reasoning {
                write_reasoning_preset(
                    &mut generated,
                    reasoning
                        .get(id.as_str())
                        .expect("validated reasoning reference"),
                    12,
                );
            }
            generated.push_str("        ],\n");
            writeln!(
                generated,
                "        default_reasoning: {},",
                option_literal(model.default_reasoning.as_deref())
            )
            .expect("write default reasoning");
            writeln!(
                generated,
                "        tool_discovery: crate::protocol::ToolDiscoveryMode::{},",
                model
                    .tool_discovery
                    .unwrap_or(resource.provider.tool_discovery)
                    .rust_variant()
            )
            .expect("write model tool discovery");
            generated.push_str("    },\n");
        }
        generated.push_str("];\n");

        generated.push_str(
            "pub const SEARCH: &[crate::backend::model::provider::HostedWebSearch] = &[\n",
        );
        for mode in &resource.provider.web_search {
            writeln!(
                generated,
                "    crate::backend::model::provider::HostedWebSearch::{},",
                mode.rust_variant()
            )
            .expect("write search mode");
        }
        generated.push_str("];\n");
    }
    write_generated(output, generated);
}

fn write_reasoning_preset(generated: &mut String, preset: &ReasoningResource, indent: usize) {
    let padding = " ".repeat(indent);
    writeln!(
        generated,
        "{padding}crate::backend::model::provider::ReasoningPreset {{"
    )
    .expect("write reasoning preset");
    writeln!(generated, "{padding}    id: {:?},", preset.id).expect("write reasoning id");
    writeln!(generated, "{padding}    label: {:?},", preset.label).expect("write reasoning label");
    writeln!(
        generated,
        "{padding}    description: {:?},",
        preset.description
    )
    .expect("write reasoning description");
    writeln!(generated, "{padding}}},").expect("finish reasoning preset");
}

fn option_literal(value: Option<&str>) -> String {
    value.map_or_else(|| "None".into(), |value| format!("Some({value:?})"))
}

fn tool_discovery_option_literal(value: Option<ToolDiscoveryResource>) -> String {
    value.map_or_else(
        || "None".into(),
        |value| {
            format!(
                "Some(crate::protocol::ToolDiscoveryMode::{})",
                value.rust_variant()
            )
        },
    )
}

fn validate_provider_manifest(resource: &ProviderResource, path: &Path) {
    require_text(&resource.provider.label, "provider.label", path);
    require_text(&resource.provider.description, "provider.description", path);
    if let Some(auth) = &resource.auth {
        require_text(&auth.label, "auth.label", path);
    }

    let mut reasoning_ids = BTreeSet::new();
    for preset in &resource.reasoning {
        require_text(&preset.id, "reasoning.id", path);
        require_text(&preset.label, "reasoning.label", path);
        require_text(&preset.description, "reasoning.description", path);
        if !reasoning_ids.insert(preset.id.as_str()) {
            panic!(
                "{} contains duplicate reasoning id `{}`",
                path.display(),
                preset.id
            );
        }
    }

    let mut model_ids = BTreeSet::new();
    let mut used_reasoning = BTreeSet::new();
    for model in &resource.models {
        require_text(&model.id, "models.id", path);
        require_text(&model.label, "models.label", path);
        require_text(&model.description, "models.description", path);
        if !model_ids.insert(model.id.as_str()) {
            panic!(
                "{} contains duplicate model id `{}`",
                path.display(),
                model.id
            );
        }
        if model.context_window <= 0 {
            panic!(
                "{} model `{}` must have a positive context_window",
                path.display(),
                model.id
            );
        }
        let mut model_reasoning = BTreeSet::new();
        for id in &model.reasoning {
            if !reasoning_ids.contains(id.as_str()) {
                panic!(
                    "{} model `{}` references unknown reasoning id `{id}`",
                    path.display(),
                    model.id
                );
            }
            if !model_reasoning.insert(id.as_str()) {
                panic!(
                    "{} model `{}` repeats reasoning id `{id}`",
                    path.display(),
                    model.id
                );
            }
            used_reasoning.insert(id.as_str());
        }
        if let Some(default) = &model.default_reasoning
            && !model_reasoning.contains(default.as_str())
        {
            panic!(
                "{} model `{}` has unavailable default reasoning `{default}`",
                path.display(),
                model.id
            );
        }
    }

    for id in &reasoning_ids {
        if !used_reasoning.contains(id) {
            panic!(
                "{} declares reasoning id `{id}` without assigning it to a model",
                path.display()
            );
        }
    }

    match (&resource.provider.default_model, resource.models.is_empty()) {
        (Some(default), false) if model_ids.contains(default.as_str()) => {
            if resource.models.first().map(|model| model.id.as_str()) != Some(default) {
                panic!(
                    "{} must list provider.default_model `{default}` first",
                    path.display()
                );
            }
        }
        (Some(default), false) => panic!(
            "{} references unknown provider.default_model `{default}`",
            path.display()
        ),
        (None, false) => panic!(
            "{} must set provider.default_model when models are advertised",
            path.display()
        ),
        (Some(_), true) => panic!(
            "{} cannot set provider.default_model without models",
            path.display()
        ),
        (None, true) => {}
    }

    let mut search = BTreeSet::new();
    for mode in &resource.provider.web_search {
        if !search.insert(*mode) {
            panic!("{} repeats a provider.web_search mode", path.display());
        }
    }
    if !resource.provider.web_search.is_empty()
        && resource.provider.web_search.first() != Some(&SearchMode::Off)
    {
        panic!(
            "{} provider.web_search must start with `off`",
            path.display()
        );
    }
}

fn require_text(value: &str, key: &str, path: &Path) {
    if value.trim().is_empty() {
        panic!("{} has empty `{key}`", path.display());
    }
}

fn flatten_values(
    prefix: &str,
    value: &toml::Value,
    values: &mut BTreeMap<String, ResourceValue>,
    resource_path: &Path,
) {
    match value {
        toml::Value::Table(table) => {
            for (key, value) in table {
                let key_path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten_values(&key_path, value, values, resource_path);
            }
        }
        toml::Value::String(value) => {
            let name = constant_name(prefix);
            insert_value(
                name,
                ResourceValue::String(value.clone()),
                values,
                resource_path,
            );
        }
        toml::Value::Integer(value) => {
            let name = constant_name(prefix);
            insert_value(name, ResourceValue::Integer(*value), values, resource_path);
        }
        toml::Value::Boolean(value) => {
            let name = constant_name(prefix);
            insert_value(name, ResourceValue::Boolean(*value), values, resource_path);
        }
        other => panic!(
            "{} contains unsupported resource value at `{prefix}`: {other:?}",
            resource_path.display()
        ),
    }
}

fn insert_value(
    name: String,
    value: ResourceValue,
    values: &mut BTreeMap<String, ResourceValue>,
    resource_path: &Path,
) {
    if values.insert(name.clone(), value).is_some() {
        panic!(
            "duplicate generated resource constant {name} in {}",
            resource_path.display()
        );
    }
}

fn constant_name(path: &str) -> String {
    let mut name = String::new();
    for character in path.chars() {
        if character.is_ascii_alphanumeric() {
            name.push(character.to_ascii_uppercase());
        } else if !name.ends_with('_') {
            name.push('_');
        }
    }
    while name.ends_with('_') {
        name.pop();
    }
    if name.is_empty() {
        panic!("empty text resource key");
    }
    if name.as_bytes()[0].is_ascii_digit() {
        name.insert(0, '_');
    }
    name
}
