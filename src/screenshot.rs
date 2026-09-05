use std::{
    fs::{File, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{Context, Result, anyhow};
use ashpd::desktop::screenshot::Screenshot as PortalScreenshot;
use chrono::{SecondsFormat, Utc};
use tokio::io::AsyncReadExt;

const MAX_SCREENSHOT_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct CapturedScreenshot {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub captured_at: String,
}

pub async fn capture() -> Result<CapturedScreenshot> {
    let started = Instant::now();
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        PortalScreenshot::request()
            .interactive(false)
            .modal(false)
            .send(),
    )
    .await
    .map_err(|_| anyhow!("screenshot portal request timed out after 30 seconds"))?
    .context("request screenshot through the XDG desktop portal")?
    .response()
    .context("read screenshot portal response")?;
    let uri = response.uri().as_str().to_owned();
    let path = file_uri_to_path(&uri)?;

    let (bytes, checked_path) = read_and_delete(&path).await?;

    let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
    tracing::info!(
        latency_ms = elapsed_ms,
        path = %checked_path.display(),
        bytes = bytes.len(),
        "screenshot portal request completed with bytes in memory"
    );

    let (width, height) = png_dimensions(&bytes)?;
    Ok(CapturedScreenshot {
        bytes,
        width,
        height,
        captured_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
    })
}

async fn read_and_delete(path: &Path) -> Result<(Vec<u8>, PathBuf)> {
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("refusing screenshot file because HOME is not set"))?;
    let (file, checked_path) = open_checked_file(path, &home)?;
    let mut file = tokio::fs::File::from_std(file);
    let mut bytes = Vec::new();
    let read_result = (&mut file)
        .take(MAX_SCREENSHOT_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .await;
    read_result.with_context(|| format!("read portal screenshot {}", checked_path.display()))?;
    drop(file);

    if bytes.len() as u64 > MAX_SCREENSHOT_BYTES {
        return Err(anyhow!(
            "refusing screenshot file {} because it exceeds the 64 MiB limit",
            checked_path.display()
        ));
    }

    tokio::fs::remove_file(&checked_path)
        .await
        .with_context(|| format!("delete portal screenshot {}", checked_path.display()))?;
    Ok((bytes, checked_path))
}

fn open_checked_file(path: &Path, home: &Path) -> Result<(File, PathBuf)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| {
            format!(
                "refusing screenshot file {}: open with O_NOFOLLOW failed",
                path.display()
            )
        })?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect screenshot file {}", path.display()))?;
    if !metadata.file_type().is_file() {
        return Err(anyhow!(
            "refusing screenshot path {} because it is not a regular file",
            path.display()
        ));
    }
    let current_euid = unsafe { libc::geteuid() } as u32;
    if metadata.uid() != current_euid {
        return Err(anyhow!(
            "refusing screenshot file {} because its owner uid {} does not match effective uid {}",
            path.display(),
            metadata.uid(),
            current_euid
        ));
    }
    if metadata.len() > MAX_SCREENSHOT_BYTES {
        return Err(anyhow!(
            "refusing screenshot file {} because it exceeds the 64 MiB limit",
            path.display()
        ));
    }

    let canonical_home = home
        .canonicalize()
        .with_context(|| format!("resolve HOME directory {}", home.display()))?;
    let canonical_path = path
        .canonicalize()
        .with_context(|| format!("resolve screenshot path {}", path.display()))?;
    if !canonical_path.starts_with(&canonical_home) {
        return Err(anyhow!(
            "refusing screenshot file {} because its canonical path is outside HOME {}",
            canonical_path.display(),
            canonical_home.display()
        ));
    }

    Ok((file, canonical_path))
}

fn png_dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let reader = decoder
        .read_info()
        .context("decode screenshot PNG header")?;
    let info = reader.info();
    Ok((info.width, info.height))
}

fn file_uri_to_path(uri: &str) -> Result<PathBuf> {
    let rest = uri
        .strip_prefix("file://")
        .ok_or_else(|| anyhow!("screenshot portal returned a non-file URI: {uri}"))?;
    let path_part = if rest.starts_with('/') {
        rest.to_owned()
    } else {
        let (host, path) = rest
            .split_once('/')
            .ok_or_else(|| anyhow!("screenshot portal returned an invalid file URI: {uri}"))?;
        if host != "localhost" {
            return Err(anyhow!(
                "refusing screenshot URI for unexpected host {host:?}"
            ));
        }
        format!("/{path}")
    };
    let path = percent_decode(&path_part)?;
    if !path.starts_with('/') {
        return Err(anyhow!(
            "screenshot portal file URI did not resolve to an absolute path"
        ));
    }
    Ok(PathBuf::from(path))
}

fn percent_decode(input: &str) -> Result<String> {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(anyhow!("invalid percent escape in screenshot URI"));
            }
            let high = hex_value(bytes[index + 1])
                .ok_or_else(|| anyhow!("invalid percent escape in screenshot URI"))?;
            let low = hex_value(bytes[index + 2])
                .ok_or_else(|| anyhow!("invalid percent escape in screenshot URI"))?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).context("screenshot URI path is not valid UTF-8")
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{self, File},
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{MAX_SCREENSHOT_BYTES, file_uri_to_path, open_checked_file};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("wayhand-mcp-screenshot-{nonce}"));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn decodes_local_file_uri() {
        assert_eq!(
            file_uri_to_path("file:///tmp/shot%20one.png").unwrap(),
            Path::new("/tmp/shot one.png")
        );
    }

    #[test]
    fn rejects_non_local_or_non_file_uri() {
        assert!(file_uri_to_path("https://example.test/shot.png").is_err());
        assert!(file_uri_to_path("file://other-host/tmp/shot.png").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn path_validation_rejects_symlink() {
        let directory = TestDirectory::new();
        let target = directory.path().join("target.png");
        File::create(&target).unwrap();
        let symlink = directory.path().join("link.png");
        std::os::unix::fs::symlink(&target, &symlink).unwrap();

        assert!(open_checked_file(&symlink, directory.path()).is_err());
        assert!(symlink.exists() || symlink.symlink_metadata().is_ok());
        assert!(target.exists());
    }

    #[test]
    fn path_validation_rejects_directory() {
        let directory = TestDirectory::new();
        let nested = directory.path().join("nested");
        fs::create_dir(&nested).unwrap();

        let error = open_checked_file(&nested, directory.path()).unwrap_err();
        assert!(error.to_string().contains("not a regular file"));
    }

    #[test]
    fn path_validation_rejects_oversize_file() {
        let directory = TestDirectory::new();
        let path = directory.path().join("large.png");
        let file = File::create(&path).unwrap();
        file.set_len(MAX_SCREENSHOT_BYTES + 1).unwrap();

        let error = open_checked_file(&path, directory.path()).unwrap_err();
        assert!(error.to_string().contains("64 MiB"));
    }
}
