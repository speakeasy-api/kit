use std::collections::BTreeMap;

use agentkit_core::Item;
use agentkit_tool_skills::Skill;
use serde::Serialize;

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
