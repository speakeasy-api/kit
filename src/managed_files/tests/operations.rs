use super::*;
use image::{ImageEncoder as _, Rgba, RgbaImage};

fn pixels(width: u32, height: u32) -> RgbaImage {
    RgbaImage::from_fn(width, height, |x, y| {
        Rgba([(y * width + x + 1) as u8, 0, 0, 255])
    })
}

fn import_pixels(f: &Fixture, image: &RgbaImage) -> FileReference {
    let path = f.dir.path().join("pixels.png");
    image.save(&path).unwrap();
    f.store.import("session", &path, None).unwrap()
}

fn transformed(f: &Fixture, file: &FileReference, op: Transform) -> (FileReference, RgbaImage) {
    let result = f.store.transform("session", file, op, None).unwrap();
    let bytes = f.store.resolve("session", &result).unwrap();
    (
        result,
        image::load_from_memory(&bytes).unwrap().into_rgba8(),
    )
}

fn reds(image: &RgbaImage) -> Vec<u8> {
    image.pixels().map(|p| p[0]).collect()
}

#[test]
fn clockwise_rotations_publish_new_immutable_authorized_pngs() {
    let f = Fixture::new();
    let file = import_pixels(&f, &pixels(3, 2));
    let original = f.store.resolve("session", &file).unwrap();
    for (degrees, expected, dims) in [
        (90, vec![4, 1, 5, 2, 6, 3], (2, 3)),
        (180, vec![6, 5, 4, 3, 2, 1], (3, 2)),
        (270, vec![3, 6, 2, 5, 1, 4], (2, 3)),
    ] {
        let (result, image) = transformed(&f, &file, Transform::Rotate { degrees });
        assert_eq!(reds(&image), expected);
        assert_eq!(image.dimensions(), dims);
        assert_ne!(result.id, file.id);
        assert_eq!(result.mime_type, "image/png");
        assert_eq!(
            image::guess_format(&f.store.resolve("session", &result).unwrap()).unwrap(),
            ImageFormat::Png
        );
    }
    assert_eq!(f.store.resolve("session", &file).unwrap(), original);
    let op = Transform::Rotate { degrees: 90 };
    assert!(f.store.transform("other", &file, op, None).is_err());
    let mut forged = file.clone();
    forged.name = "forged.png".into();
    assert!(f.store.transform("session", &forged, op, None).is_err());
    assert!(
        f.store
            .export("other", &file, &f.dir.path().join("unauthorized"), None)
            .is_err()
    );
    assert!(
        f.store
            .export("session", &forged, &f.dir.path().join("forged"), None)
            .is_err()
    );
}

#[test]
fn nine_anchors_and_odd_center_rounding() {
    let f = Fixture::new();
    let wide = import_pixels(&f, &pixels(5, 2));
    let tall = import_pixels(&f, &pixels(2, 5));
    for (anchor, x, y) in [
        (Anchor::TopLeft, 0, 0),
        (Anchor::Top, 1, 0),
        (Anchor::TopRight, 3, 0),
        (Anchor::Left, 0, 1),
        (Anchor::Center, 1, 1),
        (Anchor::Right, 3, 1),
        (Anchor::BottomLeft, 0, 3),
        (Anchor::Bottom, 1, 3),
        (Anchor::BottomRight, 3, 3),
    ] {
        let op = Transform::Crop {
            aspect_ratio: AspectRatio {
                width: 1,
                height: 1,
            },
            anchor,
        };
        let (_, image) = transformed(&f, &wide, op);
        assert_eq!(reds(&image), vec![1 + x, 2 + x, 6 + x, 7 + x]);
        let (_, image) = transformed(&f, &tall, op);
        assert_eq!(
            reds(&image),
            vec![1 + 2 * y, 2 + 2 * y, 3 + 2 * y, 4 + 2 * y]
        );
    }
    let file = import_pixels(&f, &pixels(7, 5));
    let (_, image) = transformed(
        &f,
        &file,
        Transform::Crop {
            aspect_ratio: AspectRatio {
                width: 2,
                height: 3,
            },
            anchor: Anchor::Center,
        },
    );
    assert_eq!(image.dimensions(), (3, 5));
    assert_eq!(image.get_pixel(0, 0)[0], 3);
}

