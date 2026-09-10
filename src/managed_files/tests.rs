#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
use super::*;
use serde_json::json;
use std::fs as disk;
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
    store: FileStore,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore {
            base: dir.path().canonicalize().unwrap().join("store"),
        };
        Self { dir, store }
    }
    fn source(&self, name: &str, format: ImageFormat, width: u32, height: u32) -> PathBuf {
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::new_luma8(width, height)
            .write_to(&mut bytes, format)
            .unwrap();
        let path = self.dir.path().join(name);
        disk::write(&path, bytes.into_inner()).unwrap();
        path
    }
    fn import(&self, name: &str) -> FileReference {
        let path = self.source(name, ImageFormat::Png, 3, 2);
        self.store.import("session", &path, None).unwrap()
    }
    fn object(&self, reference: &FileReference) -> PathBuf {
        self.store.session_directory("session").join(&reference.id)
    }
    fn select(&self, value: &Value) -> Result<Vec<Part>> {
        self.store.selected_parts("session", value, None)
    }
}
#[test]
fn png_and_jpeg_snapshots_survive_source_deletion_and_reopen() {
    for (format, name, mime) in [
        (ImageFormat::Png, "image.png", "image/png"),
        (ImageFormat::Jpeg, "image.jpg", "image/jpeg"),
    ] {
        let f = Fixture::new();
        let source = f.source(name, format, 3, 2);
        let bytes = disk::read(&source).unwrap();
        let reference = f.store.import("session", &source, None).unwrap();
        assert_eq!(reference.mime_type, mime);
        assert_eq!(
            reference.image,
            ImageDimensions {
                width: 3,
                height: 2
            }
        );
        assert_eq!(reference.size_bytes, bytes.len() as u64);
        disk::remove_file(source).unwrap();
        let reopened = FileStore {
            base: f.store.base.clone(),
        };
        let parts = reopened
            .selected_parts("session", &json!(reference), None)
            .unwrap();
        assert_eq!(parts.len(), 2);
        match &parts[1] {
            Part::Media(media) => match &media.data {
                DataRef::InlineBytes(actual) => assert_eq!(actual, &bytes),
                _ => panic!("expected inline snapshot"),
            },
            _ => panic!("expected image"),
        }
        assert!(
            reopened
                .selected_parts("other-session", &json!(reference), None)
                .is_err()
        );
    }
}
#[test]
fn rejects_unknown_versions_fields_forged_stale_and_traversal_refs() {
    let f = Fixture::new();
    let reference = f.import("image.png");
    let good = json!(reference);
    for (key, value) in [
        ("version", json!(2)),
        ("unexpected", json!(true)),
        ("name", json!("forged.png")),
        ("id", json!(format!("file_{}", "0".repeat(64)))),
        ("id", json!("../image.png")),
        ("id", json!("/tmp/image.png")),
        ("mime_type", json!("image/gif")),
        ("size_bytes", json!(0)),
    ] {
        let mut altered = good.clone();
        altered[key] = value;
        assert!(f.select(&altered).is_err(), "accepted {altered}");
    }
    let mut nested_unknown = good.clone();
    nested_unknown["image"]["unexpected"] = json!(1);
    assert!(f.select(&nested_unknown).is_err());
    disk::remove_file(f.object(&reference)).unwrap();
    assert!(f.select(&good).is_err());
}
#[test]
fn nested_selection_is_sorted_escaped_deduplicated_and_return_scoped() {
    let f = Fixture::new();
    let a = f.import("a.png");
    let b = f.import("b.png");
    let parts = f.select(&json!({"z": a, "a/~": [a, b]})).unwrap();
    assert_eq!(parts.len(), 4);
    for (index, expected) in [
        (0, "Image #1 at JSON Pointer \"/a~1~0/0\": a.png"),
        (2, "Image #2 at JSON Pointer \"/a~1~0/1\": b.png"),
    ] {
        match &parts[index] {
            Part::Text(text) => assert_eq!(text.text, expected),
            _ => panic!("expected label"),
        }
    }
    assert!(
        f.select(&json!({"done": true, "intermediate": null}))
            .unwrap()
            .is_empty()
    );
    let mut contradictory = json!(a);
    contradictory["name"] = json!("different.png");
    assert!(
        f.select(&json!([a, contradictory]))
            .unwrap_err()
            .contains("contradictory")
    );
}
#[test]
fn traversal_depth_label_and_occurrence_budgets() {
    let f = Fixture::new();
    let reference = json!(f.import("image.png"));
    assert_eq!(
        f.select(&Value::Array(vec![reference.clone(); MAX_OCCURRENCES]))
            .unwrap()
            .len(),
        2
    );
    assert!(
        f.select(&Value::Array(vec![reference; MAX_OCCURRENCES + 1]))
            .unwrap_err()
            .contains("occurrences")
    );
    assert!(
        f.select(&Value::Array(vec![Value::Null; MAX_NODES - 1]))
            .unwrap()
            .is_empty()
    );
    assert!(
        f.select(&Value::Array(vec![Value::Null; MAX_NODES]))
            .unwrap_err()
            .contains("traversal")
    );
    let mut nested = Value::Null;
    for _ in 0..MAX_DEPTH {
        nested = json!([nested]);
    }
    assert!(f.select(&nested).unwrap().is_empty());
    assert!(
        f.select(&json!([nested]))
            .unwrap_err()
            .contains("traversal")
    );
    assert!(
        f.select(&json!({"x".repeat(MAX_HEADER_BYTES + 1): null}))
            .unwrap_err()
            .contains("label")
    );
}
#[test]
fn aggregate_image_count_and_pixel_budgets() {
    let f = Fixture::new();
    let references: Vec<_> = (0..=MAX_IMAGES)
        .map(|i| f.import(&format!("{i}.png")))
        .collect();
    assert_eq!(
        f.select(&json!(&references[..MAX_IMAGES])).unwrap().len(),
        MAX_IMAGES * 2
    );
    assert!(
        f.select(&json!(references))
            .unwrap_err()
            .contains("delivery budget")
    );
    let source = f.source("large.png", ImageFormat::Png, 4096, 4096);
    let large: Vec<_> = (0..3)
        .map(|_| f.store.import("session", &source, None).unwrap())
        .collect();
    assert_eq!(f.select(&json!(&large[..2])).unwrap().len(), 4);
    assert!(
        f.select(&json!(large))
            .unwrap_err()
            .contains("delivery budget")
    );
}
#[test]
fn aggregate_encoded_byte_budget() {
    let f = Fixture::new();
    let source = f.source("padded.png", ImageFormat::Png, 3, 2);
    // The PNG decoder accepts trailing bytes; preserved bytes still count toward delivery.
    let mut bytes = disk::read(&source).unwrap();
    bytes.resize(MAX_FILE_BYTES as usize, 0);
    disk::write(&source, bytes).unwrap();
    let references: Vec<_> = (0..3)
        .map(|_| f.store.import("session", &source, None).unwrap())
        .collect();
    assert_eq!(f.select(&json!(&references[..2])).unwrap().len(), 4);
    assert!(
        f.select(&json!(references))
            .unwrap_err()
            .contains("delivery budget")
    );
}
#[test]
fn rejects_malformed_corrupt_truncated_and_trailing_envelopes() {
    let f = Fixture::new();
    let reference = f.import("image.png");
    let path = f.object(&reference);
    let original = disk::read(&path).unwrap();
    let mut cases = vec![
        vec![],
        original[..7].to_vec(),
        original[..12].to_vec(),
        original[..original.len() - 1].to_vec(),
    ];
    let mut magic = original.clone();
    magic[0] ^= 1;
    cases.push(magic);
    let mut corrupt = original.clone();
    *corrupt.last_mut().unwrap() ^= 1;
    cases.push(corrupt);
    let mut trailing = original.clone();
    trailing.push(0);
    cases.push(trailing);
    for length in [0, MAX_HEADER_BYTES as u32 + 1] {
        let mut invalid = original.clone();
        invalid[8..12].copy_from_slice(&length.to_le_bytes());
        cases.push(invalid);
    }
    let mut malformed = original.clone();
    malformed[12] = b'!';
    cases.push(malformed);
    let header_len = u32::from_le_bytes(original[8..12].try_into().unwrap()) as usize;
    let mut header: Value = serde_json::from_slice(&original[12..12 + header_len]).unwrap();
    header["unknown"] = json!(true);
    let encoded = serde_json::to_vec(&header).unwrap();
    let mut unknown = MAGIC.to_vec();
    unknown.extend_from_slice(&(encoded.len() as u32).to_le_bytes());
    unknown.extend(encoded);
    unknown.extend_from_slice(&original[12 + header_len..]);
    cases.push(unknown);
    for (index, bytes) in cases.into_iter().enumerate() {
        disk::write(&path, bytes).unwrap();
        assert!(
            f.select(&json!(reference)).is_err(),
            "accepted damaged envelope {index}"
        );
    }
}
#[cfg(unix)]
#[test]
fn rejects_symlink_objects_and_session_directories() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let reference = f.import("image.png");
    let path = f.object(&reference);
    let outside = f.dir.path().join("outside");
    disk::rename(&path, &outside).unwrap();
    symlink(&outside, &path).unwrap();
    assert!(f.select(&json!(reference)).is_err());
    disk::remove_file(&path).unwrap();
    disk::rename(&outside, &path).unwrap();
    let session = f.store.session_directory("session");
    let moved = f.dir.path().join("moved-session");
    disk::rename(&session, &moved).unwrap();
    symlink(&moved, &session).unwrap();
    assert!(f.select(&json!(reference)).is_err());
}
#[test]
fn import_rejects_invalid_destination_and_cancelled_work() {
    let f = Fixture::new();
    let source = f.source("image.png", ImageFormat::Png, 3, 2);
    disk::write(&f.store.base, b"not a directory").unwrap();
    assert!(f.store.import("session", &source, None).is_err());
    let f = Fixture::new();
    let source = f.source("image.png", ImageFormat::Png, 3, 2);
    let controller = agentkit_core::CancellationController::new();
    let cancellation = controller.handle().checkpoint();
    controller.interrupt();
    assert!(
        f.store
            .import("session", &source, Some(&cancellation))
            .unwrap_err()
            .contains("cancelled")
    );
    assert!(!f.store.base.exists(), "cancelled import published storage");
}
#[test]
fn import_rejects_directory_nonregular_oversize_corrupt_and_dimensions() {
    let f = Fixture::new();
    assert!(f.store.import("session", f.dir.path(), None).is_err());
    #[cfg(unix)]
    {
        let socket_path = f.dir.path().join("socket");
        let _socket = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        assert!(f.store.import("session", &socket_path, None).is_err());
    }
    let huge = f.dir.path().join("huge.png");
    disk::File::create(&huge)
        .unwrap()
        .set_len(MAX_FILE_BYTES + 1)
        .unwrap();
    assert!(f.store.import("session", &huge, None).is_err());
    for (name, bytes) in [
        ("empty.png", vec![]),
        ("junk.png", b"not an image".to_vec()),
        ("truncated.png", vec![137, 80, 78, 71, 13, 10, 26, 10]),
    ] {
        let path = f.dir.path().join(name);
        disk::write(&path, bytes).unwrap();
        assert!(f.store.import("session", &path, None).is_err());
    }
    let source = f.source("corrupt.png", ImageFormat::Png, 3, 2);
    let mut bytes = disk::read(&source).unwrap();
    bytes.truncate(bytes.len() / 2);
    disk::write(&source, bytes).unwrap();
    assert!(f.store.import("session", &source, None).is_err());
    for (width, height) in [(MAX_DIMENSION + 1, 1), (4097, 4096)] {
        let source = f.source("dimensions.png", ImageFormat::Png, width, height);
        assert!(f.store.import("session", &source, None).is_err());
    }
    assert!(!f.store.base.exists());
}

