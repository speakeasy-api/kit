use std::collections::BTreeMap;

use agentkit_core::Item;
use agentkit_tool_skills::Skill;
use serde::Serialize;

// Local TUI commands and the built-in compactor always take precedence.
fn command_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        && !matches!(
            name,
            "compact"
                | "voice"
                | "new"
                | "resume"
                | "sessions"
                | "close"
                | "model"
                | "effort"
                | "agents"
                | "login"
                | "usage"
        )
}

pub(super) fn commands(skills: &[Skill]) -> Vec<(String, String)> {
    let mut commands = skills
        .iter()
        .filter(|skill| command_name(&skill.name))
        .map(|skill| (skill.name.clone(), skill.description.clone()))
        .collect::<BTreeMap<_, _>>()
        .into_iter()
        .collect::<Vec<_>>();
    commands.insert(0, ("compact".into(), "Compact the session context".into()));
    commands
}

pub(super) async fn poll_commands(
    runtime: std::sync::Arc<crate::runtime::Runtime>,
    mut publish: impl FnMut(Vec<(String, String)>),
) {
    let mut previous = None;
    loop {
        match runtime.polled_skills().await {
            Ok(skills) => {
                let current = commands(&skills);
                if previous.as_ref() != Some(&current) {
                    publish(current.clone());
                    previous = Some(current);
                }
            }
            Err(error) => {
                tracing::warn!(%error, "could not refresh skill commands");
                if previous.is_none() {
                    let fallback = commands(&[]);
                    publish(fallback.clone());
                    previous = Some(fallback);
                }
            }
        }
        let interval = runtime.skill_poll_interval();
        if interval.is_zero() {
            std::future::pending::<()>().await;
        }
        tokio::time::sleep(interval).await;
    }
}

/// Resolve only the leading user text, never embedded or quoted slash commands.
/// Unknown/removed skills stay ordinary prompts, matching other unknown commands.
pub(super) fn expand_items(skills: &[Skill], items: &mut [Item]) {
    let Some(item) = items
        .iter_mut()
        .find(|item| item.kind == agentkit_core::ItemKind::User)
    else {
        return;
    };
    let Some(agentkit_core::Part::Text(text)) = item.parts.first_mut() else {
        return;
    };
    let Some(command) = text.text.trim_start().strip_prefix('/') else {
        return;
    };
    let end = command.find(char::is_whitespace).unwrap_or(command.len());
    let name = &command[..end];
    if !command_name(name) {
        return;
    }
    let Some(skill) = skills.iter().find(|skill| skill.name == name) else {
        return;
    };
    let args = command[end..].trim();
    let mut expanded = format!(
        "skill: {}\ndir: {}\n\n{}",
        skill.name,
        skill.base_dir.display(),
        skill.body
    );
    if !skill.resources.is_empty() {
        expanded.push_str("\n\nresources:\n");
        for resource in &skill.resources {
            expanded.push_str(&format!("  - {}\n", resource.display()));
        }
    }
    if !args.is_empty() {
        expanded.push_str("\n\nUser arguments:\n");
        expanded.push_str(args);
    }
    text.text = expanded;
}

const MAX_NOTIFICATION_BYTES: usize = 2_048;
const MAX_REPORTED_CHANGES: usize = 64;

type Catalog = BTreeMap<String, blake3::Hash>;

#[derive(Debug)]
pub(super) enum SubmitError<E> {
    Catalog(serde_json::Error),
    Submit(E),
}

#[derive(Serialize)]
struct Fingerprint<'a> {
    description: &'a str,
    body: &'a str,
    frontmatter: FingerprintFrontmatter<'a>,
}

#[derive(Serialize)]
struct FingerprintFrontmatter<'a> {
    license: Option<&'a str>,
    compatibility: Option<&'a str>,
    metadata: &'a BTreeMap<String, String>,
    allowed_tools: Option<&'a str>,
}

pub(super) struct SkillCatalogMonitor {
    baseline: Catalog,
}

impl SkillCatalogMonitor {
    pub(super) fn new(skills: &[Skill]) -> serde_json::Result<Self> {
        Ok(Self {
            baseline: catalog(skills)?,
        })
    }