fn exif(orientation: u16) -> Vec<u8> {
    let mut data = b"II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0".to_vec();
    data.extend(orientation.to_le_bytes());
    data.extend([0; 6]);
    data
}

#[test]
fn all_eight_exif_orientations_are_normalized_before_geometry() {
    let f = Fixture::new();
    let image = pixels(3, 2);
    let expected = [
        vec![1, 2, 3, 4, 5, 6],
        vec![3, 2, 1, 6, 5, 4],
        vec![6, 5, 4, 3, 2, 1],
        vec![4, 5, 6, 1, 2, 3],
        vec![1, 4, 2, 5, 3, 6],
        vec![4, 1, 5, 2, 6, 3],
        vec![6, 3, 5, 2, 4, 1],
        vec![3, 6, 2, 5, 1, 4],
    ];
    for orientation in 1..=8 {
        let mut bytes = Vec::new();
        let mut encoder = image::codecs::png::PngEncoder::new(&mut bytes);
        encoder.set_exif_metadata(exif(orientation)).unwrap();
        encoder
            .write_image(image.as_raw(), 3, 2, image::ExtendedColorType::Rgba8)
            .unwrap();
        let path = f.dir.path().join("oriented.png");
        disk::write(&path, &bytes).unwrap();
        let file = f.store.import("session", &path, None).unwrap();
        assert_eq!((file.image.width, file.image.height), (3, 2));
        assert_eq!(f.store.resolve("session", &file).unwrap(), bytes);
        let (result, actual) = transformed(&f, &file, Transform::Rotate { degrees: 180 });
        let mut want = expected[orientation as usize - 1].clone();
        want.reverse();
        assert_eq!(reds(&actual), want, "EXIF {orientation}");
        assert_eq!(
            actual.dimensions(),
            if orientation >= 5 { (2, 3) } else { (3, 2) }
        );
        let out = f.store.resolve("session", &result).unwrap();
        assert!(!out.windows(4).any(|w| w == b"eXIf"));
        assert_eq!(out[24], 8); // RGBA8, not the source's color type.
        assert_eq!(out[25], 6);
    }
}

#[test]
fn fit_rounding_upscale_and_triangle_filter() {
    let f = Fixture::new();
    let image = pixels(5, 3);
    let file = import_pixels(&f, &image);
    let (_, contain) = transformed(
        &f,
        &file,
        Transform::Resize {
            width: 8,
            height: 8,
            fit: Fit::Contain,
        },
    );
    assert_eq!(contain.dimensions(), (8, 4));
    assert_eq!(
        contain,
        image::imageops::resize(&image, 8, 4, image::imageops::FilterType::Triangle)
    );
    let (_, cover) = transformed(
        &f,
        &file,
        Transform::Resize {
            width: 4,
            height: 5,
            fit: Fit::Cover,
        },
    );
    let center = image::imageops::crop_imm(&image, 1, 0, 2, 3).to_image();
    assert_eq!(
        cover,
        image::imageops::resize(&center, 4, 5, image::imageops::FilterType::Triangle)
    );
    let (_, stretch) = transformed(
        &f,
        &file,
        Transform::Resize {
            width: 8,
            height: 8,
            fit: Fit::Stretch,
        },
    );
    assert_eq!(
        stretch,
        image::imageops::resize(&image, 8, 8, image::imageops::FilterType::Triangle)
    );
    assert_ne!(
        stretch,
        image::imageops::resize(&image, 8, 8, image::imageops::FilterType::Nearest)
    );
}

