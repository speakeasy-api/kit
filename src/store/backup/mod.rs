use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};

use crate::api::service::SqliteServiceStore;
use crate::domain::crypto::{sha256, sha256_domain};
use crate::domain::retention::{RetentionPeriod, RetentionPolicy};
use crate::store::artifacts::{
    ArtifactClass, ArtifactId, ArtifactReference, ArtifactRetention, ArtifactStore,
};
use crate::store::sqlite::append::SqliteStore;
use crate::store::sqlite::projection::ProjectionStore;

const MANIFEST_VERSION: u32 = 2;
const HEALTH_VERSION: u32 = 1;
const DATABASE_FILE: &str = "store.sqlite3";
const MANIFEST_FILE: &str = "manifest.json";
const HEALTH_FILE: &str = "health.json";
const GENERATION_PREFIX: &str = "generation-";
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ArtifactPolicy {
    pub reachable: bool,
    pub shared_reference: bool,
    pub legal_hold: bool,
    pub deletion_requested_at_unix_micros: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct BackupConfig {
    pub state_root: PathBuf,
    pub database_path: PathBuf,
    pub artifact_root: PathBuf,
    pub destination: PathBuf,
    pub retain_generations: usize,
    pub backup_expires_at_unix_micros: i64,
    pub build_version: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupGeneration {
    pub name: String,
    pub path: PathBuf,
    pub created_at_unix_micros: i64,
    pub expires_at_unix_micros: i64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BackupHealth {
    pub last_success: Option<BackupSuccess>,
    pub last_failure: Option<BackupFailure>,
    pub age_micros: Option<u64>,
    pub current_generation: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupSuccess {
    pub generation: String,
    pub at_unix_micros: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupFailure {
    pub at_unix_micros: i64,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RestoreReport {
    pub generation: String,
    pub state_root: PathBuf,
    pub commit_watermark: u64,
    pub artifact_count: usize,
}

#[derive(Debug)]
pub enum BackupError {
    Io(std::io::Error),
    Database(rusqlite::Error),
    Json(serde_json::Error),
    InvalidConfiguration(&'static str),
    UnsafePath(PathBuf),
    DestinationInsideStateRoot,
    MissingArtifactPolicy(ArtifactId),
    MissingArtifact(ArtifactId),
    CorruptManifest(&'static str),
    DigestMismatch(&'static str),
    IntegrityCheck(String),
    SchemaMismatch,
    BuildMismatch,
    SemanticMismatch(&'static str),
    BackupExpired(i64),
    GenerationNotFound(String),
    RestoreTargetExists(PathBuf),
    Inventory(String),
}

impl fmt::Display for BackupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "backup I/O error: {error}"),
            Self::Database(error) => write!(f, "backup SQLite error: {error}"),
            Self::Json(error) => write!(f, "backup JSON error: {error}"),
            Self::InvalidConfiguration(message) => {
                write!(f, "invalid backup configuration: {message}")
            }
            Self::UnsafePath(path) => write!(f, "unsafe backup path: {}", path.display()),
            Self::DestinationInsideStateRoot => {
                f.write_str("backup destination must not overlap the active state root")
            }
            Self::MissingArtifactPolicy(id) => {
                write!(f, "backup policy metadata is missing for artifact {id}")
            }
            Self::MissingArtifact(id) => write!(f, "backup artifact {id} is incomplete"),
            Self::CorruptManifest(message) => write!(f, "corrupt backup manifest: {message}"),
            Self::DigestMismatch(name) => write!(f, "backup {name} digest mismatch"),
            Self::IntegrityCheck(message) => {
                write!(f, "restored SQLite integrity check failed: {message}")
            }
            Self::SchemaMismatch => f.write_str("backup schema or migration version mismatch"),
            Self::BuildMismatch => f.write_str("backup build version mismatch"),
            Self::SemanticMismatch(name) => {
                write!(f, "restored {name} does not match the backup manifest")
            }
            Self::BackupExpired(expiry) => {
                write!(f, "backup expired at Unix microsecond {expiry}")
            }
            Self::GenerationNotFound(name) => write!(f, "backup generation {name} was not found"),
            Self::RestoreTargetExists(path) => {
                write!(f, "restore target already exists: {}", path.display())
            }
            Self::Inventory(message) => write!(f, "backup inventory error: {message}"),
        }
    }
}

impl std::error::Error for BackupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Database(error) => Some(error),
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for BackupError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for BackupError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

impl From<serde_json::Error> for BackupError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct Manifest {
    format_version: u32,
    generation: String,
    created_at_unix_micros: i64,
    backup_expires_at_unix_micros: i64,
    build_version: String,
    database_relative_path: String,
    artifact_relative_path: String,
    database_sha256: String,
    schema_version: i64,
    migration_version: i64,
    commit_watermark: u64,
    event_sha256: String,
    projection_sha256: String,
    artifacts: Vec<ManifestArtifact>,
    references: Vec<ManifestReference>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ManifestArtifact {
    id: String,
    size: u64,
    object_sha256: String,
    manifest_sha256: String,
    media_type: String,
    class: String,
    principal: String,
    project: String,
    retention: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    policy_retention: Option<String>,
    stored_at_unix_micros: i64,
    reachable: bool,
    shared_reference: bool,
    legal_hold: bool,
    deletion_requested_at_unix_micros: Option<i64>,
    backup_expires_at_unix_micros: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ManifestReference {
    reference: String,
    artifact: String,
    size: u64,
    manifest_sha256: String,
    media_type: String,
    class: String,
    principal: String,
    project: String,
    retention: String,
    stored_at_unix_micros: i64,
}

#[derive(Clone, Debug)]
struct SnapshotArtifactPolicy {
    policy: ArtifactPolicy,
    retention: RetentionPeriod,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct HealthRecord {
    format_version: u32,
    last_success: Option<HealthSuccess>,
    last_failure: Option<HealthFailure>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct HealthSuccess {
    generation: String,
    at_unix_micros: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct HealthFailure {
    at_unix_micros: i64,
    message: String,
}

pub struct BackupManager {
    config: BackupConfig,
    state_root: PathBuf,
    database_path: PathBuf,
    artifact_root: PathBuf,
    destination: PathBuf,
    database_relative_path: PathBuf,
    artifact_relative_path: PathBuf,
    health: HealthRecord,
    current_generation: Option<String>,
}

impl BackupManager {
    pub(crate) fn open(config: BackupConfig) -> Result<Self, BackupError> {
        if config.retain_generations == 0 {
            return Err(BackupError::InvalidConfiguration(
                "retain_generations must be greater than zero",
            ));
        }
        if config.build_version.is_empty() {
            return Err(BackupError::InvalidConfiguration(
                "build_version must not be empty",
            ));
        }
        let state_root = canonical_directory(&config.state_root)?;
        let database_path = canonical_file(&config.database_path)?;
        let artifact_root = canonical_directory(&config.artifact_root)?;
        if !database_path.starts_with(&state_root) || !artifact_root.starts_with(&state_root) {
            return Err(BackupError::InvalidConfiguration(
                "database and artifact roots must be inside the active state root",
            ));
        }
        ensure_directory(&config.destination)?;
        let destination = canonical_directory(&config.destination)?;
        if destination.starts_with(&state_root) || state_root.starts_with(&destination) {
            return Err(BackupError::DestinationInsideStateRoot);
        }
        let database_relative_path = database_path
            .strip_prefix(&state_root)
            .expect("validated database path")
            .to_owned();
        let artifact_relative_path = artifact_root
            .strip_prefix(&state_root)
            .expect("validated artifact path")
            .to_owned();
        validate_relative(&database_relative_path)?;
        validate_relative(&artifact_relative_path)?;
        let health = load_health(&destination)?;
        let current_generation = list_generations(&destination)?
            .last()
            .map(|generation| generation.name.clone());
        Ok(Self {
            config,
            state_root,
            database_path,
            artifact_root,
            destination,
            database_relative_path,
            artifact_relative_path,
            health,
            current_generation,
        })
    }

    #[cfg(any(test, debug_assertions))]
    pub(crate) fn create_backup(
        &mut self,
        now_unix_micros: i64,
        inventory: &mut SqliteServiceStore,
    ) -> Result<BackupGeneration, BackupError> {
        self.create_backup_until(
            now_unix_micros,
            self.config.backup_expires_at_unix_micros,
            inventory,
        )
    }

    pub(crate) fn create_backup_until(
        &mut self,
        now_unix_micros: i64,
        expires_at_unix_micros: i64,
        inventory: &mut SqliteServiceStore,
    ) -> Result<BackupGeneration, BackupError> {
        let result = self.create_backup_inner(now_unix_micros, expires_at_unix_micros, inventory);
        match result {
            Ok(generation) => {
                self.health.last_success = Some(HealthSuccess {
                    generation: generation.name.clone(),
                    at_unix_micros: now_unix_micros,
                });
                self.current_generation = Some(generation.name.clone());
                self.persist_health()?;
                Ok(generation)
            }
            Err(error) => {
                self.record_failure(now_unix_micros, error.to_string());
                Err(error)
            }
        }
    }

    pub(crate) fn reconcile_generations(
        &mut self,
        now_unix_micros: i64,
    ) -> Result<Vec<BackupGeneration>, BackupError> {
        self.expire_generations(now_unix_micros)?;
        for generation in list_generations(&self.destination)? {
            let verification_root = self
                .destination
                .join(format!(".verify-startup-{}", generation.name));
            let verification = restore_generation(
                &generation.path,
                &verification_root,
                now_unix_micros,
                &self.config.build_version,
            );
            let _ = fs::remove_dir_all(&verification_root);
            verification?;
        }
        let generations = list_generations(&self.destination)?;
        self.current_generation = generations.last().map(|generation| generation.name.clone());
        Ok(generations)
    }

    pub(crate) fn expire_generations(
        &mut self,
        now_unix_micros: i64,
    ) -> Result<usize, BackupError> {
        let mut removed = 0;
        for generation in list_generations(&self.destination)? {
            if generation.expires_at_unix_micros <= now_unix_micros {
                fs::remove_dir_all(generation.path)?;
                removed += 1;
            }
        }
        if removed != 0 {
            sync_directory(&self.destination)?;
        }
        self.current_generation = list_generations(&self.destination)?
            .last()
            .map(|generation| generation.name.clone());
        Ok(removed)
    }

    pub(crate) fn record_failure(&mut self, now_unix_micros: i64, message: String) {
        self.health.last_failure = Some(HealthFailure {
            at_unix_micros: now_unix_micros,
            message,
        });
        let _ = self.persist_health();
    }

    pub fn restore(
        &self,
        generation: &str,
        fresh_state_root: impl AsRef<Path>,
        now_unix_micros: i64,
        expected_build_version: &str,
    ) -> Result<RestoreReport, BackupError> {
        validate_generation_name(generation)?;
        let path = self.destination.join(generation);
        if !is_directory(&path)? {
            return Err(BackupError::GenerationNotFound(generation.to_owned()));
        }
        restore_generation(
            &path,
            fresh_state_root.as_ref(),
            now_unix_micros,
            expected_build_version,
        )
    }

    pub fn generations(&self) -> Result<Vec<BackupGeneration>, BackupError> {
        list_generations(&self.destination)
    }

    pub fn health(&self, now_unix_micros: i64) -> BackupHealth {
        let last_success = self
            .health
            .last_success
            .as_ref()
            .map(|success| BackupSuccess {
                generation: success.generation.clone(),
                at_unix_micros: success.at_unix_micros,
            });
        let age_micros = last_success.as_ref().map(|success| {
            now_unix_micros
                .saturating_sub(success.at_unix_micros)
                .try_into()
                .unwrap_or(0)
        });
        BackupHealth {
            last_success,
            last_failure: self
                .health
                .last_failure
                .as_ref()
                .map(|failure| BackupFailure {
                    at_unix_micros: failure.at_unix_micros,
                    message: failure.message.clone(),
                }),
            age_micros,
            current_generation: self.current_generation.clone(),
        }
    }

    fn create_backup_inner(
        &mut self,
        now_unix_micros: i64,
        expires_at_unix_micros: i64,
        inventory: &mut SqliteServiceStore,
    ) -> Result<BackupGeneration, BackupError> {
        if now_unix_micros < 0 || expires_at_unix_micros <= now_unix_micros {
            return Err(BackupError::InvalidConfiguration(
                "backup expiry must be after its creation time",
            ));
        }
        if canonical_directory(&self.state_root)? != self.state_root
            || canonical_file(&self.database_path)? != self.database_path
            || canonical_directory(&self.artifact_root)? != self.artifact_root
        {
            return Err(BackupError::UnsafePath(self.state_root.clone()));
        }

        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let name = format!(
            "{GENERATION_PREFIX}{now_unix_micros:020}-{}-{sequence:020}",
            std::process::id()
        );
        let temp_root = self.destination.join(format!(".{name}.tmp"));
        let temp = temp_root.join(&name);
        let published = self.destination.join(&name);
        fs::create_dir(&temp_root)?;
        fs::create_dir(&temp)?;
        let result = self.build_generation(&temp, &name, now_unix_micros, expires_at_unix_micros);
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&temp_root);
            return Err(error);
        }
        sync_directory(&temp)?;

        let verification_root = self.destination.join(format!(".verify-{name}"));
        let verification = restore_generation(
            &temp,
            &verification_root,
            now_unix_micros,
            &self.config.build_version,
        );
        let _ = fs::remove_dir_all(&verification_root);
        if let Err(error) = verification {
            fs::remove_dir_all(&temp_root)?;
            return Err(error);
        }
        let pending = BackupGeneration {
            name: name.clone(),
            path: temp.clone(),
            created_at_unix_micros: now_unix_micros,
            expires_at_unix_micros,
        };
        if let Err(error) = inventory.register_backup_generation(&pending) {
            fs::remove_dir_all(&temp_root)?;
            return Err(BackupError::Inventory(error.to_string()));
        }
        if let Err(error) = fs::rename(&temp, &published) {
            let _ = inventory.unregister_backup_generation(&name);
            let _ = fs::remove_dir_all(&temp_root);
            return Err(BackupError::Io(error));
        }
        let _ = fs::remove_dir(&temp_root);
        sync_directory(&self.destination)?;
        let generation = BackupGeneration {
            name,
            path: published,
            created_at_unix_micros: now_unix_micros,
            expires_at_unix_micros,
        };
        self.prune_generations()?;
        Ok(generation)
    }

    fn build_generation(
        &self,
        temp: &Path,
        generation: &str,
        now_unix_micros: i64,
        expires_at_unix_micros: i64,
    ) -> Result<(), BackupError> {
        let database_backup = temp.join(DATABASE_FILE);
        let source = Connection::open_with_flags(
            &self.database_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        source.busy_timeout(std::time::Duration::from_secs(5))?;
        source.execute("VACUUM INTO ?1", params![database_backup.to_string_lossy()])?;
        sync_file(&database_backup)?;

        let database_bytes = read_regular_file(&database_backup)?;
        let snapshot = Connection::open_with_flags(
            &database_backup,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let database_state = database_state(&snapshot)?;
        let artifact_dir = temp.join("artifacts");
        fs::create_dir(&artifact_dir)?;
        let policies = snapshot_artifact_policies(&snapshot, now_unix_micros)?;
        let (artifacts, references) =
            self.backup_artifacts(&artifact_dir, expires_at_unix_micros, &policies)?;
        sync_directory(&artifact_dir)?;

        let manifest = Manifest {
            format_version: MANIFEST_VERSION,
            generation: generation.to_owned(),
            created_at_unix_micros: now_unix_micros,
            backup_expires_at_unix_micros: expires_at_unix_micros,
            build_version: self.config.build_version.clone(),
            database_relative_path: path_text(&self.database_relative_path)?,
            artifact_relative_path: path_text(&self.artifact_relative_path)?,
            database_sha256: hex(&sha256(&database_bytes)),
            schema_version: database_state.schema_version,
            migration_version: database_state.migration_version,
            commit_watermark: database_state.commit_watermark,
            event_sha256: hex(&database_state.event_digest),
            projection_sha256: hex(&database_state.projection_digest),
            artifacts,
            references,
        };
        write_new_synced(&temp.join(MANIFEST_FILE), &canonical_json(&manifest)?)?;
        Ok(())
    }

    fn backup_artifacts(
        &self,
        destination: &Path,
        expires_at_unix_micros: i64,
        policies: &BTreeMap<ArtifactId, SnapshotArtifactPolicy>,
    ) -> Result<(Vec<ManifestArtifact>, Vec<ManifestReference>), BackupError> {
        let store = ArtifactStore::open(&self.artifact_root)
            .map_err(|_| BackupError::CorruptManifest("artifact store is not readable"))?;
        let reference_manifests = store
            .reference_manifests()
            .map_err(|_| BackupError::CorruptManifest("artifact references are not readable"))?;
        let mut ids = policies
            .keys()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        for (reference, _) in &reference_manifests {
            ids.insert(
                store
                    .open_reference(*reference)
                    .map_err(|_| BackupError::CorruptManifest("artifact reference is invalid"))?
                    .digest(),
            );
        }
        let mut records = Vec::with_capacity(ids.len());
        for id in ids {
            let snapshot_policy = policies.get(&id);
            let policy = snapshot_policy.map(|policy| &policy.policy);
            let verified = store
                .open_verified(id)
                .map_err(|_| BackupError::MissingArtifact(id))?;
            let bytes = store
                .open_bytes(id)
                .map_err(|_| BackupError::MissingArtifact(id))?;
            let artifact_manifest = verified.manifest();
            let manifest_bytes = artifact_manifest.canonical_bytes();
            let id_text = id.to_string();
            write_new_synced(
                &destination.join(format!("{}.blob", artifact_hex(id)?)),
                &bytes,
            )?;
            write_new_synced(
                &destination.join(format!("{}.manifest", artifact_hex(id)?)),
                &manifest_bytes,
            )?;
            records.push(ManifestArtifact {
                id: id_text,
                size: bytes.len() as u64,
                object_sha256: hex(&sha256(&bytes)),
                manifest_sha256: hex(&sha256(&manifest_bytes)),
                media_type: artifact_manifest.media_type.clone(),
                class: artifact_class(artifact_manifest.class).to_owned(),
                principal: artifact_manifest.principal.clone(),
                project: artifact_manifest.project.clone(),
                retention: artifact_retention(artifact_manifest.retention),
                policy_retention: snapshot_policy.map(|policy| retention_period(policy.retention)),
                stored_at_unix_micros: artifact_manifest.stored_at_unix_micros,
                reachable: policy.is_some_and(|policy| policy.reachable),
                shared_reference: policy.is_some_and(|policy| policy.shared_reference),
                legal_hold: policy.is_some_and(|policy| policy.legal_hold),
                deletion_requested_at_unix_micros: policy
                    .and_then(|policy| policy.deletion_requested_at_unix_micros),
                backup_expires_at_unix_micros: expires_at_unix_micros,
            });
        }
        let mut references = Vec::with_capacity(reference_manifests.len());
        for (reference, bytes) in reference_manifests {
            let artifact = store
                .open_reference(reference)
                .map_err(|_| BackupError::CorruptManifest("artifact reference is invalid"))?;
            let reference_text = reference.to_string();
            write_new_synced(
                &destination.join(format!("{}.reference", artifact_reference_hex(reference)?)),
                &bytes,
            )?;
            references.push(ManifestReference {
                reference: reference_text,
                artifact: artifact.digest().to_string(),
                size: artifact.manifest().size,
                manifest_sha256: hex(&sha256(&bytes)),
                media_type: artifact.manifest().media_type.clone(),
                class: artifact_class(artifact.manifest().class).to_owned(),
                principal: artifact.manifest().principal.clone(),
                project: artifact.manifest().project.clone(),
                retention: artifact_retention(artifact.manifest().retention),
                stored_at_unix_micros: artifact.manifest().stored_at_unix_micros,
            });
        }
        Ok((records, references))
    }

    pub(crate) fn prune_generations(&mut self) -> Result<usize, BackupError> {
        let generations = list_generations(&self.destination)?;
        let remove = generations
            .len()
            .saturating_sub(self.config.retain_generations);
        for generation in generations.into_iter().take(remove) {
            fs::remove_dir_all(generation.path)?;
        }
        if remove != 0 {
            sync_directory(&self.destination)?;
        }
        self.current_generation = list_generations(&self.destination)?
            .last()
            .map(|generation| generation.name.clone());
        Ok(remove)
    }

    fn persist_health(&self) -> Result<(), BackupError> {
        let bytes = canonical_json(&self.health)?;
        atomic_write(&self.destination, HEALTH_FILE, &bytes)
    }
}

fn snapshot_artifact_policies(
    snapshot: &Connection,
    now_unix_micros: i64,
) -> Result<BTreeMap<ArtifactId, SnapshotArtifactPolicy>, BackupError> {
    let mut statement = snapshot.prepare(
        "SELECT object.object_key, object.artifact_digest, object.policy_json,
                EXISTS(
                    SELECT 1 FROM deletion_artifact_references AS reference
                    WHERE reference.artifact_id = substr(object.object_key, 10)
                      AND (reference.expires_at_unix_micros IS NULL
                           OR reference.expires_at_unix_micros > ?1)
                ),
                EXISTS(
                    SELECT 1 FROM deletion_legal_holds AS hold
                    WHERE hold.placed_at_unix_micros <= ?1
                      AND (hold.released_at_unix_micros IS NULL
                           OR hold.released_at_unix_micros > ?1)
                      AND ((hold.scope_kind = 'principal'
                            AND hold.scope_id = object.principal_id)
                        OR (hold.scope_kind = 'project'
                            AND hold.scope_id = object.project_id)
                        OR (hold.scope_kind = 'object'
                            AND hold.scope_id = object.object_key))
                ),
                (SELECT min(job.requested_at_unix_micros)
                   FROM deletion_jobs AS job
                  WHERE job.object_key = object.object_key
                    AND job.state <> 'completed')
         FROM deletion_objects AS object
         WHERE object.object_kind = 'artifact'
           AND object.physically_deleted = 0
           AND object.artifact_digest IS NOT NULL
         ORDER BY object.artifact_digest, object.object_key",
    )?;
    let rows = statement.query_map([now_unix_micros], |row| {
        Ok((
            row.get::<_, String>(1)?,
            row.get::<_, Vec<u8>>(2)?,
            row.get::<_, bool>(3)?,
            row.get::<_, bool>(4)?,
            row.get::<_, Option<i64>>(5)?,
        ))
    })?;
    let mut policies = BTreeMap::new();
    for row in rows {
        let (digest, policy_json, shared_reference, legal_hold, deletion_requested) = row?;
        let id = ArtifactId::parse(&digest)
            .map_err(|_| BackupError::CorruptManifest("invalid artifact policy digest"))?;
        let retention: RetentionPolicy = serde_json::from_slice(&policy_json)
            .map_err(|_| BackupError::CorruptManifest("invalid artifact retention policy"))?;
        let candidate = SnapshotArtifactPolicy {
            policy: ArtifactPolicy {
                reachable: true,
                shared_reference,
                legal_hold,
                deletion_requested_at_unix_micros: deletion_requested,
            },
            retention: retention.artifact,
        };
        policies
            .entry(id)
            .and_modify(|current: &mut SnapshotArtifactPolicy| {
                current.policy.shared_reference |= candidate.policy.shared_reference;
                current.policy.legal_hold |= candidate.policy.legal_hold;
                current.policy.deletion_requested_at_unix_micros = match (
                    current.policy.deletion_requested_at_unix_micros,
                    candidate.policy.deletion_requested_at_unix_micros,
                ) {
                    (Some(left), Some(right)) => Some(left.min(right)),
                    _ => None,
                };
                current.retention = current.retention.max(candidate.retention);
            })
            .or_insert(candidate);
    }
    Ok(policies)
}

fn restore_generation(
    generation_path: &Path,
    fresh_state_root: &Path,
    now_unix_micros: i64,
    expected_build_version: &str,
) -> Result<RestoreReport, BackupError> {
    if fresh_state_root.exists() {
        return Err(BackupError::RestoreTargetExists(
            fresh_state_root.to_owned(),
        ));
    }
    let manifest_bytes = read_regular_file(&generation_path.join(MANIFEST_FILE))?;
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes)?;
    if canonical_json(&manifest)? != manifest_bytes {
        return Err(BackupError::CorruptManifest("manifest is not canonical"));
    }
    validate_manifest(
        &manifest,
        generation_path,
        now_unix_micros,
        expected_build_version,
    )?;

    let parent = fresh_state_root
        .parent()
        .ok_or_else(|| BackupError::UnsafePath(fresh_state_root.to_owned()))?;
    ensure_directory(parent)?;
    let file_name = fresh_state_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| BackupError::UnsafePath(fresh_state_root.to_owned()))?;
    let temp = parent.join(format!(
        ".{file_name}.restore-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    if temp.exists() {
        return Err(BackupError::RestoreTargetExists(temp));
    }
    fs::create_dir(&temp)?;
    let result = restore_into(&manifest, generation_path, &temp);
    let report = match result {
        Ok(report) => report,
        Err(error) => {
            let _ = fs::remove_dir_all(&temp);
            return Err(error);
        }
    };
    sync_tree(&temp)?;
    fs::rename(&temp, fresh_state_root)?;
    sync_directory(parent)?;
    Ok(RestoreReport {
        generation: manifest.generation,
        state_root: fresh_state_root.to_owned(),
        commit_watermark: report.commit_watermark,
        artifact_count: report.artifact_count,
    })
}

fn restore_into(
    manifest: &Manifest,
    generation_path: &Path,
    temp: &Path,
) -> Result<RestoreReport, BackupError> {
    let database_relative = checked_relative(&manifest.database_relative_path)?;
    let artifact_relative = checked_relative(&manifest.artifact_relative_path)?;
    let database_target = temp.join(database_relative);
    let artifact_target = temp.join(artifact_relative);
    ensure_parent_directories(&database_target)?;
    ensure_directory_tree(&artifact_target)?;

    let database_source = generation_path.join(DATABASE_FILE);
    let database_bytes = read_regular_file(&database_source)?;
    if hex(&sha256(&database_bytes)) != manifest.database_sha256 {
        return Err(BackupError::DigestMismatch("database"));
    }
    write_new_synced(&database_target, &database_bytes)?;
    restore_artifacts(manifest, generation_path, &artifact_target)?;

    let connection = Connection::open(&database_target)?;
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(BackupError::IntegrityCheck(integrity));
    }
    let foreign_key_failure: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_foreign_key_check)",
        [],
        |row| row.get(0),
    )?;
    if foreign_key_failure {
        return Err(BackupError::IntegrityCheck(
            "foreign-key violation".to_owned(),
        ));
    }
    drop(connection);

    SqliteStore::validate(&database_target).map_err(|_| BackupError::SchemaMismatch)?;
    ProjectionStore::open(&database_target).map_err(|_| BackupError::SchemaMismatch)?;
    let connection = Connection::open_with_flags(
        &database_target,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let state = database_state(&connection)?;
    if state.schema_version != manifest.schema_version
        || state.migration_version != manifest.migration_version
    {
        return Err(BackupError::SchemaMismatch);
    }
    if state.commit_watermark != manifest.commit_watermark {
        return Err(BackupError::SemanticMismatch("commit watermark"));
    }
    if hex(&state.event_digest) != manifest.event_sha256 {
        return Err(BackupError::SemanticMismatch("event digest"));
    }
    if hex(&state.projection_digest) != manifest.projection_sha256 {
        return Err(BackupError::SemanticMismatch("projection digest"));
    }
    Ok(RestoreReport {
        generation: manifest.generation.clone(),
        state_root: temp.to_owned(),
        commit_watermark: state.commit_watermark,
        artifact_count: manifest.artifacts.len(),
    })
}

fn validate_manifest(
    manifest: &Manifest,
    generation_path: &Path,
    now_unix_micros: i64,
    expected_build_version: &str,
) -> Result<(), BackupError> {
    if manifest.format_version != MANIFEST_VERSION {
        return Err(BackupError::CorruptManifest("unsupported format version"));
    }
    validate_generation_name(&manifest.generation)?;
    if generation_path.file_name().and_then(|name| name.to_str()) != Some(&manifest.generation) {
        return Err(BackupError::CorruptManifest("generation name mismatch"));
    }
    if manifest.build_version != expected_build_version {
        return Err(BackupError::BuildMismatch);
    }
    if now_unix_micros >= manifest.backup_expires_at_unix_micros {
        return Err(BackupError::BackupExpired(
            manifest.backup_expires_at_unix_micros,
        ));
    }
    checked_digest(&manifest.database_sha256)?;
    checked_digest(&manifest.event_sha256)?;
    checked_digest(&manifest.projection_sha256)?;
    let mut previous = None;
    for artifact in &manifest.artifacts {
        let id = ArtifactId::parse(&artifact.id)
            .map_err(|_| BackupError::CorruptManifest("invalid artifact identifier"))?;
        if previous.is_some_and(|previous| previous >= id) {
            return Err(BackupError::CorruptManifest(
                "artifact inventory is not strictly ordered",
            ));
        }
        previous = Some(id);
        checked_digest(&artifact.object_sha256)?;
        checked_digest(&artifact.manifest_sha256)?;
        if artifact.backup_expires_at_unix_micros != manifest.backup_expires_at_unix_micros {
            return Err(BackupError::CorruptManifest("artifact expiry mismatch"));
        }
    }
    let mut previous_reference = None;
    for record in &manifest.references {
        let reference = ArtifactReference::parse(&record.reference)
            .map_err(|_| BackupError::CorruptManifest("invalid artifact reference"))?;
        if previous_reference.is_some_and(|previous| previous >= reference) {
            return Err(BackupError::CorruptManifest(
                "artifact references are not strictly ordered",
            ));
        }
        previous_reference = Some(reference);
        ArtifactId::parse(&record.artifact)
            .map_err(|_| BackupError::CorruptManifest("invalid referenced artifact"))?;
        checked_digest(&record.manifest_sha256)?;
    }
    Ok(())
}

fn restore_artifacts(
    manifest: &Manifest,
    generation_path: &Path,
    artifact_target: &Path,
) -> Result<(), BackupError> {
    for name in ["objects", "manifests", "staging"] {
        ensure_directory_tree(&artifact_target.join(name))?;
    }
    for record in &manifest.artifacts {
        let id = ArtifactId::parse(&record.id)
            .map_err(|_| BackupError::CorruptManifest("invalid artifact identifier"))?;
        let id_hex = artifact_hex(id)?;
        let bytes = read_regular_file(
            &generation_path
                .join("artifacts")
                .join(format!("{id_hex}.blob")),
        )?;
        let artifact_manifest = read_regular_file(
            &generation_path
                .join("artifacts")
                .join(format!("{id_hex}.manifest")),
        )?;
        if bytes.len() as u64 != record.size
            || ArtifactId::digest(&bytes) != id
            || hex(&sha256(&bytes)) != record.object_sha256
        {
            return Err(BackupError::DigestMismatch("artifact object"));
        }
        if hex(&sha256(&artifact_manifest)) != record.manifest_sha256 {
            return Err(BackupError::DigestMismatch("artifact manifest"));
        }
        let object_shard = artifact_target.join("objects").join(&id_hex[..2]);
        let manifest_shard = artifact_target.join("manifests").join(&id_hex[..2]);
        ensure_directory_tree(&object_shard)?;
        ensure_directory_tree(&manifest_shard)?;
        write_new_synced(&object_shard.join(format!("{}.blob", &id_hex[2..])), &bytes)?;
        write_new_synced(
            &manifest_shard.join(format!("{}.manifest", &id_hex[2..])),
            &artifact_manifest,
        )?;
    }
    let store = ArtifactStore::open(artifact_target)
        .map_err(|_| BackupError::CorruptManifest("restored artifact layout is invalid"))?;
    for record in &manifest.artifacts {
        let id = ArtifactId::parse(&record.id)
            .map_err(|_| BackupError::CorruptManifest("invalid artifact identifier"))?;
        let verified = store
            .open_verified(id)
            .map_err(|_| BackupError::DigestMismatch("restored artifact"))?;
        let artifact_manifest = verified.manifest();
        if artifact_manifest.size != record.size
            || artifact_manifest.media_type != record.media_type
            || artifact_class(artifact_manifest.class) != record.class
            || artifact_manifest.principal != record.principal
            || artifact_manifest.project != record.project
            || artifact_retention(artifact_manifest.retention) != record.retention
            || artifact_manifest.stored_at_unix_micros != record.stored_at_unix_micros
        {
            return Err(BackupError::SemanticMismatch("artifact policy manifest"));
        }
    }
    for record in &manifest.references {
        let reference = ArtifactReference::parse(&record.reference)
            .map_err(|_| BackupError::CorruptManifest("invalid artifact reference"))?;
        let bytes = read_regular_file(
            &generation_path
                .join("artifacts")
                .join(format!("{}.reference", artifact_reference_hex(reference)?)),
        )?;
        if hex(&sha256(&bytes)) != record.manifest_sha256 {
            return Err(BackupError::DigestMismatch("artifact reference manifest"));
        }
        let artifact = store
            .restore_reference_manifest(reference, &bytes)
            .map_err(|_| BackupError::SemanticMismatch("artifact reference"))?;
        if artifact.digest().to_string() != record.artifact
            || artifact.manifest().size != record.size
            || artifact.manifest().media_type != record.media_type
            || artifact_class(artifact.manifest().class) != record.class
            || artifact.manifest().principal != record.principal
            || artifact.manifest().project != record.project
            || artifact_retention(artifact.manifest().retention) != record.retention
            || artifact.manifest().stored_at_unix_micros != record.stored_at_unix_micros
        {
            return Err(BackupError::SemanticMismatch("artifact reference metadata"));
        }
    }
    Ok(())
}

struct DatabaseState {
    schema_version: i64,
    migration_version: i64,
    commit_watermark: u64,
    event_digest: [u8; 32],
    projection_digest: [u8; 32],
}

fn database_state(connection: &Connection) -> Result<DatabaseState, BackupError> {
    let schema_version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let migration_version = if table_exists(connection, "kit_sqlite_migrations")? {
        connection.query_row(
            "SELECT coalesce(max(version), 0) FROM kit_sqlite_migrations",
            [],
            |row| row.get(0),
        )?
    } else {
        0
    };
    let watermark: i64 = connection.query_row(
        "SELECT position FROM commit_watermark WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    let commit_watermark = u64::try_from(watermark)
        .map_err(|_| BackupError::CorruptManifest("negative commit watermark"))?;
    Ok(DatabaseState {
        schema_version,
        migration_version,
        commit_watermark,
        event_digest: event_digest(connection, commit_watermark)?,
        projection_digest: projection_digest(connection)?,
    })
}

fn event_digest(connection: &Connection, watermark: u64) -> Result<[u8; 32], BackupError> {
    let mut canonical = Vec::new();
    let mut statement = connection.prepare(
        "SELECT event_id, stream, sequence, commit_position, event_type, schema_version,
                occurred_at, causation_id, correlation_id, attempt_id, trace_id, payload, artifacts
         FROM events WHERE commit_position <= ?1 ORDER BY commit_position",
    )?;
    let mut rows = statement.query([watermark])?;
    let mut expected = 1_u64;
    while let Some(row) = rows.next()? {
        let position: i64 = row.get(3)?;
        if u64::try_from(position).ok() != Some(expected) {
            return Err(BackupError::SemanticMismatch("gapless event prefix"));
        }
        for index in [0, 1, 4, 6, 7, 8, 10] {
            put_bytes(&mut canonical, row.get::<_, String>(index)?.as_bytes());
        }
        canonical.extend_from_slice(&row.get::<_, i64>(2)?.to_be_bytes());
        canonical.extend_from_slice(&position.to_be_bytes());
        canonical.extend_from_slice(&row.get::<_, i64>(5)?.to_be_bytes());
        match row.get::<_, Option<String>>(9)? {
            Some(value) => {
                canonical.push(1);
                put_bytes(&mut canonical, value.as_bytes());
            }
            None => canonical.push(0),
        }
        put_bytes(&mut canonical, &row.get::<_, Vec<u8>>(11)?);
        put_bytes(&mut canonical, &row.get::<_, Vec<u8>>(12)?);
        expected += 1;
    }
    if expected - 1 != watermark {
        return Err(BackupError::SemanticMismatch("event watermark"));
    }
    Ok(sha256_domain(b"kit-backup-events-v1\0", &canonical))
}

fn projection_digest(connection: &Connection) -> Result<[u8; 32], BackupError> {
    let mut canonical = Vec::new();
    if !table_exists(connection, "projection_state")? {
        return Ok(sha256_domain(b"kit-backup-projections-v1\0", &canonical));
    }
    let mut statement = connection.prepare(
        "SELECT name, canonical_bytes, digest, checkpoint, updated_at
         FROM projection_state ORDER BY name",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, Vec<u8>>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    for row in rows {
        let (name, bytes, digest, checkpoint, updated_at) = row?;
        if digest.as_slice() != sha256(&bytes) {
            return Err(BackupError::SemanticMismatch("stored projection digest"));
        }
        put_bytes(&mut canonical, name.as_bytes());
        put_bytes(&mut canonical, &bytes);
        put_bytes(&mut canonical, &digest);
        canonical.extend_from_slice(&checkpoint.to_be_bytes());
        put_bytes(&mut canonical, updated_at.as_bytes());
    }
    Ok(sha256_domain(b"kit-backup-projections-v1\0", &canonical))
}

fn table_exists(connection: &Connection, name: &str) -> Result<bool, BackupError> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1)",
        [name],
        |row| row.get(0),
    )?)
}

fn list_generations(destination: &Path) -> Result<Vec<BackupGeneration>, BackupError> {
    let mut generations = Vec::new();
    for entry in fs::read_dir(destination)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !name.starts_with(GENERATION_PREFIX) {
            continue;
        }
        if !is_directory(&entry.path())? {
            return Err(BackupError::UnsafePath(entry.path()));
        }
        let bytes = read_regular_file(&entry.path().join(MANIFEST_FILE))?;
        let manifest: Manifest = serde_json::from_slice(&bytes)?;
        if canonical_json(&manifest)? != bytes || manifest.generation != name {
            return Err(BackupError::CorruptManifest(
                "generation inventory mismatch",
            ));
        }
        generations.push(BackupGeneration {
            name,
            path: entry.path(),
            created_at_unix_micros: manifest.created_at_unix_micros,
            expires_at_unix_micros: manifest.backup_expires_at_unix_micros,
        });
    }
    generations.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(generations)
}

fn load_health(destination: &Path) -> Result<HealthRecord, BackupError> {
    let path = destination.join(HEALTH_FILE);
    if !path.exists() {
        return Ok(HealthRecord {
            format_version: HEALTH_VERSION,
            ..HealthRecord::default()
        });
    }
    let bytes = read_regular_file(&path)?;
    let health: HealthRecord = serde_json::from_slice(&bytes)?;
    if health.format_version != HEALTH_VERSION || canonical_json(&health)? != bytes {
        return Err(BackupError::CorruptManifest("invalid backup health record"));
    }
    Ok(health)
}

fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>, BackupError> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn atomic_write(directory: &Path, name: &str, bytes: &[u8]) -> Result<(), BackupError> {
    let temp = directory.join(format!(
        ".{name}.{}-{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    write_new_synced(&temp, bytes)?;
    fs::rename(&temp, directory.join(name))?;
    sync_directory(directory)
}

fn write_new_synced(path: &Path, bytes: &[u8]) -> Result<(), BackupError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    no_follow(&mut options);
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn read_regular_file(path: &Path) -> Result<Vec<u8>, BackupError> {
    if !is_file(path)? {
        return Err(BackupError::UnsafePath(path.to_owned()));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    no_follow(&mut options);
    let mut file = options.open(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(BackupError::UnsafePath(path.to_owned()));
    }
    Ok(bytes)
}

fn ensure_directory(path: &Path) -> Result<(), BackupError> {
    if path.exists() {
        if !is_directory(path)? {
            return Err(BackupError::UnsafePath(path.to_owned()));
        }
    } else {
        fs::create_dir(path)?;
    }
    Ok(())
}

fn ensure_directory_tree(path: &Path) -> Result<(), BackupError> {
    fs::create_dir_all(path)?;
    if !is_directory(path)? {
        return Err(BackupError::UnsafePath(path.to_owned()));
    }
    Ok(())
}

fn ensure_parent_directories(path: &Path) -> Result<(), BackupError> {
    let parent = path
        .parent()
        .ok_or_else(|| BackupError::UnsafePath(path.to_owned()))?;
    ensure_directory_tree(parent)
}

fn canonical_directory(path: &Path) -> Result<PathBuf, BackupError> {
    if !is_directory(path)? {
        return Err(BackupError::UnsafePath(path.to_owned()));
    }
    Ok(fs::canonicalize(path)?)
}

fn canonical_file(path: &Path) -> Result<PathBuf, BackupError> {
    if !is_file(path)? {
        return Err(BackupError::UnsafePath(path.to_owned()));
    }
    Ok(fs::canonicalize(path)?)
}

fn is_directory(path: &Path) -> Result<bool, BackupError> {
    Ok(fs::symlink_metadata(path)?.file_type().is_dir())
}

fn is_file(path: &Path) -> Result<bool, BackupError> {
    Ok(fs::symlink_metadata(path)?.file_type().is_file())
}

fn sync_file(path: &Path) -> Result<(), BackupError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), BackupError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn sync_tree(path: &Path) -> Result<(), BackupError> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if is_directory(&entry.path())? {
            sync_tree(&entry.path())?;
        } else if is_file(&entry.path())? {
            sync_file(&entry.path())?;
        } else {
            return Err(BackupError::UnsafePath(entry.path()));
        }
    }
    sync_directory(path)
}

fn path_text(path: &Path) -> Result<String, BackupError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| BackupError::UnsafePath(path.to_owned()))
}

fn checked_relative(value: &str) -> Result<PathBuf, BackupError> {
    let path = PathBuf::from(value);
    validate_relative(&path)?;
    Ok(path)
}

fn validate_relative(path: &Path) -> Result<(), BackupError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(BackupError::UnsafePath(path.to_owned()));
    }
    Ok(())
}

