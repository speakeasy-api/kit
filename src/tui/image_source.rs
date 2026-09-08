//! Presentation-only image loading. This never grants managed-file authority.
use std::{
    collections::HashSet,
    io::Read as _,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use url::Url;

// The permit is also owned by every blocking stage. Dropping a presentation
// runtime cannot release admission while its file read or DNS lookup still runs.
static SOURCE_WORKERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
type SourcePermit = Arc<tokio::sync::SemaphorePermit<'static>>;
const MAX_BYTES: u64 = 8 * 1024 * 1024;
const TOTAL_TIMEOUT: Duration = Duration::from_secs(20);
const IO_TIMEOUT: Duration = Duration::from_secs(5);
type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Debug, Default)]
pub(super) struct ImagePolicy {
    origins: HashSet<String>,
}

impl ImagePolicy {
    pub(super) fn from_environment() -> Self {
        Self::parse(&std::env::var("KIT_TUI_IMAGE_ORIGINS").unwrap_or_default())
    }

    fn parse(value: &str) -> Self {
        Self {
            origins: value
                .split(',')
                .take(64)
                .filter_map(|entry| {
                    let entry = entry.trim();
                    if entry.len() > 2048 {
                        return None;
                    }
                    let authority = entry.strip_prefix("https://")?;
                    let authority = authority.strip_suffix('/').unwrap_or(authority);
                    if authority.is_empty()
                        || authority.contains(['/', '?', '#', '@', '\\'])
                        || authority.chars().any(char::is_whitespace)
                    {
                        return None;
                    }
                    let url = Url::parse(entry).ok()?;
                    (https_url(&url)
                        && url.path() == "/"
                        && url.query().is_none()
                        && url.fragment().is_none())
                    .then(|| url.origin().ascii_serialization())
                })
                .collect(),
        }
    }

    fn permits(&self, url: &Url) -> bool {
        https_url(url) && self.origins.contains(&url.origin().ascii_serialization())
    }
}

fn https_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.port_or_known_default() == Some(443)
        && url.fragment().is_none()
}

/// Caller bounds concurrent workers and discards cancelled/stale publications.
pub(super) async fn resolve(
    source: &str,
    root: &Path,
    session: &str,
    policy: &ImagePolicy,
) -> Result<(Vec<u8>, String)> {
    if source.len() > 4096 || source.starts_with("//") || source.starts_with("\\\\") {
        return Err("invalid image source".into());
    }
    let permit = Arc::new(
        tokio::time::timeout(IO_TIMEOUT, SOURCE_WORKERS.acquire())
            .await
            .map_err(|_| "image workers busy".to_owned())?
            .map_err(|_| "image workers unavailable".to_owned())?,
    );
    if let Some(id) = source.strip_prefix("kit-file://") {
        let store = crate::managed_files::FileStore::new(root);
        let session = session.to_owned();
        let id = id.to_owned();
        return tokio::task::spawn_blocking(move || {
            let _permit = permit;
            store
                .resolve_id(&session, &id)
                .map_err(|_| "managed image unavailable".to_owned())
        })
        .await
        .map_err(|_| "image worker unavailable".to_owned())?;
    }
    if source.split_once("://").is_some_and(|(_, rest)| {
        rest.split(['/', '?', '#'])
            .next()
            .is_some_and(|authority| authority.contains('@'))
            || source.contains('\\')
            || source.chars().any(char::is_control)
    }) {
        return Err("invalid image URL".into());
    }
    let bytes = if Path::new(source).is_absolute() {
        local(native_path(source)?, permit.clone()).await?
    } else if let Ok(url) = Url::parse(source) {
        match url.scheme() {
            "https" => {
                if !policy.permits(&url) {
                    return Err("image origin not allowed".into());
                }
                tokio::time::timeout(TOTAL_TIMEOUT, remote(url, permit.clone()))
                    .await
                    .map_err(|_| "image request timed out".to_owned())??
            }
            "file" => {
                if !url.username().is_empty()
                    || url.password().is_some()
                    || url.port().is_some()
                    || url.query().is_some()
                    || url.fragment().is_some()
                    || !matches!(url.host_str(), None | Some("localhost"))
                {
                    return Err("invalid local image URL".into());
                }
                local(
                    url.to_file_path()
                        .map_err(|_| "invalid local image path".to_owned())?,
                    permit.clone(),
                )
                .await?
            }
            _ => return Err("unsupported image source".into()),
        }
    } else {
        local(root.join(native_path(source)?), permit.clone()).await?
    };
    // Sniff only; all origins share the runtime's single bounded decoder.
    let format = image::guess_format(&bytes).map_err(|_| "invalid image format".to_owned())?;
    if !matches!(
        format,
        image::ImageFormat::Png
            | image::ImageFormat::Jpeg
            | image::ImageFormat::Gif
            | image::ImageFormat::WebP
    ) {
        return Err("unsupported image format".into());
    }
    Ok((bytes, format.to_mime_type().to_owned()))
}