#[test]
fn invalid_geometry_zero_rounding_extreme_dimensions_and_scratch() {
    let f = Fixture::new();
    let file = import_pixels(&f, &pixels(3, 2));
    for degrees in [0, 1, 89, 360, u32::MAX] {
        assert!(
            f.store
                .transform("session", &file, Transform::Rotate { degrees }, None)
                .is_err()
        );
    }
    for (width, height) in [
        (0, 1),
        (1, 0),
        (8193, 1),
        (1, u32::MAX),
        (8192, 1),
        (1, 8192),
    ] {
        let op = Transform::Crop {
            aspect_ratio: AspectRatio { width, height },
            anchor: Anchor::Center,
        };
        assert!(f.store.transform("session", &file, op, None).is_err());
    }
    for (width, height) in [(0, 1), (1, 0), (8193, 1), (1, u32::MAX), (8192, 8192)] {
        assert!(
            f.store
                .transform(
                    "session",
                    &file,
                    Transform::Resize {
                        width,
                        height,
                        fit: Fit::Stretch
                    },
                    None
                )
                .is_err()
        );
    }
    let wide = import_pixels(&f, &pixels(8192, 1));
    assert!(
        f.store
            .transform(
                "session",
                &wide,
                Transform::Resize {
                    width: 1,
                    height: 8192,
                    fit: Fit::Contain
                },
                None
            )
            .is_err()
    );
    let error = f
        .store
        .transform(
            "session",
            &wide,
            Transform::Resize {
                width: 1,
                height: 8192,
                fit: Fit::Stretch,
            },
            None,
        )
        .unwrap_err();
    assert!(error.contains("scratch"), "{error}");
    let (_, exact) = transformed(
        &f,
        &wide,
        Transform::Resize {
            width: 8192,
            height: 1,
            fit: Fit::Stretch,
        },
    );
    assert_eq!(exact.dimensions(), (8192, 1));
}

