//! Progressive skill discovery and activation for agentkit agents.
//!
//! This crate implements the [Agent Skills specification](https://agentskills.io/specification),
//! providing a [`SkillRegistry`] that discovers `SKILL.md` files, parses their
//! frontmatter, and exposes an `activate_skill` tool for on-demand loading.
//!
//! # Progressive disclosure
//!
//! Instead of eagerly loading every skill into the transcript, this crate
//! follows a three-tier strategy:
//!
//! 1. **Catalog** -- skill names and descriptions are listed in the tool
//!    description at session start (~50-100 tokens per skill).
//! 2. **Instructions** -- the full `SKILL.md` body (frontmatter stripped) is
//!    loaded only when the model calls `activate_skill`.
//! 3. **Resources** -- supporting files (scripts, references, assets) are
//!    enumerated in the activation response; the model reads them on demand.
//!
//! # Example
//!
//! ```rust,no_run
//! use agentkit_tool_skills::SkillRegistry;
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! // Discover skills from default locations (.agents/skills/ at project and user level).
//! let registry = SkillRegistry::discover(".").build().await;
//!
//! // Compose with the agent's other tools.
//! let tools = agentkit_tools_core::ToolRegistry::new().merge(registry.tool_registry());
//! # Ok(())
//! # }
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agentkit_core::{MetadataMap, SessionId, ToolOutput, ToolResultPart};
use agentkit_tools_core::{
    Tool, ToolAnnotations, ToolContext, ToolError, ToolName, ToolRegistry, ToolRequest, ToolResult,
    ToolSpec,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;

const DEFAULT_SKILL_FILE: &str = "SKILL.md";
const TOOL_NAME: &str = "activate_skill";

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A parsed skill ready for catalog disclosure and on-demand activation.
#[derive(Clone, Debug)]
pub struct Skill {
    /// Machine-readable name from the frontmatter `name` field.
    pub name: String,
    /// Human-readable description from the frontmatter `description` field.
    pub description: String,
    /// Absolute path to the `SKILL.md` file.
    pub location: PathBuf,
    /// The skill directory (parent of `location`).
    pub base_dir: PathBuf,
    /// The markdown body after the YAML frontmatter (frontmatter stripped).
    pub body: String,
    /// Absolute paths to all resource files in the skill directory (excluding `SKILL.md`).
    pub resources: Vec<PathBuf>,
    /// Optional fields parsed from frontmatter.
    pub frontmatter: SkillFrontmatter,
}

/// Optional fields from the `SKILL.md` YAML frontmatter.
#[derive(Clone, Debug, Default)]
pub struct SkillFrontmatter {
    /// License name or reference to a bundled license file.
    pub license: Option<String>,
    /// Environment requirements (intended product, system packages, etc.).
    pub compatibility: Option<String>,
    /// Arbitrary key-value metadata.
    pub metadata: BTreeMap<String, String>,
    /// Space-delimited list of pre-approved tools. (Experimental)
    pub allowed_tools: Option<String>,
}

/// Errors that can occur during skill discovery.
#[derive(Debug, Error)]
pub enum SkillError {
    /// A filesystem operation failed during scanning.
    #[error("failed to inspect {path}: {error}")]
    InspectFailed {
        path: PathBuf,
        #[source]
        error: std::io::Error,
    },
    /// Reading a discovered `SKILL.md` failed.
    #[error("failed to read {path}: {error}")]
    ReadFailed {
        path: PathBuf,
        #[source]
        error: std::io::Error,
    },
}

/// Trait for filtering skills out of the catalog.
///
/// Implement this for complex filtering logic (e.g. checking a database of
/// disabled skills), or use a closure which has a blanket implementation.
///
/// # Example
///
/// ```rust
/// use agentkit_tool_skills::{Skill, SkillFilter};
///
/// // Closures work as filters automatically:
/// let disabled = vec!["old-skill".to_string()];
/// let filter = move |skill: &Skill| !disabled.contains(&skill.name);
/// ```
pub trait SkillFilter: Send + Sync {
    /// Return `true` to keep the skill, `false` to exclude it.
    fn keep(&self, skill: &Skill) -> bool;
}

impl<F> SkillFilter for F
where
    F: Fn(&Skill) -> bool + Send + Sync,
{
    fn keep(&self, skill: &Skill) -> bool {
        self(skill)
    }
}

/// Registry of discovered skills that provides the `activate_skill` tool.
///
/// The registry discovers `SKILL.md` files from one or more directory roots,
/// parses their frontmatter, and builds an in-memory catalog. When registered
/// as a tool, the model sees a YAML catalog of available skills in the tool
/// description and can call `activate_skill` with a skill name to load its
/// full instructions.
///
/// # Discovery order and name collisions
///
/// Roots are scanned in the order they are provided. When two skills share
/// the same `name`, the first one discovered wins. To implement project-level
/// overrides user-level, pass project paths before user paths.
///
/// # Example
///
/// ```rust,no_run
/// use agentkit_tool_skills::SkillRegistry;
/// use std::path::PathBuf;
///
/// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let home = std::env::var("HOME").unwrap();
/// let registry = SkillRegistry::from_paths(vec![
///     "./.agents/skills".into(),
///     PathBuf::from(home).join(".agents/skills"),
/// ])
/// .with_filter(|skill: &agentkit_tool_skills::Skill| skill.name != "deprecated-skill")
/// .discover_skills()
/// .await;
///
/// // Compose with the agent's other tools; activate_skill rediscovers each turn.
/// let tools = agentkit_tools_core::ToolRegistry::new().merge(registry.tool_registry());
/// # Ok(())
/// # }
/// ```
pub struct SkillRegistry {
    roots: Vec<PathBuf>,
    filters: Vec<Arc<dyn SkillFilter>>,
    skills: BTreeMap<String, Skill>,
    activations: Arc<Mutex<HashMap<SessionId, HashSet<String>>>>,
}

impl SkillRegistry {
    /// Create a registry that will scan the given directory roots.
    ///
    /// Roots are scanned in order; on name collision the first skill wins.
    /// Pass project-level paths first to ensure they override user-level.
    ///
    /// Call [`discover_skills`](Self::discover_skills) to actually scan the
    /// filesystem.
    pub fn from_paths(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            filters: Vec::new(),
            skills: BTreeMap::new(),
            activations: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Create a registry using the spec-default locations:
    ///
    /// 1. `<working_dir>/.agents/skills/` (project-level, scanned first)
    /// 2. `~/.agents/skills/` (user-level)
    pub fn discover(working_dir: impl Into<PathBuf>) -> DiscoverBuilder {
        let working_dir = working_dir.into();
        let mut roots = vec![working_dir.join(".agents/skills")];
        if let Some(home) = home_dir() {
            roots.push(home.join(".agents/skills"));
        }
        DiscoverBuilder {
            roots,
            filters: Vec::new(),
        }
    }

    /// Add a filter that will be evaluated when building the catalog.
    ///
    /// Multiple filters are combined with AND logic: a skill must pass all
    /// filters to appear in the catalog.
    pub fn with_filter<F: SkillFilter + 'static>(mut self, filter: F) -> Self {
        self.filters.push(Arc::new(filter));
        self
    }

    /// Scan all configured roots and populate the skill catalog.
    ///
    /// Skills that fail validation (missing name/description, unparseable
    /// YAML) are silently skipped. Filesystem errors during scanning are
    /// also silently skipped so that a missing directory does not prevent
    /// discovery of other roots.
    pub async fn discover_skills(mut self) -> Self {
        self.skills = discover_filtered_skills(&self.roots, &self.filters);
        self
    }

    /// Re-scan all roots and re-apply filters.
    ///
    /// Call this between turns to pick up newly installed or removed skills.
    /// Activation tracking is preserved across reloads — skills that were
    /// already activated remain marked.
    pub async fn reload(&mut self) {
        self.skills = discover_filtered_skills(&self.roots, &self.filters);
    }

    /// Returns `true` if the catalog contains at least one skill after
    /// filtering.
    pub fn has_skills(&self) -> bool {
        !self.skills.is_empty()
    }

    /// Returns a snapshot of all skills that pass the current filters.
    pub fn skills(&self) -> Vec<&Skill> {
        self.skills.values().collect()
    }

    /// Build a [`ToolRegistry`] containing only the `activate_skill` tool.
    ///
    /// The tool remains registered even when discovery is currently empty so
    /// future turns can surface newly added skills. If no roots are
    /// configured, returns an empty registry.
    pub fn tool_registry(&self) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        if !self.roots.is_empty() {
            registry.register(self.build_tool());
        }
        registry
    }

    /// Reset activation tracking. After this call, all skills will return
    /// their full content on next activation instead of "Skill already read."
    pub fn reset_activations(&self) {
        self.activations.lock().unwrap().clear();
    }

    fn build_tool(&self) -> ActivateSkillTool {
        let spec = ToolSpec {
            name: ToolName::new(TOOL_NAME),
            description: "Load a skill's full instructions into the conversation.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Name of the skill to activate."
                    }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
            output_schema: None,
            annotations: ToolAnnotations {
                read_only_hint: true,
                ..Default::default()
            },
            metadata: MetadataMap::new(),
        };

        ActivateSkillTool {
            static_spec: spec,
            roots: self.roots.clone(),
            filters: self.filters.clone(),
            activations: Arc::clone(&self.activations),
        }
    }
}

