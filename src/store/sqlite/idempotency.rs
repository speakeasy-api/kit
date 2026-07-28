use std::fmt;

use rusqlite::{OptionalExtension, Transaction, params};

use crate::domain::events::{CommitPosition, EntityId};
use crate::domain::ids::PrincipalId;

pub const REQUEST_DIGEST_BYTES: usize = 32;
const CLAIM_TOKEN_BYTES: usize = 16;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct IdempotencyScope {
    principal_id: PrincipalId,
    command: String,
    target: EntityId,
}

impl IdempotencyScope {
    pub fn new(
        principal_id: PrincipalId,
        command: impl Into<String>,
        target: EntityId,
    ) -> Result<Self, IdempotencyScopeError> {
        let command = command.into();
        if command.is_empty()
            || command.len() > 255
            || command.bytes().any(|byte| !byte.is_ascii_graphic())
        {
            return Err(IdempotencyScopeError);
        }
        Ok(Self {
            principal_id,
            command,
            target,
        })
    }

    pub const fn principal_id(&self) -> PrincipalId {
        self.principal_id
    }

    pub fn command(&self) -> &str {
        &self.command
    }

    pub const fn target(&self) -> EntityId {
        self.target
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdempotencyScopeError;

impl fmt::Display for IdempotencyScopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("idempotency command scope must contain 1 to 255 visible ASCII bytes")
    }
}

impl std::error::Error for IdempotencyScopeError {}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct CanonicalRequestDigest([u8; REQUEST_DIGEST_BYTES]);

impl CanonicalRequestDigest {
    pub const fn new(bytes: [u8; REQUEST_DIGEST_BYTES]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; REQUEST_DIGEST_BYTES] {
        &self.0
    }
}