// PNG chunks use CRC-32 over their type and data. Keep fixture construction local
// rather than adding a production dependency solely to encode a one-frame APNG.
fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut chunk = (data.len() as u32).to_be_bytes().to_vec();
    chunk.extend_from_slice(kind);
    chunk.extend_from_slice(data);
    let mut crc = u32::MAX;
    for byte in &chunk[4..] {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    chunk.extend_from_slice(&(!crc).to_be_bytes());
    chunk
}

#[test]
fn import_rejects_apng_even_with_one_frame() {
    let f = Fixture::new();
    let source = f.source("animated.png", ImageFormat::Png, 3, 2);
    let bytes = disk::read(&source).unwrap();
    // First frame uses the existing IDAT payload with matching canvas dimensions.
    let mut animation = bytes[..33].to_vec(); // signature and IHDR
    animation.extend(png_chunk(b"acTL", &[0, 0, 0, 1, 0, 0, 0, 0]));
    let mut frame = Vec::new();
    for value in [0_u32, 3, 2, 0, 0] {
        frame.extend_from_slice(&value.to_be_bytes());
    }
    frame.extend_from_slice(&[0, 1, 0, 10, 0, 0]); // delay, dispose, blend
    animation.extend(png_chunk(b"fcTL", &frame));
    animation.extend_from_slice(&bytes[33..]);
    disk::write(&source, animation).unwrap();
    let error = f.store.import("session", &source, None).unwrap_err();
    assert!(error.contains("animated PNG"), "{error}");
    assert!(!f.store.base.exists());
}