/// Builder returned by [`SkillRegistry::discover`] that scans default
/// locations and supports adding filters before discovery.
pub struct DiscoverBuilder {
    roots: Vec<PathBuf>,
    filters: Vec<Arc<dyn SkillFilter>>,
}

impl DiscoverBuilder {
    /// Add an additional directory root to scan.
    pub fn with_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.roots.push(root.into());
        self
    }

    /// Add a filter.
    pub fn with_filter<F: SkillFilter + 'static>(mut self, filter: F) -> Self {
        self.filters.push(Arc::new(filter));
        self
    }

    /// Scan all roots, apply filters, and return the populated registry.
    pub async fn build(self) -> SkillRegistry {
        SkillRegistry::from_paths(self.roots)
            .with_filters(self.filters)
            .discover_skills()
            .await
    }
}

impl SkillRegistry {
    fn with_filters(mut self, filters: Vec<Arc<dyn SkillFilter>>) -> Self {
        self.filters = filters;
        self
    }
}

// ---------------------------------------------------------------------------
// ActivateSkillTool
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ActivateSkillInput {
    name: String,
}

struct ActivateSkillTool {
    static_spec: ToolSpec,
    roots: Vec<PathBuf>,
    filters: Vec<Arc<dyn SkillFilter>>,
    activations: Arc<Mutex<HashMap<SessionId, HashSet<String>>>>,
}