    pub(super) fn submit<E>(
        &mut self,
        skills: &[Skill],
        mut user_items: Vec<Item>,
        submit: impl FnOnce(Vec<Item>) -> Result<(), E>,
    ) -> Result<(), SubmitError<E>> {
        let current = catalog(skills).map_err(SubmitError::Catalog)?;
        if let Some(notification) = notification(&self.baseline, &current, skills) {
            user_items.insert(0, Item::notification(notification));
        }
        submit(user_items).map_err(SubmitError::Submit)?;
        self.baseline = current;
        Ok(())
    }
}

fn catalog(skills: &[Skill]) -> serde_json::Result<Catalog> {
    skills
        .iter()
        .map(|skill| Ok((skill.name.clone(), fingerprint(skill)?)))
        .collect()
}

fn fingerprint(skill: &Skill) -> serde_json::Result<blake3::Hash> {
    let mut resources = skill
        .resources
        .iter()
        .map(|path| path.strip_prefix(&skill.base_dir).unwrap_or(path))
        .collect::<Vec<_>>();
    resources.sort();
    let semantic = Fingerprint {
        description: &skill.description,
        body: &skill.body,
        frontmatter: FingerprintFrontmatter {
            license: skill.frontmatter.license.as_deref(),
            compatibility: skill.frontmatter.compatibility.as_deref(),
            metadata: &skill.frontmatter.metadata,
            allowed_tools: skill.frontmatter.allowed_tools.as_deref(),
        },
    };
    let mut fingerprint = blake3::Hasher::new();
    fingerprint.update(&serde_json::to_vec(&semantic)?);
    for resource in resources {
        let path = resource.as_os_str().as_encoded_bytes();
        fingerprint.update(&(path.len() as u64).to_le_bytes());
        fingerprint.update(path);
    }
    Ok(fingerprint.finalize())
}

fn notification(previous: &Catalog, current: &Catalog, skills: &[Skill]) -> Option<String> {
    if previous == current {
        return None;
    }

    let descriptions = skills
        .iter()
        .map(|skill| (skill.name.as_str(), skill.description.as_str()))
        .collect::<BTreeMap<_, _>>();
    let added = current
        .keys()
        .filter(|name| !previous.contains_key(*name))
        .filter_map(|name| {
            descriptions
                .get(name.as_str())
                .map(|description| (name.as_str(), *description))
        })
        .collect::<Vec<_>>();
    let changed = current
        .iter()
        .filter(|(name, fingerprint)| previous.get(*name).is_some_and(|old| old != *fingerprint))
        .filter_map(|(name, _)| {
            descriptions
                .get(name.as_str())
                .map(|description| (name.as_str(), *description))
        })
        .collect::<Vec<_>>();
    let removed = previous
        .keys()
        .filter(|name| !current.contains_key(*name))
        .map(String::as_str)
        .collect::<Vec<_>>();
    let total = added.len() + changed.len() + removed.len();
    let omission_reserve = format!("\n{total} more change(s) omitted").len();
    let content_limit = MAX_NOTIFICATION_BYTES.saturating_sub(omission_reserve);
    let mut message = String::from("Skill catalog update (informational).");
    let mut reported = 0;
    append_skill_group(
        &mut message,
        "Added skills",
        &added,
        &mut reported,
        content_limit,
    );
    append_skill_group(
        &mut message,
        "Changed skills",
        &changed,
        &mut reported,
        content_limit,
    );
    append_removed_group(&mut message, &removed, &mut reported, content_limit);
    if reported < total {
        message.push_str(&format!("\n{} more change(s) omitted", total - reported));
    }
    Some(message)
}

