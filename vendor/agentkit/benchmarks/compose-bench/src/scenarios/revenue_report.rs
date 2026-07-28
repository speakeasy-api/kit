//! Read-only N+1 aggregation: total completed March 2026 revenue per customer
//! region. The list endpoint only exposes order ids and customer ids; amounts
//! and statuses require `get_order`, regions require `get_customer` — the
//! classic fan-out an agent either pays one round-trip per call for, or folds
//! into a single compose script.

use std::collections::BTreeMap;

use agentkit_tools_core::ToolRegistry;
use serde_json::{Value, json};

use crate::scenario::{
    Arm, BenchError, FnTool, Scenario, ScenarioInstance, Score, get_str, get_u64, page_schema,
    paginate, submit_result_tool,
};

const REGIONS: [&str; 3] = ["NA", "EU", "APAC"];

#[derive(Clone)]
struct Order {
    id: String,
    customer_id: String,
    month: &'static str,
    status: &'static str,
    amount_cents: u64,
}

fn customer_region(index: u64) -> &'static str {
    REGIONS[(index % 3) as usize]
}

fn fixture() -> Vec<Order> {
    let mut orders: Vec<Order> = (1..=40u64)
        .map(|i| Order {
            id: format!("O-{}", 2000 + i),
            customer_id: format!("C-{:02}", (i * 7) % 18 + 1),
            month: "2026-03",
            status: if i % 7 == 0 {
                "refunded"
            } else if i % 11 == 0 {
                "pending"
            } else {
                "completed"
            },
            amount_cents: 1000 + i * 137,
        })
        .collect();
    // Noise outside the requested month.
    orders.extend((1..=8u64).map(|i| Order {
        id: format!("O-{}", 2100 + i),
        customer_id: format!("C-{:02}", i % 18 + 1),
        month: if i % 2 == 0 { "2026-02" } else { "2026-04" },
        status: "completed",
        amount_cents: 5000 + i * 311,
    }));
    orders
}

fn expected_totals() -> BTreeMap<&'static str, u64> {
    let mut totals: BTreeMap<&'static str, u64> = REGIONS.iter().map(|r| (*r, 0)).collect();
    for order in fixture() {
        if order.month != "2026-03" || order.status != "completed" {
            continue;
        }
        let customer_index: u64 = order.customer_id[2..].parse().expect("customer index");
        *totals
            .get_mut(customer_region(customer_index))
            .expect("region") += order.amount_cents;
    }
    totals
}

pub struct RevenueReport;

impl Scenario for RevenueReport {
    fn name(&self) -> &'static str {
        "revenue-report"
    }

    fn setup(&self, _arm: Arm) -> Result<ScenarioInstance, BenchError> {
        let orders = fixture();

        let list_orders_data = orders.clone();
        let list_orders = FnTool::new(
            "list_orders",
            "List orders for a month (YYYY-MM). Returns id and customer_id only — amounts and statuses require get_order. 10 per page.",
            json!({
                "type": "object",
                "properties": {
                    "month": { "type": "string", "description": "YYYY-MM" },
                    "page": { "type": "integer", "minimum": 1, "default": 1 }
                },
                "required": ["month"],
                "additionalProperties": false
            }),
            page_schema(json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "customer_id": { "type": "string" }
                }
            })),
            move |input| {
                let month = get_str(input, "month")?;
                let page = get_u64(input, "page", 1);
                let items: Vec<Value> = list_orders_data
                    .iter()
                    .filter(|o| o.month == month)
                    .map(|o| json!({ "id": o.id, "customer_id": o.customer_id }))
                    .collect();
                Ok(paginate(items, page, 10))
            },
        )
        .read_only()
        // First call per run fails with a transient 503 (see FnTool::flaky).
        .flaky(1);

        let get_order_data = orders.clone();
        let get_order = FnTool::new(
            "get_order",
            "Fetch one order: status (completed | refunded | pending) and amount in cents.",
            json!({
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"],
                "additionalProperties": false
            }),
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "customer_id": { "type": "string" },
                    "status": { "type": "string" },
                    "amount_cents": { "type": "integer" },
                    "currency": { "type": "string" }
                }
            }),
            move |input| {
                let id = get_str(input, "id")?;
                let order = get_order_data
                    .iter()
                    .find(|o| o.id == id)
                    .ok_or_else(|| format!("no order with id {id}"))?;
                Ok(json!({
                    "id": order.id,
                    "customer_id": order.customer_id,
                    "status": order.status,
                    "amount_cents": order.amount_cents,
                    "currency": "USD",
                }))
            },
        )
        .read_only();

        let get_customer = FnTool::new(
            "get_customer",
            "Fetch one customer: name and billing region (NA | EU | APAC).",
            json!({
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"],
                "additionalProperties": false
            }),
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "name": { "type": "string" },
                    "region": { "type": "string", "enum": ["NA", "EU", "APAC"] }
                }
            }),
            move |input| {
                let id = get_str(input, "id")?;
                let index: u64 = id
                    .strip_prefix("C-")
                    .and_then(|v| v.parse().ok())
                    .filter(|v| (1..=18).contains(v))
                    .ok_or_else(|| format!("no customer with id {id}"))?;
                Ok(json!({
                    "id": id,
                    "name": format!("Customer {index:02}"),
                    "region": customer_region(index),
                }))
            },
        )
        .read_only();

        let (submit, submission) = submit_result_tool(json!({
            "type": "object",
            "properties": {
                "NA": { "type": "integer", "description": "total completed revenue in cents" },
                "EU": { "type": "integer", "description": "total completed revenue in cents" },
                "APAC": { "type": "integer", "description": "total completed revenue in cents" }
            },
            "required": ["NA", "EU", "APAC"]
        }));

        let tools = ToolRegistry::new()
            .with(list_orders)
            .with(get_order)
            .with(get_customer)
            .with(submit);

        let user_prompt = "Compute total revenue for March 2026 (month \"2026-03\") per customer \
             billing region, counting ONLY orders with status \"completed\" (exclude refunded and \
             pending). Report each region's total in integer cents. When done, submit \
             {\"NA\": <cents>, \"EU\": <cents>, \"APAC\": <cents>} via submit_result."
            .to_string();

        let scorer_submission = submission.clone();
        let scorer = Box::new(move || {
            let expected = expected_totals();
            let submitted = scorer_submission.lock().expect("submission lock").clone();
            let mut hits = 0usize;
            let mut notes = vec![format!("expected={expected:?}")];
            for region in REGIONS {
                let got = submitted
                    .as_ref()
                    .and_then(|v| v.get(region))
                    .and_then(Value::as_u64);
                if got == Some(expected[region]) {
                    hits += 1;
                } else {
                    notes.push(format!(
                        "{region}: expected {} got {got:?}",
                        expected[region]
                    ));
                }
            }
            Score {
                accuracy: hits as f64 / REGIONS.len() as f64,
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
    fn totals_are_nonzero_in_every_region() {
        let totals = expected_totals();
        for region in REGIONS {
            assert!(totals[region] > 0, "{region} should have revenue");
        }
        // Noise months and non-completed statuses exist so naive sums are wrong.
        assert!(fixture().iter().any(|o| o.month != "2026-03"));
        assert!(fixture().iter().any(|o| o.status == "refunded"));
        assert!(fixture().iter().any(|o| o.status == "pending"));
    }
}
