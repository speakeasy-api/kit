use std::{fs::File, io, io::Read, path::PathBuf};

use reqwest::Method;
use serde::Serialize;
use serde_json::Value;

use crate::{
    api::http::exec::{
        AllocateTerminalBody, EXEC_ROUTES, EmptyBody, TerminalInputResolution,
        TerminalInputResolutionBody, TerminalResizeBody, WriterLeaseBody,
    },
    domain::ids::{ProcessId, ProjectId, TerminalId},
    store::sqlite::idempotency::IdempotencyKey,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecCliOperation {
    pub command: &'static str,
    pub service_operation: &'static str,
    pub openapi_operation_id: &'static str,
    pub mutation: bool,
}

pub const EXEC_CLI_OPERATIONS: &[ExecCliOperation] = &[
    operation("process list", "process.list", "listProcesses", false),
    operation("process show", "process.get", "getProcess", false),
    operation("process cancel", "process.cancel", "cancelProcess", true),
    operation(
        "terminal allocate",
        "terminal.allocate",
        "allocateTerminal",
        true,
    ),
    operation("terminal show", "terminal.get", "getTerminal", false),
    operation(
        "terminal attach",
        "terminal.viewer.attach",
        "attachTerminalViewer",
        true,
    ),
    operation(
        "terminal writer-claim",
        "terminal.writer.claim",
        "claimTerminalWriter",
        true,
    ),
    operation(
        "terminal attachment-show",
        "terminal.attachment.get",
        "getTerminalAttachment",
        false,
    ),
    operation(
        "terminal writer-renew",
        "terminal.writer.renew",
        "renewTerminalWriter",
        true,
    ),
    operation(
        "terminal writer-release",
        "terminal.writer.release",
        "releaseTerminalWriter",
        true,
    ),
    operation(
        "terminal input",
        "terminal.input",
        "writeTerminalInput",
        true,
    ),
    operation(
        "terminal input-resolve",
        "terminal.input.resolve",
        "resolveTerminalInput",
        true,
    ),
    operation("terminal resize", "terminal.resize", "resizeTerminal", true),
    operation(
        "terminal output",
        "terminal.output",
        "readTerminalOutput",
        false,
    ),
    operation(
        "terminal resizes",
        "terminal.resizes",
        "readTerminalResizes",
        false,
    ),
    operation("terminal detach", "terminal.detach", "detachTerminal", true),
    operation(
        "executor events",
        "executor.events",
        "listExecutorEvents",
        false,
    ),
];

const fn operation(
    command: &'static str,
    service_operation: &'static str,
    openapi_operation_id: &'static str,
    mutation: bool,
) -> ExecCliOperation {
    ExecCliOperation {
        command,
        service_operation,
        openapi_operation_id,
        mutation,
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ExecRequest {
    pub operation: &'static str,
    pub method: Method,
    pub path: String,
    body: Option<ExecBody>,
    pub idempotency_key: Option<IdempotencyKey>,
}

#[derive(Clone, Eq, PartialEq)]
enum ExecBody {
    Json(Value),
    SecretInput(SecretInput),
    InputSource(InputSource),
}

pub const MAX_TERMINAL_INPUT_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputSource {
    Stdin,
    File(PathBuf),
}

#[derive(Clone, Eq, PartialEq)]
struct SecretInput(Vec<u8>);

impl Drop for SecretInput {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

impl std::fmt::Debug for ExecRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExecRequest")
            .field("operation", &self.operation)
            .field("method", &self.method)
            .field("path", &self.path)
            .field(
                "body",
                &if matches!(self.body, Some(ExecBody::SecretInput(_))) {
                    Some("[REDACTED]")
                } else if matches!(self.body, Some(ExecBody::InputSource(_))) {
                    Some("input source")
                } else if self.body.is_some() {
                    Some("present")
                } else {
                    None
                },
            )
            .field("idempotency_key", &self.idempotency_key)
            .finish()
    }
}

impl ExecRequest {
    pub fn list_processes(project_id: ProjectId) -> Self {
        query(
            "process.list",
            format!("/v1/projects/{project_id}/processes"),
        )
    }
    pub fn get_process(process_id: ProcessId) -> Self {
        query("process.get", format!("/v1/processes/{process_id}"))
    }
    pub fn cancel_process(process_id: ProcessId, key: IdempotencyKey) -> Self {
        mutation(
            "process.cancel",
            format!("/v1/processes/{process_id}/cancel"),
            &EmptyBody {},
            key,
        )
    }
    pub fn allocate_terminal(
        process_id: ProcessId,
        body: AllocateTerminalBody,
        key: IdempotencyKey,
    ) -> Self {
        mutation(
            "terminal.allocate",
            format!("/v1/processes/{process_id}/terminals"),
            &body,
            key,
        )
    }
    pub fn get_terminal(terminal_id: TerminalId) -> Self {
        query("terminal.get", format!("/v1/terminals/{terminal_id}"))
    }
    pub fn attach_viewer(terminal_id: TerminalId, key: IdempotencyKey) -> Self {
        mutation(
            "terminal.viewer.attach",
            format!("/v1/terminals/{terminal_id}/attachments"),
            &EmptyBody {},
            key,
        )
    }
    pub fn claim_writer(terminal_id: TerminalId, lease_millis: u64, key: IdempotencyKey) -> Self {
        mutation(
            "terminal.writer.claim",
            format!("/v1/terminals/{terminal_id}/writer-claims"),
            &WriterLeaseBody { lease_millis },
            key,
        )
    }
    pub fn get_attachment(attachment_id: &str) -> Self {
        query(
            "terminal.attachment.get",
            format!("/v1/terminal-attachments/{attachment_id}"),
        )
    }
    pub fn renew_writer(attachment_id: &str, lease_millis: u64, key: IdempotencyKey) -> Self {
        mutation(
            "terminal.writer.renew",
            format!("/v1/terminal-attachments/{attachment_id}/renew"),
            &WriterLeaseBody { lease_millis },
            key,
        )
    }
    pub fn release_writer(attachment_id: &str, key: IdempotencyKey) -> Self {
        mutation(
            "terminal.writer.release",
            format!("/v1/terminal-attachments/{attachment_id}/release"),
            &EmptyBody {},
            key,
        )
    }
    pub fn write_input(attachment_id: &str, bytes: &[u8], key: IdempotencyKey) -> Self {
        Self {
            operation: "terminal.input",
            method: Method::POST,
            path: format!("/v1/terminal-attachments/{attachment_id}/input"),
            body: Some(ExecBody::SecretInput(SecretInput(bytes.to_vec()))),
            idempotency_key: Some(key),
        }
    }
    pub fn write_input_from(attachment_id: &str, source: InputSource, key: IdempotencyKey) -> Self {
        Self {
            operation: "terminal.input",
            method: Method::POST,
            path: format!("/v1/terminal-attachments/{attachment_id}/input"),
            body: Some(ExecBody::InputSource(source)),
            idempotency_key: Some(key),
        }
    }
    pub fn resolve_input(
        attachment_id: &str,
        outcome: TerminalInputResolution,
        key: IdempotencyKey,
    ) -> Self {
        mutation(
            "terminal.input.resolve",
            format!("/v1/terminal-attachments/{attachment_id}/input-resolution"),
            &TerminalInputResolutionBody { outcome },
            key,
        )
    }

    pub fn read_input_source(&mut self, stdin: &mut dyn Read) -> io::Result<()> {
        let Some(ExecBody::InputSource(source)) = self.body.take() else {
            return Ok(());
        };
        let mut bytes = Vec::new();
        match source {
            InputSource::Stdin => stdin
                .take((MAX_TERMINAL_INPUT_BYTES + 1) as u64)
                .read_to_end(&mut bytes)?,
            InputSource::File(path) => File::open(path)?
                .take((MAX_TERMINAL_INPUT_BYTES + 1) as u64)
                .read_to_end(&mut bytes)?,
        };
        if bytes.is_empty() || bytes.len() > MAX_TERMINAL_INPUT_BYTES {
            bytes.fill(0);
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "terminal input must contain 1 to 16384 bytes",
            ));
        }
        self.body = Some(ExecBody::SecretInput(SecretInput(bytes)));
        Ok(())
    }
    pub fn resize(attachment_id: &str, columns: u16, rows: u16, key: IdempotencyKey) -> Self {
        mutation(
            "terminal.resize",
            format!("/v1/terminal-attachments/{attachment_id}/resize"),
            &TerminalResizeBody { columns, rows },
            key,
        )
    }
    pub fn read_output(attachment_id: &str, cursor: &str) -> Self {
        query(
            "terminal.output",
            format!("/v1/terminal-attachments/{attachment_id}/output?cursor={cursor}"),
        )
    }
    pub fn read_resizes(attachment_id: &str, cursor: &str) -> Self {
        query(
            "terminal.resizes",
            format!("/v1/terminal-attachments/{attachment_id}/resizes?cursor={cursor}"),
        )
    }
    pub fn detach(attachment_id: &str, key: IdempotencyKey) -> Self {
        mutation(
            "terminal.detach",
            format!("/v1/terminal-attachments/{attachment_id}/detach"),
            &EmptyBody {},
            key,
        )
    }
    pub fn events(project_id: ProjectId, cursor: &str) -> Self {
        query(
            "executor.events",
            format!("/v1/projects/{project_id}/executor/events?cursor={cursor}"),
        )
    }

    pub(crate) fn take_body_bytes(&mut self) -> Result<Option<Vec<u8>>, serde_json::Error> {
        self.body
            .take()
            .map(|body| match body {
                ExecBody::Json(value) => serde_json::to_vec(&value),
                ExecBody::SecretInput(input) => {
                    #[derive(Serialize)]
                    struct WireInput<'a> {
                        bytes: &'a [u8],
                    }
                    serde_json::to_vec(&WireInput { bytes: &input.0 })
                }
                ExecBody::InputSource(_) => {
                    unreachable!("input source must be read before dispatch")
                }
            })
            .transpose()
    }
}