#[async_trait]
impl Tool for ActivateSkillTool {
    fn spec(&self) -> &ToolSpec {
        &self.static_spec
    }

    fn current_spec(&self) -> Option<ToolSpec> {
        let skills = discover_filtered_skills(&self.roots, &self.filters);
        (!skills.is_empty()).then(|| build_activate_skill_spec(&skills))
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let input: ActivateSkillInput = serde_json::from_value(request.input)
            .map_err(|e| ToolError::InvalidInput(format!("invalid input: {e}")))?;

        let skills = discover_filtered_skills(&self.roots, &self.filters);
        let skill = skills
            .get(&input.name)
            .ok_or_else(|| ToolError::InvalidInput(format!("unknown skill: {}", input.name)))?;

        // Deduplicate within a session, not globally across the registry.
        {
            let mut activated = self.activations.lock().unwrap();
            let session_activations = activated.entry(request.session_id.clone()).or_default();
            if session_activations.contains(&input.name) {
                return Ok(ToolResult {
                    result: ToolResultPart {
                        call_id: request.call_id,
                        output: ToolOutput::Text("Skill already read.".into()),
                        is_error: false,
                        metadata: MetadataMap::new(),
                    },
                    duration: None,
                    metadata: MetadataMap::new(),
                });
            }
            session_activations.insert(input.name.clone());
        }

        // Build the response with body + directory + resources.
        let mut response = format!(
            "skill: {name}\ndir: {dir}\n\n{body}",
            name = skill.name,
            dir = skill.base_dir.display(),
            body = skill.body,
        );

        if !skill.resources.is_empty() {
            response.push_str("\n\nresources:\n");
            for resource in &skill.resources {
                response.push_str(&format!("  - {}\n", resource.display()));
            }
        }

        Ok(ToolResult {
            result: ToolResultPart {
                call_id: request.call_id,
                output: ToolOutput::Text(response),
                is_error: false,
                metadata: MetadataMap::new(),
            },
            duration: None,
            metadata: MetadataMap::new(),
        })
    }
}

// ---------------------------------------------------------------------------
// Catalog formatting
// ---------------------------------------------------------------------------

fn build_activate_skill_spec(skills: &BTreeMap<String, Skill>) -> ToolSpec {
    let catalog = build_catalog_yaml(skills);
    let mut name_schema = json!({
        "type": "string",
        "description": "Name of the skill to activate."
    });
    if !skills.is_empty() {
        let enum_values: Vec<Value> = skills.keys().map(|n| Value::String(n.clone())).collect();
        name_schema["enum"] = Value::Array(enum_values);
    }

    let description = if skills.is_empty() {
        "Load a skill's full instructions into the conversation. Available skills are discovered before each turn, but none are available right now.".into()
    } else {
        format!(
            "Load a skill's full instructions into the conversation. \
             Call this when a task matches a skill's description.\n\n\
             Available skills:\n{catalog}"
        )
    };

    ToolSpec {
        name: ToolName::new(TOOL_NAME),
        description,
        input_schema: json!({
            "type": "object",
            "properties": {
                "name": name_schema
            },
            "required": ["name"],
            "additionalProperties": false
        }),
        output_schema: None,
        annotations: ToolAnnotations {
            read_only_hint: true,
            ..Default::default()
        },
        metadata: MetadataMap::new(),
    }
}