#[test]
fn import_rejects_decoded_allocation_over_budget() {
    let f = Fixture::new();
    // RGBA16 requires 72 MB despite fitting both dimension and pixel limits.
    let image = DynamicImage::new_rgba16(3000, 3000);
    let mut bytes = Cursor::new(Vec::new());
    image.write_to(&mut bytes, ImageFormat::Png).unwrap();
    drop(image);
    assert!(bytes.get_ref().len() as u64 <= MAX_FILE_BYTES);
    let source = f.dir.path().join("allocation.png");
    disk::write(&source, bytes.into_inner()).unwrap();
    assert!(f.store.import("session", &source, None).is_err());
    assert!(!f.store.base.exists());
}

#[test]
fn aggregate_label_budget_is_checked_before_resolving_missing_payloads() {
    let f = Fixture::new();
    let mut selected = serde_json::Map::new();
    for prefix in ['a', 'b', 'c'] {
        let reference = f.import(&format!("{prefix}.png"));
        let key = prefix.to_string().repeat(1500);
        // Every individual path and label is permitted; only their sum exceeds
        // 4 KiB. Resolve once to establish genuine same-session references.
        let individual = json!({key.clone(): reference});
        assert_eq!(f.select(&individual).unwrap().len(), 2);
        disk::remove_file(f.object(&reference)).unwrap();
        assert!(f.select(&individual).unwrap_err().contains("inaccessible"));
        selected.insert(key, json!(reference));
    }
    let error = f.select(&Value::Object(selected)).unwrap_err();
    assert_eq!(error, "selected image labels exceed the 4 KiB label budget");
}

