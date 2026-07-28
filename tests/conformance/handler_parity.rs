use std::collections::BTreeSet;

use kit::api::service::{AuthorityPath, OperationKind, handlers};

const COMMAND_COUNT: usize = 16;
const QUERY_COUNT: usize = 19;

#[test]
fn handler_parity() {
    let descriptors = handlers();
    let command_count = descriptors
        .iter()
        .filter(|descriptor| descriptor.kind == OperationKind::Command)
        .count();
    let query_count = descriptors
        .iter()
        .filter(|descriptor| descriptor.kind == OperationKind::Query)
        .count();
    let operations = descriptors
        .iter()
        .map(|descriptor| descriptor.operation)
        .collect::<BTreeSet<_>>();
    let registered_handlers = descriptors
        .iter()
        .map(|descriptor| descriptor.handler)
        .collect::<BTreeSet<_>>();

    assert_eq!(command_count, COMMAND_COUNT, "{}", parity_table());
    assert_eq!(query_count, QUERY_COUNT, "{}", parity_table());
    assert_eq!(operations.len(), descriptors.len(), "{}", parity_table());
    assert_eq!(
        registered_handlers.len(),
        descriptors.len(),
        "{}",
        parity_table()
    );
    assert!(
        descriptors.iter().all(|descriptor| {
            descriptor.authority == AuthorityPath::Service && descriptor.authority_path_count == 1
        }),
        "{}",
        parity_table()
    );
}

fn parity_table() -> String {
    let mut table = String::from("kind | operation | handler | authority paths\n");
    for descriptor in handlers() {
        table.push_str(&format!(
            "{:?} | {} | {} | {}\n",
            descriptor.kind,
            descriptor.operation,
            descriptor.handler,
            descriptor.authority_path_count,
        ));
    }
    table
}