impl fmt::Debug for CanonicalRequestDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CanonicalRequestDigest([redacted])")
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    pub fn parse(value: &str) -> Result<Self, IdempotencyKeyError> {
        if value.is_empty()
            || value.len() > 255
            || value.bytes().any(|byte| !byte.is_ascii_graphic())
        {
            Err(IdempotencyKeyError)
        } else {
            Ok(Self(value.to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for IdempotencyKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdempotencyKeyError;

impl fmt::Display for IdempotencyKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("idempotency key must contain 1 to 255 visible ASCII bytes")
    }
}

impl std::error::Error for IdempotencyKeyError {}

#[derive(Clone, Eq, PartialEq)]
pub struct IdempotencyClaim {
    pub(crate) scope: IdempotencyScope,
    pub(crate) key: IdempotencyKey,
    pub(crate) digest: CanonicalRequestDigest,
    pub(crate) token: PendingToken,
}

impl IdempotencyClaim {
    pub fn scope(&self) -> &IdempotencyScope {
        &self.scope
    }

    pub fn key(&self) -> &IdempotencyKey {
        &self.key
    }

    pub const fn request_digest(&self) -> CanonicalRequestDigest {
        self.digest
    }
}

impl fmt::Debug for IdempotencyClaim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IdempotencyClaim")
            .field("scope", &self.scope)
            .field("key", &self.key)
            .field("request_digest", &self.digest)
            .field("token", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdempotentResponse {
    pub response: Vec<u8>,
    pub commit_positions: Vec<CommitPosition>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClaimOutcome {
    Claimed(IdempotencyClaim),
    Replay(IdempotentResponse),
    Pending,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IdempotencyStatus {
    Missing,
    Pending {
        request_digest: CanonicalRequestDigest,
    },
    Terminal {
        request_digest: CanonicalRequestDigest,
        result: IdempotentResponse,
    },
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct PendingToken([u8; CLAIM_TOKEN_BYTES]);

impl PendingToken {
    pub(crate) fn generate() -> Result<Self, getrandom::Error> {
        let mut bytes = [0; CLAIM_TOKEN_BYTES];
        getrandom::fill(&mut bytes)?;
        Ok(Self(bytes))
    }

    pub(crate) fn from_bytes(bytes: &[u8]) -> Option<Self> {
        bytes.try_into().ok().map(Self)
    }

    pub(crate) fn as_bytes(&self) -> &[u8; CLAIM_TOKEN_BYTES] {
        &self.0
    }
}

pub(crate) enum StoredState {
    Pending(PendingToken),
    Terminal(Vec<u8>),
    Invalid,
}

pub(crate) struct StoredRecord {
    pub digest: Vec<u8>,
    pub state: StoredState,
}

pub(crate) fn lookup(
    transaction: &Transaction<'_>,
    scope: &IdempotencyScope,
    key: &IdempotencyKey,
) -> rusqlite::Result<Option<StoredRecord>> {
    transaction
        .query_row(
            "SELECT request_digest, state, claim_token, response
             FROM idempotency
             WHERE principal_id = ?1 AND command_name = ?2 AND target = ?3
               AND idempotency_key = ?4",
            params![
                scope.principal_id.to_string(),
                scope.command,
                scope.target.to_string(),
                key.as_str()
            ],
            |row| {
                let digest = row.get(0)?;
                let state: String = row.get(1)?;
                let token: Option<Vec<u8>> = row.get(2)?;
                let response: Option<Vec<u8>> = row.get(3)?;
                let state = match (state.as_str(), token, response) {
                    ("pending", Some(token), None) => PendingToken::from_bytes(&token)
                        .map(StoredState::Pending)
                        .unwrap_or(StoredState::Invalid),
                    ("terminal", None, Some(response)) => StoredState::Terminal(response),
                    _ => StoredState::Invalid,
                };
                Ok(StoredRecord { digest, state })
            },
        )
        .optional()
}

pub(crate) fn insert_pending(
    transaction: &Transaction<'_>,
    scope: &IdempotencyScope,
    key: &IdempotencyKey,
    digest: CanonicalRequestDigest,
    token: &PendingToken,
) -> rusqlite::Result<()> {
    transaction.execute(
        "INSERT INTO idempotency (
             principal_id, command_name, target, idempotency_key,
             request_digest, state, claim_token, response
         ) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, NULL)",
        params![
            scope.principal_id.to_string(),
            scope.command,
            scope.target.to_string(),
            key.as_str(),
            digest.as_bytes().as_slice(),
            token.as_bytes().as_slice()
        ],
    )?;
    Ok(())
}

pub(crate) fn set_terminal(
    transaction: &Transaction<'_>,
    scope: &IdempotencyScope,
    key: &IdempotencyKey,
    response: &[u8],
) -> rusqlite::Result<bool> {
    Ok(transaction.execute(
        "UPDATE idempotency
         SET state = 'terminal', claim_token = NULL, response = ?5
         WHERE principal_id = ?1 AND command_name = ?2 AND target = ?3
           AND idempotency_key = ?4 AND state = 'pending'",
        params![
            scope.principal_id.to_string(),
            scope.command,
            scope.target.to_string(),
            key.as_str(),
            response
        ],
    )? == 1)
}

pub(crate) fn insert_position(
    transaction: &Transaction<'_>,
    scope: &IdempotencyScope,
    key: &IdempotencyKey,
    ordinal: usize,
    commit_position: i64,
) -> rusqlite::Result<()> {
    transaction.execute(
        "INSERT INTO idempotency_events (
             principal_id, command_name, target, idempotency_key, ordinal, commit_position
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            scope.principal_id.to_string(),
            scope.command,
            scope.target.to_string(),
            key.as_str(),
            ordinal as i64,
            commit_position
        ],
    )?;
    Ok(())
}

pub(crate) fn positions(
    transaction: &Transaction<'_>,
    scope: &IdempotencyScope,
    key: &IdempotencyKey,
) -> rusqlite::Result<Vec<i64>> {
    let mut statement = transaction.prepare(
        "SELECT commit_position FROM idempotency_events
         WHERE principal_id = ?1 AND command_name = ?2 AND target = ?3
           AND idempotency_key = ?4
         ORDER BY ordinal",
    )?;
    statement
        .query_map(
            params![
                scope.principal_id.to_string(),
                scope.command,
                scope.target.to_string(),
                key.as_str()
            ],
            |row| row.get(0),
        )?
        .collect()
}