#[test]
fn current_writer_descriptor_has_strict_shape_and_roundtrips() {
    let f = Fixture::new();
    let source = f.source("shape.png", ImageFormat::Png, 3, 2);
    let reference = f.store.import("session", &source, None).unwrap();
    let encoded = serde_json::to_value(&reference).unwrap();
    assert_eq!(
        encoded,
        json!({
            "$kit": "file",
            "version": 1,
            "id": reference.id,
            "name": "shape.png",
            "path": source.canonicalize().unwrap(),
            "mime_type": "image/png",
            "size_bytes": disk::metadata(&source).unwrap().len(),
            "image": {"width": 3, "height": 2}
        })
    );
    let decoded: FileReference = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(decoded, reference);
    assert_eq!(f.select(&json!(decoded)).unwrap().len(), 2);
    for key in encoded
        .as_object()
        .unwrap()
        .keys()
        .filter(|key| *key != "path")
    {
        let mut missing = encoded.clone();
        missing.as_object_mut().unwrap().remove(key);
        assert!(
            serde_json::from_value::<FileReference>(missing).is_err(),
            "accepted missing {key}"
        );
    }
    let mut unknown = encoded.clone();
    unknown["unknown"] = json!(source);
    assert!(serde_json::from_value::<FileReference>(unknown).is_err());
    let mut unknown_dimension = encoded;
    unknown_dimension["image"]["channels"] = json!(1);
    assert!(serde_json::from_value::<FileReference>(unknown_dimension).is_err());
}