#[test]
fn export_exact_original_no_clobber_permissions_and_pre_cancel() {
    let f = Fixture::new();
    let file = import_pixels(&f, &pixels(3, 2));
    let destination = f.dir.path().join("export.png");
    f.store
        .export("session", &file, &destination, None)
        .unwrap();
    assert_eq!(
        disk::read(&destination).unwrap(),
        f.store.resolve("session", &file).unwrap()
    );
    assert!(
        f.store
            .export("session", &file, &destination, None)
            .is_err()
    );
    assert!(
        f.store
            .export("session", &file, f.dir.path(), None)
            .is_err()
    );
    assert!(
        f.store
            .export("session", &file, &f.dir.path().join("missing/out"), None)
            .is_err()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        assert_eq!(
            disk::metadata(&destination).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let dangling = f.dir.path().join("dangling");
        symlink(f.dir.path().join("absent"), &dangling).unwrap();
        assert!(f.store.export("session", &file, &dangling, None).is_err());
        assert!(
            disk::symlink_metadata(dangling)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
    let controller = agentkit_core::CancellationController::new();
    let cancellation = controller.handle().checkpoint();
    controller.interrupt();
    let absent = f.dir.path().join("cancelled");
    assert!(
        f.store
            .export("session", &file, &absent, Some(&cancellation))
            .unwrap_err()
            .contains("cancelled")
    );
    assert!(!absent.exists());
    assert!(
        f.store
            .transform(
                "session",
                &file,
                Transform::Rotate { degrees: 90 },
                Some(&cancellation)
            )
            .unwrap_err()
            .contains("cancelled")
    );
}

#[test]
fn concurrent_exports_have_exactly_one_winner() {
    let f = Fixture::new();
    let file = import_pixels(&f, &pixels(3, 2));
    let path = f.dir.path().join("winner");
    std::thread::scope(|scope| {
        let a = scope.spawn(|| f.store.export("session", &file, &path, None));
        let b = scope.spawn(|| f.store.export("session", &file, &path, None));
        assert_ne!(a.join().unwrap().is_ok(), b.join().unwrap().is_ok());
    });
    assert_eq!(
        disk::read(path).unwrap(),
        f.store.resolve("session", &file).unwrap()
    );
}

#[test]
fn strict_transform_deserialization() {
    assert!(
        serde_json::from_value::<Transform>(json!({"rotate":{"degrees":90,"extra":true}})).is_err()
    );
    assert!(
        serde_json::from_value::<AspectRatio>(json!({"width":1,"height":1,"extra":true})).is_err()
    );
    for name in [
        "center",
        "top_left",
        "top",
        "top_right",
        "left",
        "right",
        "bottom_left",
        "bottom",
        "bottom_right",
    ] {
        assert!(serde_json::from_value::<Anchor>(json!(name)).is_ok());
    }
    for name in ["contain", "cover", "stretch"] {
        assert!(serde_json::from_value::<Fit>(json!(name)).is_ok());
    }
}

fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut result = (data.len() as u32).to_be_bytes().to_vec();
    result.extend(kind);
    result.extend(data);
    let mut crc = !0_u32;
    for byte in &result[4..] {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    result.extend((!crc).to_be_bytes());
    result
}

#[test]
fn strips_text_metadata_and_rejects_expanding_png_ancillary() {
    let f = Fixture::new();
    let source = import_pixels(&f, &pixels(3, 2));
    let original = f.store.resolve("session", &source).unwrap();
    let path = f.dir.path().join("metadata.png");
    let mut bytes = original.clone();
    bytes.splice(33..33, png_chunk(b"tEXt", b"comment\0private information"));
    disk::write(&path, &bytes).unwrap();
    let file = f.store.import("session", &path, None).unwrap();
    assert_eq!(f.store.resolve("session", &file).unwrap(), bytes);
    let (output, _) = transformed(&f, &file, Transform::Rotate { degrees: 90 });
    let out = f.store.resolve("session", &output).unwrap();
    assert!(
        !out.windows(4)
            .any(|w| w == b"tEXt" || w == b"iCCP" || w == b"eXIf")
    );
    for kind in [b"iCCP", b"zTXt", b"iTXt"] {
        let mut bytes = original.clone();
        bytes.splice(33..33, png_chunk(kind, b"not expanded or passed to codec"));
        disk::write(&path, bytes).unwrap();
        assert!(
            f.store
                .import("session", &path, None)
                .unwrap_err()
                .contains("metadata")
        );
    }
}

#[test]
fn high_entropy_jpeg_transform_hits_png_output_cap_without_publication() {
    let f = Fixture::new();
    // The compressed import fits 8 MiB; the lossless transformed PNG does not.
    let mut state = 0x12345678_u32;
    let image = image::RgbImage::from_fn(2304, 1536, |_, _| {
        let mut channels = [0; 3];
        for channel in &mut channels {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *channel = state as u8;
        }
        image::Rgb(channels)
    });
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 85)
        .encode_image(&image)
        .unwrap();
    assert!(bytes.len() as u64 <= MAX_FILE_BYTES);
    let path = f.dir.path().join("noise.jpg");
    disk::write(&path, bytes).unwrap();
    let file = f.store.import("session", &path, None).unwrap();
    let before = disk::read_dir(f.store.session_directory("session"))
        .unwrap()
        .count();
    let error = f
        .store
        .transform("session", &file, Transform::Rotate { degrees: 180 }, None)
        .unwrap_err();
    assert!(error.contains("8 MiB output"), "{error}");
    assert_eq!(
        disk::read_dir(f.store.session_directory("session"))
            .unwrap()
            .count(),
        before
    );
}

#[test]
fn jpeg_orientation_and_icc_are_not_forwarded_and_malformed_exif_is_identity() {
    let f = Fixture::new();
    let image = image::RgbImage::from_fn(7, 5, |x, y| {
        image::Rgb([(x * 35) as u8, (y * 50) as u8, 75])
    });
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 95)
        .encode_image(&image)
        .unwrap();
    let decoded = image::load_from_memory(&jpeg).unwrap().into_rgba8();
    for orientation in [0, 6, 9] {
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend(exif(orientation));
        let app2 = b"ICC_PROFILE\0\x01\x01opaque profile is not color converted";
        let mut bytes = jpeg[..2].to_vec();
        for (marker, data) in [(0xe1, app1.as_slice()), (0xe2, app2.as_slice())] {
            bytes.extend([0xff, marker]);
            bytes.extend(((data.len() + 2) as u16).to_be_bytes());
            bytes.extend(data);
        }
        bytes.extend(&jpeg[2..]);
        let path = f.dir.path().join("metadata.jpg");
        disk::write(&path, &bytes).unwrap();
        let file = f.store.import("session", &path, None).unwrap();
        let (result, actual) = transformed(&f, &file, Transform::Rotate { degrees: 180 });
        let expected = if orientation == 6 {
            image::imageops::rotate270(&decoded)
        } else {
            image::imageops::rotate180(&decoded)
        };
        assert_eq!(actual, expected);
        let out = f.store.resolve("session", &result).unwrap();
        assert!(!out.windows(4).any(|w| w == b"iCCP" || w == b"eXIf"));
        let export = f.dir.path().join(format!("original-{orientation}.jpg"));
        f.store.export("session", &file, &export, None).unwrap();
        assert_eq!(disk::read(export).unwrap(), bytes);
    }
}

#[cfg(unix)]
const EXPORT_FAILURE_PARENT_ENV: &str = "KIT_MANAGED_EXPORT_FAILURE_PARENT_PID";

#[cfg(unix)]
#[test]
fn export_after_create_failure_retains_destination_and_refuses_retry() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "managed_files::tests::operations::export_file_size_limit_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(EXPORT_FAILURE_PARENT_ENV, std::process::id().to_string())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "export failure child failed: {}\n{stdout}\n{stderr}",
        output.status
    );
    assert!(
        stdout.contains("managed_files::tests::operations::export_file_size_limit_child ... ok"),
        "child test did not succeed: {stdout}"
    );
    assert!(
        stdout.contains("1 passed; 0 failed"),
        "child did not run exactly one successful test: {stdout}"
    );
}

