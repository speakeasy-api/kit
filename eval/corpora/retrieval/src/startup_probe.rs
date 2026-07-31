use crate::{ProtocolError, Result};
use serde::Deserialize;
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

const RESPONSE: &[u8] = b"{\"arbitrary_home_denied\":true,\"network_denied\":true,\"oracle_denied\":true,\"reached_main\":true}\n";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartupProbeRequest {
    arbitrary_home_file: PathBuf,
    oracle_file: PathBuf,
}

pub fn run_worker_startup_probe(request_root: &Path, output_root: &Path) -> Result<()> {
    if !request_root.is_absolute()
        || !output_root.is_absolute()
        || request_root.starts_with(output_root)
        || output_root.starts_with(request_root)
    {
        return Err(ProtocolError("invalid startup probe roots".into()).into());
    }
    let request_path = request_root.join("startup-probe.json");
    let mut request_file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&request_path)?;
    let metadata = request_file.metadata()?;
    if !metadata.file_type().is_file() || metadata.len() == 0 || metadata.len() > 4096 {
        return Err(ProtocolError("invalid bounded startup probe request".into()).into());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut request_file)
        .take(4097)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 != metadata.len() {
        return Err(ProtocolError("startup probe request changed while read".into()).into());
    }
    let request: StartupProbeRequest = serde_json::from_slice(&bytes)?;
    if !request.arbitrary_home_file.is_absolute()
        || !request.oracle_file.is_absolute()
        || std::env::var_os("HOME").as_deref() != Some(output_root.as_os_str())
        || !permission_denied(fs::File::open(&request.arbitrary_home_file))
        || !permission_denied(fs::File::open(&request.oracle_file))
        || !permission_denied(TcpListener::bind(("127.0.0.1", 0)))
    {
        return Err(ProtocolError("startup probe isolation check failed".into()).into());
    }
    let output = output_root.join("startup-probe.json");
    let mut output = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(output)?;
    output.write_all(RESPONSE)?;
    output.sync_all()?;
    Ok(())
}

fn permission_denied<T>(result: std::io::Result<T>) -> bool {
    result.is_err_and(|error| {
        error.kind() == std::io::ErrorKind::PermissionDenied
            || error
                .raw_os_error()
                .is_some_and(|code| code == libc::EACCES || code == libc::EPERM)
    })
}