fn build_catalog_yaml(skills: &BTreeMap<String, Skill>) -> String {
    let mut lines = Vec::new();
    for skill in skills.values() {
        lines.push(format!("- name: {}", skill.name));
        // Quote description to avoid YAML parsing ambiguity.
        let desc = skill.description.replace('"', "\\\"");
        lines.push(format!("  description: \"{desc}\""));
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Discovery and parsing
// ---------------------------------------------------------------------------

fn discover_filtered_skills(
    roots: &[PathBuf],
    filters: &[Arc<dyn SkillFilter>],
) -> BTreeMap<String, Skill> {
    let mut skills = BTreeMap::new();

    for root in roots {
        if !path_exists(root) {
            continue;
        }
        let Ok(paths) = collect_skill_files(root) else {
            continue;
        };

        for path in paths {
            let Some(skill) = parse_skill(&path) else {
                continue;
            };
            if !filters.iter().all(|filter| filter.keep(&skill)) {
                continue;
            }

            // First passing match wins, with deterministic path ordering inside each root.
            skills.entry(skill.name.clone()).or_insert(skill);
        }
    }

    skills
}

// Agent Skills name requirements come from the public specification:
// https://agentskills.io/specification
fn is_valid_skill_name(name: &str, parent_dir: &str) -> bool {
    if name.is_empty() || name.len() > 64 || name != parent_dir {
        return false;
    }
    if name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        return false;
    }

    name.bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn is_valid_description(description: &str) -> bool {
    !description.is_empty() && description.len() <= 1024
}

/// YAML frontmatter as deserialized from the `SKILL.md` file.
#[derive(Deserialize, Default)]
struct RawFrontmatter {
    name: Option<String>,
    description: Option<String>,
    license: Option<String>,
    compatibility: Option<String>,
    metadata: Option<BTreeMap<String, String>>,
    #[serde(rename = "allowed-tools")]
    allowed_tools: Option<String>,
}

/// Parse a `SKILL.md` file into a [`Skill`], returning `None` if validation
/// fails.
fn parse_skill(path: &Path) -> Option<Skill> {
    let content = fs::read_to_string(path).ok()?;
    let (raw_fm, body) = split_frontmatter(&content)?;
    let fm: RawFrontmatter = parse_yaml_lenient(&raw_fm)?;

    let name = fm.name?.trim().to_string();
    let description = fm.description?.trim().to_string();
    let parent_dir = path.parent()?.file_name()?.to_str()?;

    if !is_valid_skill_name(&name, parent_dir) || !is_valid_description(&description) {
        return None;
    }

    let base_dir = path.parent()?.to_path_buf();
    let abs_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let abs_base = if base_dir.is_absolute() {
        base_dir.clone()
    } else {
        std::env::current_dir().ok()?.join(&base_dir)
    };

    let resources = collect_resources(&abs_base);

    Some(Skill {
        name,
        description,
        location: abs_path,
        base_dir: abs_base,
        body: body.trim().to_string(),
        resources,
        frontmatter: SkillFrontmatter {
            license: fm.license,
            compatibility: fm.compatibility,
            metadata: fm.metadata.unwrap_or_default(),
            allowed_tools: fm.allowed_tools,
        },
    })
}

/// Split content into (frontmatter_yaml, body). Returns `None` if no valid
/// frontmatter delimiters are found.
fn split_frontmatter(content: &str) -> Option<(String, String)> {
    let stripped = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))?;
    if stripped.starts_with("---") {
        return None;
    }

    if let Some((yaml, body)) = stripped.split_once("\n---\n") {
        return Some((yaml.to_string(), body.to_string()));
    }
    if let Some((yaml, body)) = stripped.split_once("\r\n---\r\n") {
        return Some((yaml.to_string(), body.to_string()));
    }
    None
}