fn validate_generation_name(name: &str) -> Result<(), BackupError> {
    if !name.starts_with(GENERATION_PREFIX)
        || name.len() > 128
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(BackupError::GenerationNotFound(name.to_owned()));
    }
    Ok(())
}

fn artifact_hex(id: ArtifactId) -> Result<String, BackupError> {
    id.to_string()
        .strip_prefix("blake3:")
        .map(str::to_owned)
        .ok_or(BackupError::CorruptManifest("invalid artifact identifier"))
}

fn artifact_reference_hex(reference: ArtifactReference) -> Result<String, BackupError> {
    reference
        .to_string()
        .strip_prefix("artifact-ref:")
        .map(str::to_owned)
        .ok_or(BackupError::CorruptManifest("invalid artifact reference"))
}

fn artifact_class(class: ArtifactClass) -> &'static str {
    match class {
        ArtifactClass::Log => "log",
        ArtifactClass::Diff => "diff",
        ArtifactClass::File => "file",
        ArtifactClass::Index => "index",
        ArtifactClass::Image => "image",
        ArtifactClass::Report => "report",
        ArtifactClass::RestrictedEncrypted => "restricted_encrypted",
    }
}

fn artifact_retention(retention: ArtifactRetention) -> String {
    match retention {
        ArtifactRetention::UntilUnixMicros(value) => format!("until:{value}"),
        ArtifactRetention::Forever => "forever".to_owned(),
    }
}

fn retention_period(retention: RetentionPeriod) -> String {
    match retention {
        RetentionPeriod::ForMicros(value) => format!("for:{value}"),
        RetentionPeriod::Forever => "forever".to_owned(),
    }
}

fn checked_digest(value: &str) -> Result<(), BackupError> {
    if valid_hex(value, 64) {
        Ok(())
    } else {
        Err(BackupError::CorruptManifest("invalid SHA-256 digest"))
    }
}

fn valid_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn hex(bytes: &[u8]) -> String {
    use fmt::Write as _;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}

fn put_bytes(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    output.extend_from_slice(bytes);
}

#[cfg(unix)]
fn no_follow(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    #[cfg(target_os = "macos")]
    const O_NOFOLLOW: i32 = 0x0000_0100;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const O_NOFOLLOW: i32 = 0x0002_0000;
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
    const O_NOFOLLOW: i32 = 0;
    options.custom_flags(O_NOFOLLOW);
}

#[cfg(not(unix))]
fn no_follow(_: &mut OpenOptions) {}