pub trait ExecHttpClient {
    type Error;
    fn execute(&mut self, request: ExecRequest) -> Result<Value, Self::Error>;
}

pub fn execute<C: ExecHttpClient>(client: &mut C, request: ExecRequest) -> Result<Value, C::Error> {
    client.execute(request)
}

pub fn parity_table() -> String {
    let routes = EXEC_ROUTES
        .iter()
        .map(|route| route.operation)
        .collect::<std::collections::BTreeSet<_>>();
    let commands = EXEC_CLI_OPERATIONS
        .iter()
        .map(|operation| operation.service_operation)
        .collect::<std::collections::BTreeSet<_>>();
    let uncovered = routes
        .symmetric_difference(&commands)
        .copied()
        .collect::<Vec<_>>();
    format!(
        "executor API/CLI parity: routes={} commands={} uncovered={} {:?}",
        routes.len(),
        commands.len(),
        uncovered.len(),
        uncovered
    )
}

fn query(operation: &'static str, path: String) -> ExecRequest {
    ExecRequest {
        operation,
        method: Method::GET,
        path,
        body: None,
        idempotency_key: None,
    }
}

fn mutation(
    operation: &'static str,
    path: String,
    body: &impl Serialize,
    idempotency_key: IdempotencyKey,
) -> ExecRequest {
    ExecRequest {
        operation,
        method: Method::POST,
        path,
        body: Some(ExecBody::Json(
            serde_json::to_value(body).expect("request body serializes"),
        )),
        idempotency_key: Some(idempotency_key),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Cursor};

    use super::*;

    const ATTACHMENT: &str = "attachment_00000000000000000000000000000000";

    #[test]
    fn binary_stdin_and_file_are_materialized_only_after_parsing() {
        let binary = [0, 0xff, b'\n'];
        let key = || IdempotencyKey::parse("binary-input").unwrap();
        let mut stdin = ExecRequest::write_input_from(ATTACHMENT, InputSource::Stdin, key());
        assert!(format!("{stdin:?}").contains("input source"));
        stdin.read_input_source(&mut Cursor::new(binary)).unwrap();
        let body: Value =
            serde_json::from_slice(&stdin.take_body_bytes().unwrap().unwrap()).unwrap();
        assert_eq!(body["bytes"], serde_json::json!(binary));

        let path = std::env::temp_dir().join(format!("kit-terminal-input-{}", std::process::id()));
        fs::write(&path, binary).unwrap();
        let mut file =
            ExecRequest::write_input_from(ATTACHMENT, InputSource::File(path.clone()), key());
        file.read_input_source(&mut Cursor::new([])).unwrap();
        let body: Value =
            serde_json::from_slice(&file.take_body_bytes().unwrap().unwrap()).unwrap();
        assert_eq!(body["bytes"], serde_json::json!(binary));
        fs::remove_file(path).unwrap();
    }
}