// Markdown destinations retain URL escapes. Decode only native destinations;
// Url::to_file_path already decodes file URLs. The caller caps input at 4096 bytes.
fn native_path(source: &str) -> Result<PathBuf> {
    let mut decoded = Vec::with_capacity(source.len());
    let mut bytes = source.bytes();
    while let Some(byte) = bytes.next() {
        decoded.push(if byte == b'%' {
            let high = bytes.next().and_then(|b| char::from(b).to_digit(16));
            let low = bytes.next().and_then(|b| char::from(b).to_digit(16));
            match (high, low) {
                (Some(high), Some(low)) => (high * 16 + low) as u8,
                _ => return Err("invalid local image path".into()),
            }
        } else {
            byte
        });
    }
    // Reject malformed UTF-8 rather than opening a lossy replacement filename.
    let decoded = String::from_utf8(decoded).map_err(|_| "invalid local image path".to_owned())?;
    let path = PathBuf::from(decoded);
    validate_local_path(&path)?;
    Ok(path)
}

fn validate_local_path(path: &Path) -> Result<()> {
    let bytes = path.as_os_str().as_encoded_bytes();
    // Check after decoding as well as before it, including mixed separators.
    // Do not reinterpret native paths as URLs or change their OS path semantics.
    if bytes.contains(&0) {
        return Err("invalid local image path".into());
    }
    if bytes.len() >= 2 && matches!(bytes[0], b'/' | b'\\') && matches!(bytes[1], b'/' | b'\\') {
        return Err("invalid image source".into());
    }
    Ok(())
}

async fn local(path: PathBuf, permit: SourcePermit) -> Result<Vec<u8>> {
    validate_local_path(&path)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        // Open first, then inspect the actual handle. Symlinks are allowed, but
        // a FIFO must not block this worker before its type can be checked.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NONBLOCK);
        }
        let file = options
            .open(path)
            .map_err(|_| "local image unavailable".to_owned())?;
        let metadata = file
            .metadata()
            .map_err(|_| "local image unavailable".to_owned())?;
        if !metadata.is_file() || metadata.len() > MAX_BYTES {
            return Err("invalid or oversized local image".into());
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "local image read failed".to_owned())?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err("image too large".into());
        }
        Ok(bytes)
    })
    .await
    .map_err(|_| "image worker unavailable".to_owned())?
}