#[test]
fn import_rejects_control_character_name_and_unsupported_image_format() {
    let f = Fixture::new();
    let invalid_name = f.source("invalid\nname.png", ImageFormat::Png, 3, 2);
    let error = f.store.import("session", &invalid_name, None).unwrap_err();
    assert!(error.contains("file name"), "{error}");
    let unsupported = f.dir.path().join("unsupported.gif");
    DynamicImage::new_rgba8(3, 2)
        .save_with_format(&unsupported, ImageFormat::Gif)
        .unwrap();
    let error = f.store.import("session", &unsupported, None).unwrap_err();
    assert!(error.contains("only nonanimated PNG and JPEG"), "{error}");
    assert!(!f.store.base.exists());
}

const RESTART_MANIFEST_ENV: &str = "KIT_MANAGED_FILES_RESTART_TEST_MANIFEST";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RestartManifest {
    base: PathBuf,
    source: PathBuf,
    reference: FileReference,
    bytes: Vec<u8>,
    digest: String,
    parent_pid: u32,
}

#[test]
fn snapshots_resolve_in_a_fresh_process_after_source_deletion() {
    for (format, name) in [
        (ImageFormat::Png, "restart.png"),
        (ImageFormat::Jpeg, "restart.jpg"),
    ] {
        let f = Fixture::new();
        let source = f.source(name, format, 3, 2);
        let bytes = disk::read(&source).unwrap();
        let reference = f.store.import("session", &source, None).unwrap();
        let manifest = RestartManifest {
            base: f.store.base.clone(),
            source: source.clone(),
            reference,
            digest: blake3::hash(&bytes).to_hex().to_string(),
            bytes,
            parent_pid: std::process::id(),
        };
        let manifest_path = f.dir.path().join("restart-manifest.json");
        disk::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        disk::remove_file(source).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "managed_files::tests::fresh_process_resolve_child",
                "--ignored",
                "--nocapture",
            ])
            .env(RESTART_MANIFEST_ENV, &manifest_path)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "restart child failed for {name}: {}\n{stdout}\n{stderr}",
            output.status
        );
        assert!(
            stdout.contains("managed_files::tests::fresh_process_resolve_child ... ok"),
            "child test did not succeed: {stdout}"
        );
        assert!(
            stdout.contains("1 passed; 0 failed"),
            "child did not run exactly one successful test: {stdout}"
        );
    }
}

#[test]
#[ignore = "invoked only by the fresh-process restart parent with a fixture manifest"]
fn fresh_process_resolve_child() {
    let path = std::env::var_os(RESTART_MANIFEST_ENV).expect("restart manifest path");
    let manifest: RestartManifest = serde_json::from_slice(&disk::read(path).unwrap()).unwrap();
    assert_ne!(std::process::id(), manifest.parent_pid);
    assert!(!manifest.source.exists(), "original source must be deleted");
    let store = FileStore {
        base: manifest.base,
    };
    let parts = store
        .selected_parts("session", &json!(manifest.reference), None)
        .unwrap();
    assert_eq!(parts.len(), 2);
    match &parts[1] {
        Part::Media(media) => match &media.data {
            DataRef::InlineBytes(actual) => {
                assert_eq!(actual, &manifest.bytes);
                assert_eq!(blake3::hash(actual).to_hex().as_str(), manifest.digest);
            }
            _ => panic!("expected inline snapshot"),
        },
        _ => panic!("expected image"),
    }
}

mod faults;