fn append_skill_group(
    message: &mut String,
    label: &str,
    skills: &[(&str, &str)],
    reported: &mut usize,
    content_limit: usize,
) {
    if skills.is_empty() || *reported >= MAX_REPORTED_CHANGES {
        return;
    }
    let heading = format!("\n{label}:");
    if message.len() + heading.len() > content_limit {
        return;
    }
    let heading_start = message.len();
    message.push_str(&heading);
    let mut group_count = 0;
    for (name, description) in skills
        .iter()
        .take(MAX_REPORTED_CHANGES.saturating_sub(*reported))
    {
        const ROW_OVERHEAD: usize = "\n- name: \"\"\n  description: \"\"".len();
        if message.len() + ROW_OVERHEAD + name.len() + description.len() > content_limit {
            continue;
        }
        let row = format!(
            "\n- name: {}\n  description: {}",
            serde_json::Value::String((*name).to_owned()),
            serde_json::Value::String((*description).to_owned())
        );
        if message.len() + row.len() > content_limit {
            continue;
        }
        message.push_str(&row);
        group_count += 1;
        *reported += 1;
    }
    if group_count == 0 {
        message.truncate(heading_start);
    }
}

fn append_removed_group(
    message: &mut String,
    names: &[&str],
    reported: &mut usize,
    content_limit: usize,
) {
    if names.is_empty() || *reported >= MAX_REPORTED_CHANGES {
        return;
    }
    const HEADING: &str = "\nRemoved skills:";
    if message.len() + HEADING.len() > content_limit {
        return;
    }
    let heading_start = message.len();
    message.push_str(HEADING);
    let mut group_count = 0;
    for name in names
        .iter()
        .take(MAX_REPORTED_CHANGES.saturating_sub(*reported))
    {
        let row = format!(
            "\n- name: {}",
            serde_json::Value::String((*name).to_owned())
        );
        if message.len() + row.len() > content_limit {
            continue;
        }
        message.push_str(&row);
        group_count += 1;
        *reported += 1;
    }
    if group_count == 0 {
        message.truncate(heading_start);
    }
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
mod tests {
    use std::path::PathBuf;

    use agentkit_core::{ItemKind, Part, TextPart};
    use agentkit_tool_skills::SkillFrontmatter;

    use super::*;

    fn skill(name: &str, body: &str) -> Skill {
        let base_dir = PathBuf::from("/skills").join(name);
        Skill {
            name: name.into(),
            description: format!("Description for {name}"),
            location: base_dir.join("SKILL.md"),
            base_dir,
            body: body.into(),
            resources: Vec::new(),
            frontmatter: SkillFrontmatter::default(),
        }
    }

    fn notification_text(items: &[Item]) -> Option<&str> {
        let item = items
            .first()
            .filter(|item| item.kind == ItemKind::Notification)?;
        match item.parts.first() {
            Some(Part::Text(TextPart { text, .. })) => Some(text),
            _ => None,
        }
    }

    #[test]
    fn command_lists_ignore_body_edits_but_include_description_edits() {
        let original = skill("review", "old body");
        let mut edited = original.clone();
        edited.body = "new instructions".into();
        assert_eq!(
            commands(std::slice::from_ref(&original)),
            commands(std::slice::from_ref(&edited))
        );
        edited.description = "Updated description".into();
        assert_ne!(commands(&[original]), commands(&[edited]));
    }

    #[test]
    fn slash_commands_filter_collisions_and_expand_current_instructions() {
        let mut current = skill("review", "Current instructions");
        current.resources.push(current.base_dir.join("guide.md"));
        let skills = vec![skill("compact", "wrong"), skill("new", "wrong"), current];
        assert_eq!(
            commands(&skills)
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["compact", "review"]
        );
        let mut items = vec![Item::text(ItemKind::User, "/review focus on tests")];
        expand_items(&skills, &mut items);
        let Part::Text(text) = &items[0].parts[0] else {
            panic!("text")
        };
        assert_eq!(
            text.text,
            "skill: review\ndir: /skills/review\n\nCurrent instructions\n\nresources:\n  - /skills/review/guide.md\n\n\nUser arguments:\nfocus on tests"
        );
        for input in [
            "/removed args",
            "/compact",
            "/new",
            "Explain /review",
            "/review-other",
        ] {
            let mut items = vec![Item::text(ItemKind::User, input)];
            expand_items(&skills, &mut items);
            let Part::Text(text) = &items[0].parts[0] else {
                panic!("text")
            };
            assert_eq!(text.text, input);
        }
        let mut items = vec![Item::text(ItemKind::User, "/review")];
        expand_items(&skills, &mut items);
        let Part::Text(text) = &items[0].parts[0] else {
            panic!("text")
        };
        assert!(!text.text.contains("User arguments:"));
    }

    fn write_skill(root: &std::path::Path, description: &str, body: &str) {
        let directory = root.join(".agents/skills/live");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("SKILL.md"),
            format!("---\nname: live\ndescription: {description}\n---\n{body}\n"),
        )
        .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn polling_replaces_catalog_after_add_edit_remove_and_stops_on_drop() {
        let root = tempfile::tempdir().unwrap();
        let runtime = crate::runtime::Runtime::new(root.path(), "gpt-5.4").unwrap();
        let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(poll_commands(runtime.clone(), move |commands| {
            send.send(commands).unwrap();
        }));
        assert_eq!(receive.recv().await.unwrap(), commands(&[]));
        write_skill(root.path(), "First description", "first body");
        tokio::time::advance(std::time::Duration::from_secs(5)).await;
        let added = receive.recv().await.unwrap();
        assert!(added.contains(&("live".into(), "First description".into())));
        write_skill(root.path(), "Second description", "new body");
        tokio::time::advance(std::time::Duration::from_secs(5)).await;
        let edited = receive.recv().await.unwrap();
        assert!(edited.contains(&("live".into(), "Second description".into())));
        let current = runtime.current_skills().await.unwrap();
        let mut items = vec![Item::text(ItemKind::User, "/live")];
        expand_items(&current.skills, &mut items);
        let Part::Text(text) = &items[0].parts[0] else {
            panic!("text")
        };
        assert!(text.text.contains("new body"));
        drop(current);
        std::fs::remove_dir_all(root.path().join(".agents/skills/live")).unwrap();
        tokio::time::advance(std::time::Duration::from_secs(5)).await;
        assert_eq!(receive.recv().await.unwrap(), commands(&[]));
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(receive.recv().await.is_none());
        assert!(runtime.current_skills().await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn disabled_polling_still_advertises_initial_catalog() {
        let root = tempfile::tempdir().unwrap();
        write_skill(root.path(), "Initial description", "body");
        let runtime = crate::runtime::Runtime::with_skill_poll_interval(
            crate::runtime::Runtime::new(root.path(), "gpt-5.4").unwrap(),
            std::time::Duration::ZERO,
        )
        .unwrap();
        let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(poll_commands(runtime, move |commands| {
            send.send(commands).unwrap();
        }));
        assert!(
            receive
                .recv()
                .await
                .unwrap()
                .iter()
                .any(|(name, _)| name == "live")
        );
        write_skill(root.path(), "Changed description", "body");
        tokio::time::advance(std::time::Duration::from_secs(60)).await;
        assert!(matches!(
            receive.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn disabled_polling_advertises_compact_when_initial_refresh_fails() {
        let root = tempfile::tempdir().unwrap();
        let plugin_config = root.path().join("config.toml");
        std::fs::write(&plugin_config, "").unwrap();
        let plugins = crate::plugins::PluginRuntime::load(
            plugin_config,
            root.path().to_path_buf(),
            root.path().join("cache"),
            root.path().join("data"),
        )
        .await
        .unwrap();
        let runtime = crate::runtime::Runtime::with_plugin_runtime(
            crate::runtime::Runtime::with_skill_poll_interval(
                crate::runtime::Runtime::new(root.path(), "gpt-5.4").unwrap(),
                std::time::Duration::ZERO,
            )
            .unwrap(),
            Some(plugins),
        )
        .unwrap();
        let mcp_config = root.path().join("mcp.json");
        std::fs::write(&mcp_config, "{\"mcpServers\":{}}").unwrap();
        let runtime = crate::runtime::Runtime::with_mcp_config(
            runtime,
            Some(&mcp_config),
            Vec::new(),
            false,
            crate::tools::mcp::CredentialStorage::Memory,
        )
        .await
        .unwrap();
        std::fs::write(&mcp_config, "invalid JSON").unwrap();
        assert!(
            runtime
                .polled_skills()
                .await
                .unwrap_err()
                .contains("invalid MCP config")
        );
        let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(poll_commands(runtime, move |commands| {
            send.send(commands).unwrap();
        }));
        let advertised = tokio::time::timeout(std::time::Duration::from_secs(10), receive.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(advertised, commands(&[]));
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }

    #[test]
    fn fingerprint_covers_semantic_fields_and_sorted_resource_paths() {
        let mut original = skill("semantic", "body");
        original.resources = vec![
            original.base_dir.join("z.txt"),
            original.base_dir.join("a.txt"),
        ];

        let mut reordered = original.clone();
        reordered.resources.reverse();
        assert_eq!(
            fingerprint(&original).unwrap(),
            fingerprint(&reordered).unwrap()
        );

        let mut changed = original.clone();
        changed.description.push_str(" changed");
        assert_ne!(
            fingerprint(&original).unwrap(),
            fingerprint(&changed).unwrap()
        );
        changed = original.clone();
        changed.body.push_str(" changed");
        assert_ne!(
            fingerprint(&original).unwrap(),
            fingerprint(&changed).unwrap()
        );
        changed = original.clone();
        changed
            .frontmatter
            .metadata
            .insert("key".into(), "value".into());
        assert_ne!(
            fingerprint(&original).unwrap(),
            fingerprint(&changed).unwrap()
        );
        changed = original.clone();
        changed.resources.push(changed.base_dir.join("new.txt"));
        assert_ne!(
            fingerprint(&original).unwrap(),
            fingerprint(&changed).unwrap()
        );
    }

    #[test]
    fn baseline_and_unchanged_catalog_do_not_notify() {
        let skills = vec![skill("existing", "body")];
        let mut monitor = SkillCatalogMonitor::new(&skills).unwrap();
        monitor
            .submit(
                &skills,
                vec![Item::text(ItemKind::User, "hello")],
                |items| {
                    assert_eq!(items.len(), 1);
                    assert_eq!(items[0].kind, ItemKind::User);
                    Ok::<_, ()>(())
                },
            )
            .unwrap();
    }

    #[test]
    fn reports_added_changed_and_removed_names_in_the_user_batch() {
        let before = vec![skill("removed", "old"), skill("changed", "old")];
        let after = vec![skill("added", "new"), skill("changed", "new")];
        let mut monitor = SkillCatalogMonitor::new(&before).unwrap();
        monitor
            .submit(&after, vec![Item::text(ItemKind::User, "hello")], |items| {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].kind, ItemKind::Notification);
                assert_eq!(items[1].kind, ItemKind::User);
                let text = notification_text(&items).unwrap();
                assert!(text.starts_with("Skill catalog update (informational)."));
                assert!(text.contains(
                    "Added skills:\n- name: \"added\"\n  description: \"Description for added\""
                ));
                assert!(text.contains(
                    "Changed skills:\n- name: \"changed\"\n  description: \"Description for changed\""
                ));
                assert!(text.contains("Removed skills:\n- name: \"removed\""));
                Ok::<_, ()>(())
            })
            .unwrap();
    }

    #[test]
    fn notification_is_sorted_and_bounded() {
        let after = (0..100)
            .rev()
            .map(|index| skill(&format!("skill-{index:03}"), "body"))
            .collect::<Vec<_>>();
        let mut monitor = SkillCatalogMonitor::new(&[]).unwrap();
        monitor
            .submit(&after, vec![Item::text(ItemKind::User, "hello")], |items| {
                let text = notification_text(&items).unwrap();
                assert!(text.len() <= MAX_NOTIFICATION_BYTES);
                assert!(text.find("skill-000").unwrap() < text.find("skill-001").unwrap());
                assert!(text.contains("more change(s) omitted"));
                Ok::<_, ()>(())
            })
            .unwrap();
    }

    #[test]
    fn failed_submission_does_not_advance_baseline() {
        let after = vec![skill("added", "body")];
        let mut monitor = SkillCatalogMonitor::new(&[]).unwrap();
        assert!(
            monitor
                .submit(&after, vec![Item::text(ItemKind::User, "first")], |_| Err(
                    ()
                ))
                .is_err()
        );
        monitor
            .submit(&after, vec![Item::text(ItemKind::User, "retry")], |items| {
                assert!(
                    notification_text(&items)
                        .unwrap()
                        .contains("Added skills:\n- name: \"added\"")
                );
                Ok::<_, ()>(())
            })
            .unwrap();
    }
}