async fn remote(url: Url, permit: SourcePermit) -> Result<Vec<u8>> {
    let host = url
        .host_str()
        .ok_or_else(|| "invalid image host".to_owned())?;
    let addresses: Vec<SocketAddr> = match url.host() {
        Some(url::Host::Ipv4(ip)) => vec![SocketAddr::new(ip.into(), 443)],
        Some(url::Host::Ipv6(ip)) => vec![SocketAddr::new(ip.into(), 443)],
        _ => {
            let host = host.to_owned();
            // DNS is blocking and cannot be cancelled. Its actual closure owns
            // admission until completion/unwind, even if either timeout fires
            // or the awaiting presentation task is dropped.
            let lookup = tokio::task::spawn_blocking(move || {
                use std::net::ToSocketAddrs as _;
                let _permit = permit;
                (host.as_str(), 443)
                    .to_socket_addrs()
                    .map(|addresses| addresses.take(17).collect::<Vec<_>>())
            });
            tokio::time::timeout(IO_TIMEOUT, lookup)
                .await
                .map_err(|_| "image DNS timed out".to_owned())?
                .map_err(|_| "image DNS worker unavailable".to_owned())?
                .map_err(|_| "image DNS failed".to_owned())?
        }
    };
    if addresses.is_empty()
        || addresses.len() > 16
        || addresses.iter().any(|addr| !public_address(addr.ip()))
    {
        return Err("image address not public".into());
    }
    // A fresh client has no cookie jar or credential defaults. Disabling proxy
    // discovery and redirects prevents bypasses of the checked, pinned DNS set.
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(IO_TIMEOUT)
        .read_timeout(IO_TIMEOUT)
        .timeout(TOTAL_TIMEOUT)
        .resolve_to_addrs(host, &addresses)
        .build()
        .map_err(|_| "image client unavailable".to_owned())?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|_| "image request failed".to_owned())?;
    read_response(response).await
}

