use crate::logging::{log, LogLevel, LogSource, Logger};
use crate::proxy::{ErrorCode, ProxyError};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tauri::{AppHandle, Manager};

pub struct Installer;

impl Installer {
    pub fn binary_name() -> &'static str {
        #[cfg(target_os = "windows")]
        {
            "sing-box.exe"
        }
        #[cfg(not(target_os = "windows"))]
        {
            "sing-box"
        }
    }

    pub fn is_installed(bin_dir: &Path) -> bool {
        let binary_path = bin_dir.join(Self::binary_name());
        binary_path.exists()
    }

    // ── sing-box installation ──────────────────────────────────────────────

    pub async fn install(
        app_handle: &AppHandle,
        bin_dir: &PathBuf,
        version: &str,
    ) -> Result<PathBuf, ProxyError> {
        let logger = app_handle.state::<Mutex<Logger>>();
        let url = Self::download_url(version);
        let suffix = Self::platform_suffix();
        let binary_name = Self::binary_name();

        std::fs::create_dir_all(bin_dir)?;

        log(
            app_handle,
            &logger,
            LogLevel::Info,
            LogSource::Installer,
            format!("Downloading sing-box {version}..."),
        );
        // Try direct GitHub download, with ghproxy.net fallback for blocked regions
        let bytes = Self::download_with_fallback(&url).await?;

        log(
            app_handle,
            &logger,
            LogLevel::Info,
            LogSource::Installer,
            format!("Downloaded {size} bytes, extracting...", size = bytes.len()),
        );

        if suffix.ends_with(".zip") {
            Self::extract_zip(&bytes, bin_dir, binary_name)?;
        } else {
            Self::extract_tar_gz(&bytes, bin_dir, binary_name)?;
        }

        let binary_path = bin_dir.join(binary_name);

        #[cfg(not(target_os = "windows"))]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&binary_path, std::fs::Permissions::from_mode(0o755))?;
        }

        log(
            app_handle,
            &logger,
            LogLevel::Success,
            LogSource::Installer,
            format!("sing-box {version} installed"),
        );

        Ok(binary_path)
    }

    pub async fn latest_version(app_handle: &AppHandle) -> Result<String, ProxyError> {
        let logger = app_handle.state::<Mutex<Logger>>();
        log(
            app_handle,
            &logger,
            LogLevel::Info,
            LogSource::Installer,
            "Checking latest sing-box version...".into(),
        );

        let client = reqwest::Client::new();
        let resp = client
            .get("https://api.github.com/repos/SagerNet/sing-box/releases/latest")
            .header("User-Agent", "omnitun/0.1.0")
            .header("Accept", "application/json")
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => match r.json::<serde_json::Value>().await {
                Ok(json) => {
                    if let Some(tag) = json["tag_name"].as_str() {
                        let version = tag.trim_start_matches('v').to_string();
                        log(
                            app_handle,
                            &logger,
                            LogLevel::Info,
                            LogSource::Installer,
                            format!("Latest version: {version}"),
                        );
                        return Ok(version);
                    }
                }
                Err(e) => {
                    log(
                        app_handle,
                        &logger,
                        LogLevel::Error,
                        LogSource::Installer,
                        format!("Failed to parse GitHub response: {e}"),
                    );
                }
            },
            Ok(r) => {
                log(
                    app_handle,
                    &logger,
                    LogLevel::Warning,
                    LogSource::Installer,
                    format!("GitHub API returned {}", r.status()),
                );
            }
            Err(e) => {
                log(
                    app_handle,
                    &logger,
                    LogLevel::Error,
                    LogSource::Installer,
                    format!("GitHub API unreachable (blocked?): {e}"),
                );
            }
        }

        // Fallback: GitHub API blocked/unreachable (China, Iran, Russia)
        log(
            app_handle,
            &logger,
            LogLevel::Warning,
            LogSource::Installer,
            "GitHub API failed, using fallback version 1.12.0".into(),
        );
        // ⚠️ 1.12.0 minimum — config uses type-based DNS format (introduced in 1.12)
        // Older sing-box versions will fail to parse the generated config.
        Ok("1.12.0".to_string())
    }

    // ── ZIP / tar.gz extraction ────────────────────────────────────────────

    fn extract_zip(bytes: &[u8], bin_dir: &Path, binary_name: &str) -> Result<(), ProxyError> {
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| {
            ProxyError::install(
                ErrorCode::InstallExtractFailed,
                format!("Failed to open zip: {e}"),
            )
        })?;

        let mut found_binary = false;
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).map_err(|e| {
                ProxyError::install(
                    ErrorCode::InstallExtractFailed,
                    format!("Failed to read zip entry: {e}"),
                )
            })?;

            if entry.is_dir() {
                continue;
            }

            let entry_name = entry.name().to_string();
            // Flatten directory structure — extract every file directly into bin_dir
            let file_name = std::path::Path::new(&entry_name)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            if file_name.is_empty() {
                continue;
            }

            if file_name == binary_name || file_name == "sing-box" {
                found_binary = true;
            }

            let out_path = bin_dir.join(&file_name);
            let mut out = std::fs::File::create(&out_path).map_err(|e| {
                ProxyError::install(
                    ErrorCode::InstallExtractFailed,
                    format!("Failed to create {file_name}: {e}"),
                )
            })?;
            std::io::copy(&mut entry, &mut out).map_err(|e| {
                ProxyError::install(
                    ErrorCode::InstallExtractFailed,
                    format!("Failed to extract {file_name}: {e}"),
                )
            })?;
        }

        if found_binary {
            Ok(())
        } else {
            Err(ProxyError::install(
                ErrorCode::InstallBinaryNotFound,
                "Binary not found in zip archive",
            ))
        }
    }

    fn extract_tar_gz(bytes: &[u8], bin_dir: &Path, binary_name: &str) -> Result<(), ProxyError> {
        let decompressed = flate2::read::GzDecoder::new(bytes);
        let mut archive = tar::Archive::new(decompressed);

        for entry in archive.entries().map_err(|e| {
            ProxyError::install(
                ErrorCode::InstallExtractFailed,
                format!("Failed to read tar: {e}"),
            )
        })? {
            let mut entry = entry.map_err(|e| {
                ProxyError::install(
                    ErrorCode::InstallExtractFailed,
                    format!("Failed to read tar entry: {e}"),
                )
            })?;

            let path = entry
                .path()
                .map_err(|e| ProxyError::install(ErrorCode::InstallExtractFailed, e.to_string()))?;
            let file_name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            if file_name == binary_name || file_name == "sing-box" {
                entry.unpack(bin_dir.join(binary_name)).map_err(|e| {
                    ProxyError::install(
                        ErrorCode::InstallExtractFailed,
                        format!("Failed to extract binary: {e}"),
                    )
                })?;
                return Ok(());
            }
        }

        Err(ProxyError::install(
            ErrorCode::InstallBinaryNotFound,
            "Binary not found in tar.gz archive",
        ))
    }

    fn download_url(version: &str) -> String {
        let suffix = Self::platform_suffix();
        format!("https://github.com/SagerNet/sing-box/releases/download/v{version}/sing-box-{version}-{suffix}")
    }

    /// Download a file, with ghproxy.net fallback for blocked regions.
    async fn download_with_fallback(url: &str) -> Result<Vec<u8>, ProxyError> {
        // First attempt: direct GitHub download
        match reqwest::get(url).await {
            Ok(resp) if resp.status().is_success() => {
                return resp.bytes().await.map(|b| b.to_vec()).map_err(|e| {
                    ProxyError::install(
                        ErrorCode::InstallDownloadFailed,
                        format!("Failed to read response: {e}"),
                    )
                });
            }
            Ok(resp) => {
                tracing::warn!(target: "installer",
                    "Direct download returned HTTP {}, trying ghproxy...",
                    resp.status()
                );
            }
            Err(e) => {
                tracing::warn!(target: "installer", "Direct download failed ({e}), trying ghproxy...");
            }
        }

        // Fallback: ghproxy.net mirror (accessible from China, Russia, Iran)
        let ghproxy_url = format!("https://ghproxy.net/{}", url);
        match reqwest::get(&ghproxy_url).await {
            Ok(resp) if resp.status().is_success() => {
                resp.bytes().await.map(|b| b.to_vec()).map_err(|e| {
                    ProxyError::install(
                        ErrorCode::InstallDownloadFailed,
                        format!("Failed to read ghproxy response: {e}"),
                    )
                })
            }
            Ok(resp) => Err(ProxyError::install(
                ErrorCode::InstallDownloadFailed,
                format!(
                    "ghproxy download failed (HTTP {}): {}",
                    resp.status(),
                    resp.text().await.unwrap_or_default().trim()
                ),
            )),
            Err(e) => Err(ProxyError::install(
                ErrorCode::InstallDownloadFailed,
                format!("All download methods failed. Last error (ghproxy): {e}"),
            )),
        }
    }

    fn platform_suffix() -> &'static str {
        #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
        {
            "windows-amd64.zip"
        }
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            "linux-amd64.tar.gz"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_binary_name_not_empty() {
        let name = Installer::binary_name();
        assert!(!name.is_empty());
        #[cfg(target_os = "windows")]
        assert!(name.ends_with(".exe"));
        #[cfg(not(target_os = "windows"))]
        assert!(!name.ends_with(".exe"));
    }

    #[test]
    fn test_platform_suffix_not_empty() {
        let suffix = Installer::platform_suffix();
        assert!(!suffix.is_empty());
        assert!(suffix.ends_with(".zip") || suffix.ends_with(".tar.gz"));
    }

    #[test]
    fn test_download_url_format() {
        let url = Installer::download_url("1.13.13");
        assert!(url.starts_with("https://github.com/SagerNet/sing-box/releases/download/"));
        assert!(url.contains("v1.13.13"));
        assert!(url.contains("sing-box-1.13.13-"));

        let suffix = Installer::platform_suffix();
        assert!(url.ends_with(suffix));
    }

    #[test]
    fn test_is_installed_returns_false_for_nonexistent() {
        let dir = PathBuf::from("/nonexistent/path");
        assert!(!Installer::is_installed(&dir));
    }

    #[test]
    fn test_extract_zip_all_files() {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zip_writer = zip::ZipWriter::new(&mut buf);
            zip_writer
                .start_file(
                    "sing-box-1.0.0-windows-amd64/sing-box.exe",
                    zip::write::FileOptions::<()>::default(),
                )
                .unwrap();
            zip_writer.write_all(b"fake binary content").unwrap();

            zip_writer
                .start_file(
                    "sing-box-1.0.0-windows-amd64/wintun.dll",
                    zip::write::FileOptions::<()>::default(),
                )
                .unwrap();
            zip_writer.write_all(b"fake dll content").unwrap();

            zip_writer
                .start_file(
                    "sing-box-1.0.0-windows-amd64/README.md",
                    zip::write::FileOptions::<()>::default(),
                )
                .unwrap();
            zip_writer.write_all(b"readme").unwrap();

            zip_writer.finish().unwrap();
        }

        let bytes = buf.into_inner();
        let temp_dir =
            std::env::temp_dir().join(format!("omnitun-test-zip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let result = Installer::extract_zip(&bytes, &temp_dir, "sing-box.exe");
        assert!(result.is_ok(), "extract_zip failed: {:?}", result.err());

        assert!(
            temp_dir.join("sing-box.exe").exists(),
            "sing-box.exe must be extracted"
        );
        assert!(
            temp_dir.join("wintun.dll").exists(),
            "bundled DLL must be extracted"
        );
        assert!(
            temp_dir.join("README.md").exists(),
            "README.md must be extracted"
        );

        assert_eq!(
            std::fs::read(temp_dir.join("sing-box.exe")).unwrap(),
            b"fake binary content"
        );
        assert_eq!(
            std::fs::read(temp_dir.join("wintun.dll")).unwrap(),
            b"fake dll content"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_extract_zip_empty_archive() {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let zip_writer = zip::ZipWriter::new(&mut buf);
            zip_writer.finish().unwrap();
        }

        let bytes = buf.into_inner();
        let temp_dir =
            std::env::temp_dir().join(format!("omnitun-test-zip-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let result = Installer::extract_zip(&bytes, &temp_dir, "sing-box.exe");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Binary not found"));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