#[test]
fn generated_exports_are_real_movable_images_with_independent_snapshots() {
    for (format, name) in [
        (ImageFormat::Png, "generated.png"),
        (ImageFormat::Jpeg, "generated.jpg"),
    ] {
        let f = Fixture::new();
        let source = f.source(name, format, 3, 2);
        let bytes = disk::read(&source).unwrap();
        let reference = f.store.import_bytes("session", name, &bytes, None).unwrap();
        let path = Path::new(reference.path.as_deref().unwrap());
        assert!(path.is_absolute());
        assert!(disk::symlink_metadata(path).unwrap().is_file());
        assert_ne!(path, f.object(&reference));
        assert_eq!(disk::read(path).unwrap(), bytes);
        assert_eq!(
            image::guess_format(&disk::read(path).unwrap()).unwrap(),
            format
        );
        let moved = f.dir.path().join("moved image");
        assert!(
            std::process::Command::new("mv")
                .arg(path)
                .arg(&moved)
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(disk::read(&moved).unwrap(), bytes);
        assert!(!path.exists());
        let restarted = FileStore {
            base: f.store.base.clone(),
        };
        assert_eq!(restarted.resolve("session", &reference).unwrap(), bytes);
        disk::remove_file(moved).unwrap();
        assert_eq!(restarted.resolve("session", &reference).unwrap(), bytes);
        assert!(restarted.resolve("other-session", &reference).is_err());
    }
}

#[test]
fn imports_reuse_source_path_without_exporting_or_changing_bytes() {
    let f = Fixture::new();
    let source = f.source("original.png", ImageFormat::Png, 3, 2);
    let bytes = disk::read(&source).unwrap();
    let reference = f.store.import("session", &source, None).unwrap();
    assert_eq!(
        Path::new(reference.path.as_deref().unwrap()),
        source.canonicalize().unwrap()
    );
    assert!(
        !f.store
            .session_directory("session")
            .join("exports")
            .exists()
    );
    assert_eq!(disk::read(&source).unwrap(), bytes);
    let moved = f.dir.path().join("relocated.png");
    disk::rename(source, moved).unwrap();
    assert_eq!(f.store.resolve("session", &reference).unwrap(), bytes);
}

#[test]
fn historical_references_and_envelopes_migrate_without_rewriting() {
    let f = Fixture::new();
    let reference = f.import("legacy.png");
    let bytes = f.store.resolve("session", &reference).unwrap();
    let mut legacy = json!(reference);
    legacy.as_object_mut().unwrap().remove("path");
    let decoded: FileReference = serde_json::from_value(legacy.clone()).unwrap();
    assert!(decoded.path.is_none());
    assert_eq!(f.store.resolve("session", &decoded).unwrap(), bytes);
    assert_eq!(
        f.select(&json!([legacy.clone(), reference])).unwrap().len(),
        2
    );
    let header = serde_json::to_vec(
        &json!({"file":legacy, "digest":blake3::hash(&bytes).to_hex().to_string()}),
    )
    .unwrap();
    let mut envelope = MAGIC.to_vec();
    envelope.extend_from_slice(&(header.len() as u32).to_le_bytes());
    envelope.extend_from_slice(&header);
    envelope.extend_from_slice(&bytes);
    disk::write(f.object(&reference), &envelope).unwrap();
    assert_eq!(f.store.resolve("session", &reference).unwrap(), bytes);
    assert_eq!(f.store.resolve("session", &decoded).unwrap(), bytes);
    assert_eq!(disk::read(f.object(&reference)).unwrap(), envelope);
    let mut migrated = legacy;
    migrate_file_reference(&mut migrated);
    let once = migrated.clone();
    migrate_file_reference(&mut migrated);
    assert_eq!(migrated, once);
    assert!(migrated["path"].is_null());
    let mut current = json!(reference);
    let before = current.clone();
    migrate_file_reference(&mut current);
    assert_eq!(current, before);
    for invalid in [
        json!("relative.png"),
        json!(123),
        json!("/bad\u{0}path"),
        json!(format!("/{}", "x".repeat(4096))),
    ] {
        let mut malformed = before.clone();
        malformed["path"] = invalid;
        assert!(serde_json::from_value::<FileReference>(malformed.clone()).is_err());
        assert!(f.select(&malformed).is_err());
    }
}