/// Attempt lenient YAML parsing. If standard parsing fails, try wrapping
/// problematic values in quotes (handles unquoted colons).
fn parse_yaml_lenient(yaml: &str) -> Option<RawFrontmatter> {
    if let Ok(fm) = serde_saphyr::from_str::<RawFrontmatter>(yaml) {
        return Some(fm);
    }

    // Fallback: wrap each line's value in quotes if it contains an unquoted colon.
    let fixed: String = yaml
        .lines()
        .map(|line| {
            if let Some((key, value)) = line.split_once(':') {
                let value = value.trim();
                if !value.is_empty()
                    && !value.starts_with('"')
                    && !value.starts_with('\'')
                    && value.contains(':')
                {
                    let escaped = value.replace('"', "\\\"");
                    return format!("{key}: \"{escaped}\"");
                }
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");

    serde_saphyr::from_str::<RawFrontmatter>(&fixed).ok()
}

/// Recursively collect all files in `dir` except `SKILL.md`.
fn collect_resources(dir: &Path) -> Vec<PathBuf> {
    let mut resources = Vec::new();
    let mut pending = vec![dir.to_path_buf()];

    while let Some(current) = pending.pop() {
        let entries = match fs::read_dir(&current) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };

            if ft.is_dir() {
                // Skip common non-skill directories.
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name == ".git" || name == "node_modules" {
                    continue;
                }
                pending.push(path);
            } else if ft.is_file() {
                let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if file_name != DEFAULT_SKILL_FILE {
                    resources.push(path);
                }
            }
        }
    }

    resources.sort();
    resources
}

/// Collect all `SKILL.md` paths under `root`.
fn collect_skill_files(root: &Path) -> Result<Vec<PathBuf>, SkillError> {
    let mut pending = vec![root.to_path_buf()];
    let mut skill_paths = Vec::new();

    while let Some(dir_path) = pending.pop() {
        let entries = fs::read_dir(&dir_path).map_err(|error| SkillError::InspectFailed {
            path: dir_path.clone(),
            error,
        })?;

        for entry in entries {
            let entry = entry.map_err(|error| SkillError::InspectFailed {
                path: dir_path.clone(),
                error,
            })?;
            let path = entry.path();
            let ft = entry
                .file_type()
                .map_err(|error| SkillError::InspectFailed {
                    path: path.clone(),
                    error,
                })?;

            if ft.is_dir() {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name == ".git" || name == "node_modules" {
                    continue;
                }
                pending.push(path);
                continue;
            }

            if ft.is_file()
                && path
                    .file_name()
                    .is_some_and(|name| name == DEFAULT_SKILL_FILE)
            {
                skill_paths.push(path);
            }
        }
    }

    skill_paths.sort();
    Ok(skill_paths)
}

fn path_exists(path: &Path) -> bool {
    fs::metadata(path).is_ok()
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
    #[cfg(not(unix))]
    {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_core::ToolCallId;
    use agentkit_tools_core::Tool;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(prefix: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("agentkit-skill-{prefix}-{suffix}"))
    }

    async fn write_skill(dir: &Path, name: &str, description: &str, body: &str) {
        let skill_dir = dir.join(name);
        async_fs::create_dir_all(&skill_dir).await.unwrap();
        let content = format!("---\nname: {name}\ndescription: {description}\n---\n{body}");
        async_fs::write(skill_dir.join("SKILL.md"), content)
            .await
            .unwrap();
    }

    async fn write_skill_with_resources(dir: &Path, name: &str) {
        let skill_dir = dir.join(name);
        let scripts_dir = skill_dir.join("scripts");
        async_fs::create_dir_all(&scripts_dir).await.unwrap();
        let content = format!(
            "---\nname: {name}\ndescription: A skill with resources.\n---\nUse the scripts."
        );
        async_fs::write(skill_dir.join("SKILL.md"), content)
            .await
            .unwrap();
        async_fs::write(scripts_dir.join("run.sh"), "#!/bin/bash\necho hi")
            .await
            .unwrap();
        async_fs::write(skill_dir.join("README.md"), "# Info")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn discovers_skills_from_directory() {
        let root = temp_dir("discover");
        write_skill(&root, "alpha", "Alpha skill.", "Do alpha things.").await;
        write_skill(&root, "beta", "Beta skill.", "Do beta things.").await;

        let reg = SkillRegistry::from_paths(vec![root.clone()])
            .discover_skills()
            .await;

        assert_eq!(reg.skills().len(), 2);
        assert!(reg.has_skills());

        async_fs::remove_dir_all(&root).await.unwrap();
    }

    #[tokio::test]
    async fn project_level_overrides_user_level() {
        let project = temp_dir("project");
        let user = temp_dir("user");

        write_skill(&project, "review", "Project review.", "Project body.").await;
        write_skill(&user, "review", "User review.", "User body.").await;

        let reg = SkillRegistry::from_paths(vec![project.clone(), user.clone()])
            .discover_skills()
            .await;

        let skills = reg.skills();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "Project review.");

        async_fs::remove_dir_all(&project).await.unwrap();
        async_fs::remove_dir_all(&user).await.unwrap();
    }

    #[tokio::test]
    async fn skips_invalid_skills_silently() {
        let root = temp_dir("invalid");

        // Valid skill.
        write_skill(&root, "good", "Good skill.", "Works.").await;

        // Missing description.
        let bad_dir = root.join("bad");
        async_fs::create_dir_all(&bad_dir).await.unwrap();
        async_fs::write(bad_dir.join("SKILL.md"), "---\nname: bad\n---\nNo desc.")
            .await
            .unwrap();

        // Completely broken YAML.
        let broken_dir = root.join("broken");
        async_fs::create_dir_all(&broken_dir).await.unwrap();
        async_fs::write(broken_dir.join("SKILL.md"), "not yaml at all")
            .await
            .unwrap();

        // No frontmatter.
        let nofm_dir = root.join("nofm");
        async_fs::create_dir_all(&nofm_dir).await.unwrap();
        async_fs::write(
            nofm_dir.join("SKILL.md"),
            "# Just markdown\nNo frontmatter here.",
        )
        .await
        .unwrap();

        let reg = SkillRegistry::from_paths(vec![root.clone()])
            .discover_skills()
            .await;

        assert_eq!(reg.skills().len(), 1);
        assert_eq!(reg.skills()[0].name, "good");

        async_fs::remove_dir_all(&root).await.unwrap();
    }

    #[tokio::test]
    async fn activation_deduplication() {
        let root = temp_dir("dedup");
        write_skill(&root, "test-skill", "Test.", "Body content here.").await;

        let reg = SkillRegistry::from_paths(vec![root.clone()])
            .discover_skills()
            .await;

        let tool = reg.build_tool();
        let call_id = ToolCallId::new("call-1");

        // First activation returns body.
        let request = ToolRequest {
            call_id: call_id.clone(),
            tool_name: ToolName::new(TOOL_NAME),
            input: json!({ "name": "test-skill" }),
            session_id: agentkit_core::SessionId::new("s"),
            turn_id: agentkit_core::TurnId::new("t"),
            metadata: MetadataMap::new(),
        };

        let noop_perms = NoopPermissions;
        let mut ctx = ToolContext {
            capability: agentkit_capabilities::CapabilityContext {
                session_id: None,
                turn_id: None,
                metadata: &MetadataMap::new(),
            },
            permissions: &noop_perms,
            resources: &(),
            cancellation: None,
            execution_scope: None,
            approved_request: None,
        };

        let result = tool.invoke(request, &mut ctx).await.unwrap();
        let text = match &result.result.output {
            ToolOutput::Text(t) => t.clone(),
            _ => panic!("expected text output"),
        };
        assert!(text.contains("Body content here."));

        // Second activation returns dedup message.
        let request2 = ToolRequest {
            call_id: ToolCallId::new("call-2"),
            tool_name: ToolName::new(TOOL_NAME),
            input: json!({ "name": "test-skill" }),
            session_id: agentkit_core::SessionId::new("s"),
            turn_id: agentkit_core::TurnId::new("t"),
            metadata: MetadataMap::new(),
        };

        let result2 = tool.invoke(request2, &mut ctx).await.unwrap();
        let text2 = match &result2.result.output {
            ToolOutput::Text(t) => t.clone(),
            _ => panic!("expected text output"),
        };
        assert_eq!(text2, "Skill already read.");

        async_fs::remove_dir_all(&root).await.unwrap();
    }

    #[tokio::test]
    async fn activation_includes_resources() {
        let root = temp_dir("resources");
        write_skill_with_resources(&root, "with-res").await;

        let reg = SkillRegistry::from_paths(vec![root.clone()])
            .discover_skills()
            .await;

        let skill = &reg.skills()[0];
        assert_eq!(skill.resources.len(), 2); // README.md + scripts/run.sh

        let tool = reg.build_tool();
        let request = ToolRequest {
            call_id: ToolCallId::new("call-1"),
            tool_name: ToolName::new(TOOL_NAME),
            input: json!({ "name": "with-res" }),
            session_id: agentkit_core::SessionId::new("s"),
            turn_id: agentkit_core::TurnId::new("t"),
            metadata: MetadataMap::new(),
        };

        let noop_perms = NoopPermissions;
        let mut ctx = ToolContext {
            capability: agentkit_capabilities::CapabilityContext {
                session_id: None,
                turn_id: None,
                metadata: &MetadataMap::new(),
            },
            permissions: &noop_perms,
            resources: &(),
            cancellation: None,
            execution_scope: None,
            approved_request: None,
        };

        let result = tool.invoke(request, &mut ctx).await.unwrap();
        let text = match &result.result.output {
            ToolOutput::Text(t) => t.clone(),
            _ => panic!("expected text output"),
        };
        assert!(text.contains("resources:"));
        assert!(text.contains("run.sh"));

        async_fs::remove_dir_all(&root).await.unwrap();
    }

    #[tokio::test]
    async fn filter_excludes_skills() {
        let root = temp_dir("filter");
        write_skill(&root, "keep-me", "Keeper.", "Body.").await;
        write_skill(&root, "drop-me", "Dropper.", "Body.").await;

        let reg = SkillRegistry::from_paths(vec![root.clone()])
            .with_filter(|skill: &Skill| skill.name != "drop-me")
            .discover_skills()
            .await;

        let skills = reg.skills();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "keep-me");

        async_fs::remove_dir_all(&root).await.unwrap();
    }

    #[tokio::test]
    async fn hidden_from_model_when_empty() {
        let root = temp_dir("empty");
        async_fs::create_dir_all(&root).await.unwrap();

        let reg = SkillRegistry::from_paths(vec![root.clone()])
            .discover_skills()
            .await;

        assert!(!reg.has_skills());
        let tool_reg = reg.tool_registry();
        assert!(tool_reg.specs().is_empty());

        async_fs::remove_dir_all(&root).await.unwrap();
    }

    #[tokio::test]
    async fn reload_picks_up_new_skills() {
        let root = temp_dir("reload");
        write_skill(&root, "initial", "Initial.", "Body.").await;

        let mut reg = SkillRegistry::from_paths(vec![root.clone()])
            .discover_skills()
            .await;

        assert_eq!(reg.skills().len(), 1);

        // Add a new skill and reload.
        write_skill(&root, "added", "Added later.", "New body.").await;
        reg.reload().await;

        assert_eq!(reg.skills().len(), 2);

        async_fs::remove_dir_all(&root).await.unwrap();
    }

    #[tokio::test]
    async fn frontmatter_stripped_from_body() {
        let root = temp_dir("strip-fm");
        let skill_dir = root.join("stripped");
        async_fs::create_dir_all(&skill_dir).await.unwrap();
        async_fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: stripped\ndescription: Test stripping.\nlicense: MIT\n---\n# Instructions\n\nDo the thing.",
        )
        .await
        .unwrap();

        let reg = SkillRegistry::from_paths(vec![root.clone()])
            .discover_skills()
            .await;

        let skill = &reg.skills()[0];
        assert!(!skill.body.contains("---"));
        assert!(!skill.body.contains("name:"));
        assert!(skill.body.contains("# Instructions"));
        assert!(skill.body.contains("Do the thing."));

        async_fs::remove_dir_all(&root).await.unwrap();
    }

    #[tokio::test]
    async fn lenient_yaml_handles_unquoted_colons() {
        let root = temp_dir("lenient");
        let skill_dir = root.join("colon-skill");
        async_fs::create_dir_all(&skill_dir).await.unwrap();
        async_fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: colon-skill\ndescription: Use this skill when: the user asks about PDFs\n---\nBody.",
        )
        .await
        .unwrap();

        let reg = SkillRegistry::from_paths(vec![root.clone()])
            .discover_skills()
            .await;

        assert_eq!(reg.skills().len(), 1);
        assert!(reg.skills()[0].description.contains("when:"));

        async_fs::remove_dir_all(&root).await.unwrap();
    }

    #[tokio::test]
    async fn tool_schema_constrains_names_to_enum() {
        let root = temp_dir("enum");
        write_skill(&root, "alpha", "Alpha.", "Body.").await;
        write_skill(&root, "beta", "Beta.", "Body.").await;

        let reg = SkillRegistry::from_paths(vec![root.clone()])
            .discover_skills()
            .await;

        let tool = reg.build_tool();
        let spec = tool.current_spec().unwrap();
        let schema = &spec.input_schema;
        let enum_values = schema["properties"]["name"]["enum"].as_array().unwrap();

        assert_eq!(enum_values.len(), 2);
        let names: Vec<&str> = enum_values.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));

        async_fs::remove_dir_all(&root).await.unwrap();
    }

    #[tokio::test]
    async fn activation_deduplicates_per_session() {
        let root = temp_dir("session-dedup");
        write_skill(&root, "test-skill", "Test.", "Body content here.").await;

        let reg = SkillRegistry::from_paths(vec![root.clone()])
            .discover_skills()
            .await;

        let tool = reg.build_tool();
        let noop_perms = NoopPermissions;
        let mut ctx = ToolContext {
            capability: agentkit_capabilities::CapabilityContext {
                session_id: None,
                turn_id: None,
                metadata: &MetadataMap::new(),
            },
            permissions: &noop_perms,
            resources: &(),
            cancellation: None,
            execution_scope: None,
            approved_request: None,
        };

        let first = tool
            .invoke(
                ToolRequest {
                    call_id: ToolCallId::new("call-1"),
                    tool_name: ToolName::new(TOOL_NAME),
                    input: json!({ "name": "test-skill" }),
                    session_id: agentkit_core::SessionId::new("session-a"),
                    turn_id: agentkit_core::TurnId::new("t1"),
                    metadata: MetadataMap::new(),
                },
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(
            matches!(first.result.output, ToolOutput::Text(ref t) if t.contains("Body content here."))
        );

        let second = tool
            .invoke(
                ToolRequest {
                    call_id: ToolCallId::new("call-2"),
                    tool_name: ToolName::new(TOOL_NAME),
                    input: json!({ "name": "test-skill" }),
                    session_id: agentkit_core::SessionId::new("session-b"),
                    turn_id: agentkit_core::TurnId::new("t1"),
                    metadata: MetadataMap::new(),
                },
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(
            matches!(second.result.output, ToolOutput::Text(ref t) if t.contains("Body content here."))
        );

        async_fs::remove_dir_all(&root).await.unwrap();
    }

    #[tokio::test]
    async fn filtered_duplicate_falls_back_to_later_root() {
        let project = temp_dir("project-filtered");
        let user = temp_dir("user-filtered");

        write_skill(&project, "review", "Project review.", "Project body.").await;
        write_skill(&user, "review", "User review.", "User body.").await;

        let reg = SkillRegistry::from_paths(vec![project.clone(), user.clone()])
            .with_filter(|skill: &Skill| skill.description != "Project review.")
            .discover_skills()
            .await;

        let skills = reg.skills();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "User review.");

        async_fs::remove_dir_all(&project).await.unwrap();
        async_fs::remove_dir_all(&user).await.unwrap();
    }

    #[tokio::test]
    async fn current_spec_rediscovers_skills_without_reload() {
        let root = temp_dir("dynamic-spec");
        write_skill(&root, "initial", "Initial.", "Body.").await;

        let reg = SkillRegistry::from_paths(vec![root.clone()])
            .discover_skills()
            .await;
        let tool = reg.build_tool();

        let initial_spec = tool.current_spec().unwrap();
        let initial_names = initial_spec.input_schema["properties"]["name"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(initial_names, vec!["initial"]);

        write_skill(&root, "added", "Added.", "Body.").await;

        let refreshed_spec = tool.current_spec().unwrap();
        let refreshed_names = refreshed_spec.input_schema["properties"]["name"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(refreshed_names, vec!["added", "initial"]);

        async_fs::remove_dir_all(&root).await.unwrap();
    }

    #[tokio::test]
    async fn skips_skills_that_violate_required_spec_validation() {
        let root = temp_dir("spec-validation");

        write_skill(&root, "valid-skill", "Valid description.", "Body.").await;

        let uppercase_dir = root.join("Uppercase");
        async_fs::create_dir_all(&uppercase_dir).await.unwrap();
        async_fs::write(
            uppercase_dir.join("SKILL.md"),
            "---\nname: Uppercase\ndescription: Invalid uppercase name.\n---\nBody.",
        )
        .await
        .unwrap();

        let mismatched_dir = root.join("dir-name");
        async_fs::create_dir_all(&mismatched_dir).await.unwrap();
        async_fs::write(
            mismatched_dir.join("SKILL.md"),
            "---\nname: other-name\ndescription: Parent mismatch.\n---\nBody.",
        )
        .await
        .unwrap();

        let long_desc = "x".repeat(1025);
        let long_desc_dir = root.join("too-long");
        async_fs::create_dir_all(&long_desc_dir).await.unwrap();
        async_fs::write(
            long_desc_dir.join("SKILL.md"),
            format!("---\nname: too-long\ndescription: {long_desc}\n---\nBody."),
        )
        .await
        .unwrap();

        let reg = SkillRegistry::from_paths(vec![root.clone()])
            .discover_skills()
            .await;

        let names: Vec<&str> = reg
            .skills()
            .iter()
            .map(|skill| skill.name.as_str())
            .collect();
        assert_eq!(names, vec!["valid-skill"]);

        async_fs::remove_dir_all(&root).await.unwrap();
    }

    #[tokio::test]
    async fn missing_root_does_not_error() {
        let reg = SkillRegistry::from_paths(vec![PathBuf::from("/nonexistent/path/skills")])
            .discover_skills()
            .await;

        assert!(!reg.has_skills());
    }

    #[tokio::test]
    async fn reset_activations_allows_reread() {
        let root = temp_dir("reset");
        write_skill(&root, "reread", "Reread.", "Content.").await;

        let reg = SkillRegistry::from_paths(vec![root.clone()])
            .discover_skills()
            .await;

        let tool = reg.build_tool();
        let noop_perms = NoopPermissions;
        let mut ctx = ToolContext {
            capability: agentkit_capabilities::CapabilityContext {
                session_id: None,
                turn_id: None,
                metadata: &MetadataMap::new(),
            },
            permissions: &noop_perms,
            resources: &(),
            cancellation: None,
            execution_scope: None,
            approved_request: None,
        };

        // First read.
        let req1 = ToolRequest {
            call_id: ToolCallId::new("c1"),
            tool_name: ToolName::new(TOOL_NAME),
            input: json!({ "name": "reread" }),
            session_id: agentkit_core::SessionId::new("s"),
            turn_id: agentkit_core::TurnId::new("t"),
            metadata: MetadataMap::new(),
        };
        let r1 = tool.invoke(req1, &mut ctx).await.unwrap();
        assert!(matches!(r1.result.output, ToolOutput::Text(ref t) if t.contains("Content.")));

        // Second read is deduplicated.
        let req2 = ToolRequest {
            call_id: ToolCallId::new("c2"),
            tool_name: ToolName::new(TOOL_NAME),
            input: json!({ "name": "reread" }),
            session_id: agentkit_core::SessionId::new("s"),
            turn_id: agentkit_core::TurnId::new("t"),
            metadata: MetadataMap::new(),
        };
        let r2 = tool.invoke(req2, &mut ctx).await.unwrap();
        assert!(matches!(r2.result.output, ToolOutput::Text(ref t) if t == "Skill already read."));

        // Reset and re-read.
        reg.reset_activations();
        let req3 = ToolRequest {
            call_id: ToolCallId::new("c3"),
            tool_name: ToolName::new(TOOL_NAME),
            input: json!({ "name": "reread" }),
            session_id: agentkit_core::SessionId::new("s"),
            turn_id: agentkit_core::TurnId::new("t"),
            metadata: MetadataMap::new(),
        };
        let r3 = tool.invoke(req3, &mut ctx).await.unwrap();
        assert!(matches!(r3.result.output, ToolOutput::Text(ref t) if t.contains("Content.")));

        async_fs::remove_dir_all(&root).await.unwrap();
    }

    // -- test helpers -------------------------------------------------------

    struct NoopPermissions;

    impl agentkit_tools_core::PermissionChecker for NoopPermissions {
        fn evaluate(
            &self,
            _request: &dyn agentkit_tools_core::PermissionRequest,
        ) -> agentkit_tools_core::PermissionDecision {
            agentkit_tools_core::PermissionDecision::Allow
        }
    }
}