#[cfg(unix)]
#[test]
#[ignore = "invoked only by the export failure parent in an isolated Unix process"]
fn export_file_size_limit_child() {
    let parent_pid: u32 = std::env::var(EXPORT_FAILURE_PARENT_ENV)
        .expect("export failure parent PID")
        .parse()
        .unwrap();
    assert_ne!(std::process::id(), parent_pid);
    // SAFETY: getppid takes no arguments and has no memory preconditions.
    assert_eq!(unsafe { libc::getppid() } as u32, parent_pid);

    // Import and resolve before reducing the process-wide file-size limit, so
    // the failure is at the real export write, not fixture publication.
    let f = Fixture::new();
    let file = import_pixels(&f, &pixels(3, 2));
    let original = f.store.resolve("session", &file).unwrap();
    let destination = f.dir.path().join("retained-export.png");
    const LIMIT: libc::rlim_t = 32;
    assert!(original.len() > LIMIT as usize);
    assert!(!destination.exists());
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: limit is a valid writable rlimit. Only this explicitly selected
    // child changes signal disposition and resource limits; the parent does not.
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_FSIZE, &mut limit) },
        0
    );
    assert!(limit.rlim_cur >= LIMIT);
    // SAFETY: SIG_IGN is the OS-defined disposition, not a Rust signal handler.
    assert_ne!(
        unsafe { libc::signal(libc::SIGXFSZ, libc::SIG_IGN) },
        libc::SIG_ERR
    );
    limit.rlim_cur = LIMIT;
    // SAFETY: limit points to an initialized rlimit; the hard limit is unchanged.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &limit) }, 0);

    let error = f
        .store
        .export("session", &file, &destination, None)
        .unwrap_err();
    assert!(error.contains(destination.to_str().unwrap()), "{error}");
    assert!(
        error.contains("partial-or-complete file retained"),
        "{error}"
    );
    assert!(disk::metadata(&destination).unwrap().is_file());
    let retained = disk::read(&destination).unwrap();
    assert_eq!(retained, original[..LIMIT as usize]);

    // The retained partial file is still protected by create_new on retry.
    let retry = f
        .store
        .export("session", &file, &destination, None)
        .unwrap_err();
    assert!(retry.contains(destination.to_str().unwrap()), "{retry}");
    assert!(
        !retry.contains("partial-or-complete file retained"),
        "{retry}"
    );
    assert_eq!(disk::read(&destination).unwrap(), retained);
    // No restoration is needed: this is the only test in the child, which exits.
}
