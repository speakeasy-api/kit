//! Write-heavy CRM cleanup: normalize phone numbers to E.164 and backfill
//! missing company names from the contact's email domain. Many small,
//! mechanical writes — one `update_contact` round-trip each in the granular
//! arm, or a single loop in a compose script.

use std::sync::{Arc, Mutex};

use agentkit_tools_core::ToolRegistry;
use serde_json::{Value, json};

use crate::scenario::{
    Arm, BenchError, FnTool, Scenario, ScenarioInstance, Score, get_str, get_u64, page_schema,
    paginate, submit_result_tool,
};

const COMPANIES: [(&str, &str); 4] = [
    ("acme.com", "Acme Corporation"),
    ("globex.io", "Globex"),
    ("initech.dev", "Initech"),
    ("umbrella.co", "Umbrella Holdings"),
];

#[derive(Clone)]
struct Contact {
    id: String,
    name: String,
    email: String,
    phone: String,
    company: String,
}

fn fixture() -> Vec<Contact> {
    (1..=24u64)
        .map(|i| {
            let last4 = format!("{:04}", 100 + i);
            let phone = match i % 4 {
                0 => format!("+1415555{last4}"),
                1 => format!("(415) 555-{last4}"),
                2 => format!("415.555.{last4}"),
                _ => format!("+1-415-555-{last4}"),
            };
            let domain = COMPANIES[(i % 4) as usize].0;
            let company = if i % 5 == 0 {
                String::new()
            } else {
                COMPANIES[(i % 4) as usize].1.to_string()
            };
            Contact {
                id: format!("CT-{:03}", i),
                name: format!("Contact {i:02}"),
                email: format!("contact{i:02}@{domain}"),
                phone,
                company,
            }
        })
        .collect()
}

/// The benchmark's own normalizer — ground truth for scoring.
fn normalize_phone(raw: &str) -> Option<String> {
    let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
    match digits.len() {
        10 => Some(format!("+1{digits}")),
        11 if digits.starts_with('1') => Some(format!("+{digits}")),
        _ => None,
    }
}

fn company_for_email(email: &str) -> Option<&'static str> {
    let domain = email.split('@').nth(1)?;
    COMPANIES
        .iter()
        .find(|(d, _)| *d == domain)
        .map(|(_, name)| *name)
}

fn expected_contact(contact: &Contact) -> Contact {
    let mut expected = contact.clone();
    if let Some(e164) = normalize_phone(&contact.phone) {
        expected.phone = e164;
    }
    if expected.company.is_empty()
        && let Some(company) = company_for_email(&contact.email)
    {
        expected.company = company.to_string();
    }
    expected
}

pub struct CrmHygiene;