async fn read_response(mut response: reqwest::Response) -> Result<Vec<u8>> {
    if response.status().is_redirection() {
        return Err("image redirect rejected".into());
    }
    if !response.status().is_success() {
        return Err("image request rejected".into());
    }
    if response
        .content_length()
        .is_some_and(|size| size > MAX_BYTES)
    {
        return Err("image too large".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "image read failed".to_owned())?
    {
        if chunk.len() as u64 > MAX_BYTES - bytes.len() as u64 {
            return Err("image too large".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn public_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => public_v6(ip),
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    // Fail closed on special-purpose blocks, including shared space, protocol
    // assignments, documentation, benchmarking, multicast and future use.
    !(a == 0
        || a == 10
        || a == 127
        || a >= 224
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && (b == 0 || (b == 88 && c == 99) || b == 168))
        || (a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
        || (a == 203 && b == 0 && c == 113))
}

fn public_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    // Only ordinary global unicast, excluding IETF special assignments,
    // documentation, 6to4 and deprecated 6bone. This also rejects all mapped,
    // translated, local, multicast and unspecified address representations.
    (s[0] & 0xe000) == 0x2000
        && !(s[0] == 0x2001 && (s[1] <= 0x01ff || s[1] == 0x0db8))
        && s[0] != 0x2002
        && s[0] != 0x3ffe
        && !(s[0] == 0x3fff && s[1] <= 0x0fff)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn png() -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 3)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    #[tokio::test]
    #[ignore = "explicit external HTTPS smoke; requires public Internet and httpbin.org"]
    async fn authorized_https_load_and_redirect_rejection() {
        let root = tempfile::tempdir().unwrap();
        let policy = ImagePolicy::parse("https://httpbin.org");
        let (bytes, mime) = resolve(
            "https://httpbin.org/image/png",
            root.path(),
            "smoke",
            &policy,
        )
        .await
        .unwrap();
        assert_eq!(mime, "image/png");
        assert!(image::load_from_memory(&bytes).is_ok());
        let error = resolve(
            "https://httpbin.org/redirect-to?url=https%3A%2F%2Fhttpbin.org%2Fimage%2Fpng",
            root.path(),
            "smoke",
            &policy,
        )
        .await
        .unwrap_err();
        assert_eq!(error, "image redirect rejected");
    }

    #[test]
    fn origins_are_exact_and_fail_closed() {
        let policy = ImagePolicy::parse("https://example.com, https://other.example:443/");
        assert!(policy.permits(&Url::parse("https://example.com/image?q=1").unwrap()));
        for url in [
            "http://example.com/a",
            "https://sub.example.com/a",
            "https://example.com:444/a",
            "https://user@example.com/a",
            "https://example.com/a#fragment",
        ] {
            assert!(!policy.permits(&Url::parse(url).unwrap()), "{url}");
        }
        for origin in [
            "",
            "https://example.com/path",
            "https://example.com/path/..",
            "https://example.com?",
            "https://example.com#",
            "https://@example.com",
            "https://example.com:444",
            "http://example.com",
            "https://example.com\\",
        ] {
            assert!(ImagePolicy::parse(origin).origins.is_empty(), "{origin}");
        }
    }

    #[test]
    fn origin_count_and_length_are_capped() {
        let origins = (0..65)
            .map(|index| format!("https://host{index}.example"))
            .collect::<Vec<_>>();
        let policy = ImagePolicy::parse(&origins.join(","));
        assert_eq!(policy.origins.len(), 64);
        assert!(policy.permits(&Url::parse(&origins[63]).unwrap()));
        assert!(!policy.permits(&Url::parse(&origins[64]).unwrap()));
        let overlong = format!("https://{}", "a".repeat(2041));
        assert_eq!(overlong.len(), 2049);
        assert!(ImagePolicy::parse(&overlong).origins.is_empty());
        // A rejected entry does not grant authority or hide a later valid one.
        let policy = ImagePolicy::parse(&format!("{overlong},https://allowed.example"));
        assert_eq!(policy.origins.len(), 1);
        assert!(policy.permits(&Url::parse("https://allowed.example/image").unwrap()));
    }

    #[tokio::test]
    async fn rejects_overlong_unc_and_nonlocal_file_sources() {
        let dir = tempfile::tempdir().unwrap();
        let policy = ImagePolicy::default();
        for source in [
            "a".repeat(4097),
            "//server/share/image.png".into(),
            "\\\\server\\share\\image.png".into(),
        ] {
            assert_eq!(
                resolve(&source, dir.path(), "s", &policy)
                    .await
                    .unwrap_err(),
                "invalid image source"
            );
        }
        for source in [
            "file://server/share/image.png",
            "file://127.0.0.1/image.png",
            "file://localhost/image.png?secret=1",
            "file://localhost/image.png#fragment",
        ] {
            assert_eq!(
                resolve(source, dir.path(), "s", &policy).await.unwrap_err(),
                "invalid local image URL"
            );
        }
        let path = dir.path().join("image.png");
        let bytes = png();
        std::fs::write(&path, &bytes).unwrap();
        let url = Url::from_file_path(&path).unwrap();
        let localhost = format!("file://localhost{}", url.path());
        assert_eq!(
            resolve(&localhost, dir.path(), "s", &policy).await.unwrap(),
            (bytes, "image/png".into())
        );
    }

    #[tokio::test]
    async fn acquisition_sniffs_magic_without_claiming_pixel_validation() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        std::fs::write(dir.path().join("truncated.png"), &bytes).unwrap();
        // Acquisition accepts supported magic only. The common runtime decoder
        // must reject this missing image header before publishing pixels.
        assert_eq!(
            resolve("truncated.png", dir.path(), "s", &ImagePolicy::default())
                .await
                .unwrap(),
            (bytes, "image/png".into())
        );
    }

    #[test]
    fn rejects_special_addresses_in_both_families() {
        for ip in [
            "0.0.0.0",
            "10.1.2.3",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.168.1.1",
            "192.0.2.1",
            "192.88.99.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "fc00::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
            "2001:20::1",
            "2002:808:808::1",
            "3fff::1",
        ] {
            assert!(!public_address(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "8.8.8.8",
            "1.1.1.1",
            "2606:4700:4700::1111",
            "2001:4860:4860::8888",
        ] {
            assert!(public_address(ip.parse().unwrap()), "{ip}");
        }
    }

    #[tokio::test]
    async fn allowed_origins_still_cannot_fetch_nonpublic_hosts() {
        for origin in [
            "https://127.0.0.1",
            "https://[::1]",
            "https://[::ffff:8.8.8.8]",
            "https://localhost",
        ] {
            let policy = ImagePolicy::parse(origin);
            assert_eq!(
                resolve(origin, Path::new("."), "s", &policy)
                    .await
                    .unwrap_err(),
                "image address not public"
            );
        }
    }

    #[tokio::test]
    async fn local_bytes_are_bounded_and_sniffed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("picture.png");
        let bytes = png();
        std::fs::write(&path, &bytes).unwrap();
        let policy = ImagePolicy::default();
        for source in [
            "picture.png".to_owned(),
            path.to_str().unwrap().to_owned(),
            Url::from_file_path(&path).unwrap().to_string(),
        ] {
            assert_eq!(
                resolve(&source, dir.path(), "session", &policy)
                    .await
                    .unwrap(),
                (bytes.clone(), "image/png".into())
            );
        }
        assert!(resolve(".", dir.path(), "s", &policy).await.is_err());
        std::fs::write(&path, b"not an image").unwrap();
        assert_eq!(
            resolve("picture.png", dir.path(), "s", &policy)
                .await
                .unwrap_err(),
            "invalid image format"
        );
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_BYTES + 1)
            .unwrap();
        assert!(
            resolve("picture.png", dir.path(), "s", &policy)
                .await
                .is_err()
        );
        assert!(
            resolve(
                "https://example.com/private?token=secret",
                dir.path(),
                "s",
                &policy
            )
            .await
            .unwrap_err()
                == "image origin not allowed"
        );
    }

    #[tokio::test]
    async fn native_markdown_destinations_decode_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("screenshots")).unwrap();
        let policy = ImagePolicy::default();
        let fixtures = [
            ("run%201.png", "run 1.png"),
            ("run%25201.png", "run%201.png"),
            ("literal%25.png", "literal%.png"),
            ("caf%C3%A9.png", "café.png"),
        ];
        // Distinct valid images prove we open the decoded file, not a literal
        // %20 filename or a twice-decoded space filename that also exists.
        for (index, (_, filename)) in fixtures.iter().enumerate() {
            let mut bytes = png();
            bytes.push(index as u8);
            std::fs::write(dir.path().join("screenshots").join(filename), bytes).unwrap();
        }
        for (index, (destination, filename)) in fixtures.iter().enumerate() {
            let mut expected = png();
            expected.push(index as u8);
            let relative = format!("screenshots/{destination}");
            let absolute = format!(
                "{}/{relative}",
                dir.path()
                    .to_str()
                    .unwrap()
                    .replace('%', "%25")
                    .replace(' ', "%20")
            );
            let file_url = Url::from_file_path(dir.path().join("screenshots").join(filename))
                .unwrap()
                .to_string();
            for source in [relative, absolute, file_url] {
                assert_eq!(
                    resolve(&source, dir.path(), "s", &policy).await.unwrap(),
                    (expected.clone(), "image/png".into()),
                    "{source}"
                );
            }
        }
    }

    #[tokio::test]
    async fn malformed_native_escapes_fail_without_lossy_or_literal_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let policy = ImagePolicy::default();
        for filename in [
            "bad%.png",
            "bad%2.png",
            "bad%GG.png",
            "bad%FF.png",
            "bad%C3%28.png",
            "bad%00.png",
        ] {
            std::fs::write(dir.path().join(filename), png()).unwrap();
            for source in [
                filename.to_owned(),
                dir.path().join(filename).to_str().unwrap().to_owned(),
            ] {
                assert_eq!(
                    resolve(&source, dir.path(), "s", &policy)
                        .await
                        .unwrap_err(),
                    "invalid local image path",
                    "{source}"
                );
            }
        }
        std::fs::write(dir.path().join("bad�.png"), png()).unwrap();
        assert_eq!(
            resolve("bad%EF%BF.png", dir.path(), "s", &policy)
                .await
                .unwrap_err(),
            "invalid local image path"
        );
    }

    #[tokio::test]
    async fn decoded_local_destinations_cannot_introduce_unc_paths() {
        let dir = tempfile::tempdir().unwrap();
        let policy = ImagePolicy::default();
        for source in [
            "%2f%2fserver/share/image.png",
            "/%2Fserver/share/image.png",
            "%5c%5cserver%5cshare%5cimage.png",
            "%2f%5cserver/share/image.png",
            "%5c%2fserver/share/image.png",
            "file:///%2Fserver/share/image.png",
            "file://localhost/%2Fserver/share/image.png",
        ] {
            let error = resolve(source, dir.path(), "s", &policy).await.unwrap_err();
            assert!(
                matches!(
                    error.as_str(),
                    "invalid image source" | "invalid local image path"
                ),
                "{source}: {error}"
            );
        }
        for source in [
            "file://%73erver/share/image.png",
            "file://%31%32%37.0.0.1/image.png",
        ] {
            assert_eq!(
                resolve(source, dir.path(), "s", &policy).await.unwrap_err(),
                "invalid local image URL"
            );
        }
    }

    #[tokio::test]
    async fn managed_source_preserves_session_isolation_without_reimport() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("image.png");
        let bytes = png();
        std::fs::write(&source, &bytes).unwrap();
        let store = crate::managed_files::FileStore::new(dir.path());
        let reference = store.import("owner", &source, None).unwrap();
        let descriptor = serde_json::to_value(reference).unwrap();
        let id = descriptor["id"].as_str().unwrap();
        let uri = format!("kit-file://{id}");
        std::fs::remove_file(&source).unwrap();
        assert_eq!(
            resolve(&uri, dir.path(), "owner", &ImagePolicy::default())
                .await
                .unwrap(),
            (bytes, "image/png".into())
        );
        assert_eq!(
            resolve(&uri, dir.path(), "other", &ImagePolicy::default())
                .await
                .unwrap_err(),
            "managed image unavailable"
        );
        for suffix in ["/", "?x=1", "#fragment"] {
            assert!(
                resolve(
                    &format!("{uri}{suffix}"),
                    dir.path(),
                    "owner",
                    &ImagePolicy::default()
                )
                .await
                .is_err()
            );
        }
    }

    #[tokio::test]
    async fn actual_streamed_body_is_bounded_without_content_length() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 4096];
            let _ = stream.read(&mut buffer).await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            let chunk = vec![0; 1024 * 1024];
            for _ in 0..9 {
                if stream.write_all(b"100000\r\n").await.is_err() {
                    return;
                }
                if stream.write_all(&chunk).await.is_err() {
                    return;
                }
                if stream.write_all(b"\r\n").await.is_err() {
                    return;
                }
            }
            let _ = stream.write_all(b"0\r\n\r\n").await;
        });
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.content_length(), None);
        assert_eq!(
            read_response(response).await.unwrap_err(),
            "image too large"
        );
        server.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_allowed_fifo_and_permissions_rejected() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image");
        std::fs::write(&path, png()).unwrap();
        symlink(&path, dir.path().join("link")).unwrap();
        assert!(
            resolve("link", dir.path(), "s", &ImagePolicy::default())
                .await
                .is_ok()
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o0)).unwrap();
        // Root can read mode-000 files; assert only when the OS denies access.
        if std::fs::File::open(&path).is_err() {
            assert!(
                resolve("image", dir.path(), "s", &ImagePolicy::default())
                    .await
                    .is_err()
            );
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let fifo = dir.path().join("fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            resolve("fifo", dir.path(), "s", &ImagePolicy::default())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn actual_http_redirect_response_is_rejected_without_following() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 4096];
            let _ = stream.read(&mut buffer).await.unwrap();
            stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1/private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        });
        // Exercise the real HTTP response boundary; production still only
        // permits HTTPS/public DNS, with no test bypass in its network path.
        let response = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            read_response(response).await.unwrap_err(),
            "image redirect rejected"
        );
        server.await.unwrap();
    }
}