impl Scenario for CrmHygiene {
    fn name(&self) -> &'static str {
        "crm-hygiene"
    }

    fn setup(&self, _arm: Arm) -> Result<ScenarioInstance, BenchError> {
        let world = Arc::new(Mutex::new(fixture()));

        let list_world = world.clone();
        let list_contacts = FnTool::new(
            "list_contacts",
            "List CRM contacts with all fields (id, name, email, phone, company). 8 per page.",
            json!({
                "type": "object",
                "properties": {
                    "page": { "type": "integer", "minimum": 1, "default": 1 }
                },
                "additionalProperties": false
            }),
            page_schema(json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "name": { "type": "string" },
                    "email": { "type": "string" },
                    "phone": { "type": "string" },
                    "company": { "type": "string" }
                }
            })),
            move |input| {
                let page = get_u64(input, "page", 1);
                let contacts = list_world.lock().expect("world lock");
                let items: Vec<Value> = contacts
                    .iter()
                    .map(|c| {
                        json!({
                            "id": c.id,
                            "name": c.name,
                            "email": c.email,
                            "phone": c.phone,
                            "company": c.company,
                        })
                    })
                    .collect();
                Ok(paginate(items, page, 8))
            },
        )
        .read_only()
        // First call per run fails with a transient 503 (see FnTool::flaky).
        .flaky(1);

        let list_companies = FnTool::new(
            "list_companies",
            "List known companies and their email domains, for mapping contacts to companies.",
            json!({ "type": "object", "additionalProperties": false }),
            json!({
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "domain": { "type": "string" },
                        "name": { "type": "string" }
                    }
                }
            }),
            move |_input| {
                Ok(Value::Array(
                    COMPANIES
                        .iter()
                        .map(|(domain, name)| json!({ "domain": domain, "name": name }))
                        .collect(),
                ))
            },
        )
        .read_only();

        let update_world = world.clone();
        let update_contact = FnTool::new(
            "update_contact",
            "Update a contact's phone and/or company. Returns the updated contact.",
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "phone": { "type": "string" },
                    "company": { "type": "string" }
                },
                "required": ["id"],
                "additionalProperties": false
            }),
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "phone": { "type": "string" },
                    "company": { "type": "string" }
                }
            }),
            move |input| {
                let id = get_str(input, "id")?.to_string();
                let mut contacts = update_world.lock().expect("world lock");
                let contact = contacts
                    .iter_mut()
                    .find(|c| c.id == id)
                    .ok_or_else(|| format!("no contact with id {id}"))?;
                if let Some(phone) = input.get("phone").and_then(Value::as_str) {
                    contact.phone = phone.to_string();
                }
                if let Some(company) = input.get("company").and_then(Value::as_str) {
                    contact.company = company.to_string();
                }
                Ok(json!({ "id": contact.id, "phone": contact.phone, "company": contact.company }))
            },
        );

        let (submit, submission) = submit_result_tool(json!({
            "type": "object",
            "properties": {
                "phones_fixed": { "type": "integer" },
                "companies_filled": { "type": "integer" }
            },
            "required": ["phones_fixed", "companies_filled"]
        }));

        let tools = ToolRegistry::new()
            .with(list_contacts)
            .with(list_companies)
            .with(update_contact)
            .with(submit);

        let user_prompt = "Clean up our CRM contacts: (1) rewrite every phone number that is not \
             already in E.164 format (US numbers: `+1` followed by exactly 10 digits, no spaces or \
             punctuation) into E.164, leaving already-valid numbers untouched; (2) for every \
             contact whose company field is empty, fill it with the company name matching the \
             contact's email domain (see list_companies). Do not change any other field or any \
             already-correct value. When done, submit \
             {\"phones_fixed\": <count>, \"companies_filled\": <count>} via submit_result."
            .to_string();

        let scorer_world = world;
        let scorer_submission = submission.clone();
        let scorer = Box::new(move || {
            let contacts = scorer_world.lock().expect("world lock");
            let originals = fixture();
            let expected_phone_fixes = originals
                .iter()
                .filter(|c| normalize_phone(&c.phone).is_some_and(|e| e != c.phone))
                .count();
            let expected_company_fills = originals.iter().filter(|c| c.company.is_empty()).count();

            let mut correct = 0usize;
            let mut details = Vec::new();
            for (current, original) in contacts.iter().zip(originals.iter()) {
                let expected = expected_contact(original);
                if current.phone == expected.phone && current.company == expected.company {
                    correct += 1;
                } else if details.len() < 5 {
                    details.push(format!(
                        "{}: expected phone={} company={:?}, got phone={} company={:?}",
                        current.id,
                        expected.phone,
                        expected.company,
                        current.phone,
                        current.company
                    ));
                }
            }
            let state_score = correct as f64 / contacts.len() as f64;

            let submitted = scorer_submission.lock().expect("submission lock").clone();
            let count = |key: &str| -> Option<u64> {
                submitted
                    .as_ref()
                    .and_then(|v| v.get(key))
                    .and_then(Value::as_u64)
            };
            let counts_score =
                (u64::from(count("phones_fixed") == Some(expected_phone_fixes as u64))
                    + u64::from(count("companies_filled") == Some(expected_company_fills as u64)))
                    as f64
                    / 2.0;

            let mut notes = vec![format!(
                "state: {correct}/{} contacts in expected final state; expected {expected_phone_fixes} phone fixes, {expected_company_fills} company fills; submitted {submitted:?}",
                contacts.len()
            )];
            notes.extend(details);
            Score {
                accuracy: 0.8 * state_score + 0.2 * counts_score,
                notes,
            }
        });

        Ok(ScenarioInstance {
            tools,
            user_prompt,
            permissions: None,
            submission,
            scorer,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_needs_fixes_on_both_axes() {
        let contacts = fixture();
        let phone_fixes = contacts
            .iter()
            .filter(|c| normalize_phone(&c.phone).is_some_and(|e| e != c.phone))
            .count();
        let company_fills = contacts.iter().filter(|c| c.company.is_empty()).count();
        assert_eq!(phone_fixes, 18);
        assert_eq!(company_fills, 4);
        // Already-valid phones exist and normalize to themselves (must be untouched).
        assert!(
            contacts
                .iter()
                .any(|c| normalize_phone(&c.phone).as_deref() == Some(c.phone.as_str()))
        );
        // Every empty company is recoverable from the email domain.
        for contact in contacts.iter().filter(|c| c.company.is_empty()) {
            assert!(company_for_email(&contact.email).is_some());
        }
    }
}
