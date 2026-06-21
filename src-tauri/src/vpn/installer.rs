use crate::logging::{log, LogLevel, Logger, LogSource};
use crate::vpn::VpnError;
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
        if !binary_path.exists() {
            return false;
        }
        // Windows: wintun.dll is required for TUN (VPN) mode.
        // Without it, sing-box will crash on VPN start.
        #[cfg(target_os = "windows")]
        if !bin_dir.join("wintun.dll").exists() {
            return false;
        }
        true
    }

    pub async fn install(
        app_handle: &AppHandle,
        bin_dir: &PathBuf,
        version: &str,
    ) -> Result<PathBuf, VpnError> {
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
        let response = reqwest::get(&url)
            .await
            .map_err(|e| VpnError::Install(format!("Failed to download sing-box: {e}")))?;

        let bytes = response
            .bytes()
            .await
            .map_err(|e| VpnError::Install(format!("Failed to read response: {e}")))?;

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

        // Fallback: download wintun.dll separately if still missing (Windows)
        #[cfg(target_os = "windows")]
        if !bin_dir.join("wintun.dll").exists() {
            log(
                app_handle,
                &logger,
                LogLevel::Info,
                LogSource::Installer,
                "wintun.dll not found in sing-box archive, downloading separately...".into(),
            );
            Self::download_wintun(bin_dir).await?;
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

    pub async fn latest_version(app_handle: &AppHandle) -> Result<String, VpnError> {
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
            .await
            .map_err(|e| VpnError::Install(format!("Failed to check latest version: {e}")))?;

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| VpnError::Install(format!("Failed to parse version: {e}")))?;

        let tag = json["tag_name"]
            .as_str()
            .ok_or_else(|| VpnError::Install("Missing tag_name in response".into()))?;

        let version = tag.trim_start_matches('v').to_string();

        log(
            app_handle,
            &logger,
            LogLevel::Info,
            LogSource::Installer,
            format!("Latest version: {version}"),
        );

        Ok(version)
    }

    fn extract_zip(bytes: &[u8], bin_dir: &Path, binary_name: &str) -> Result<(), VpnError> {
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
            .map_err(|e| VpnError::Install(format!("Failed to open zip: {e}")))?;

        let mut found_binary = false;
        for i in 0..archive.len() {
            let mut entry = archive
                .by_index(i)
                .map_err(|e| VpnError::Install(format!("Failed to read zip entry: {e}")))?;

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
            let mut out = std::fs::File::create(&out_path)
                .map_err(|e| VpnError::Install(format!("Failed to create {file_name}: {e}")))?;
            std::io::copy(&mut entry, &mut out)
                .map_err(|e| VpnError::Install(format!("Failed to extract {file_name}: {e}")))?;
        }

        if found_binary {
            Ok(())
        } else {
            Err(VpnError::Install("Binary not found in zip archive".into()))
        }
    }

    fn extract_tar_gz(bytes: &[u8], bin_dir: &Path, binary_name: &str) -> Result<(), VpnError> {
        let decompressed = flate2::read::GzDecoder::new(bytes);
        let mut archive = tar::Archive::new(decompressed);

        for entry in archive
            .entries()
            .map_err(|e| VpnError::Install(format!("Failed to read tar: {e}")))?
        {
            let mut entry =
                entry.map_err(|e| VpnError::Install(format!("Failed to read tar entry: {e}")))?;

            let path = entry.path().map_err(|e| VpnError::Install(e.to_string()))?;
            let file_name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            if file_name == binary_name || file_name == "sing-box" {
                entry
                    .unpack(bin_dir.join(binary_name))
                    .map_err(|e| VpnError::Install(format!("Failed to extract binary: {e}")))?;
                return Ok(());
            }
        }

        Err(VpnError::Install(
            "Binary not found in tar.gz archive".into(),
        ))
    }

    fn download_url(version: &str) -> String {
        let suffix = Self::platform_suffix();
        format!("https://github.com/SagerNet/sing-box/releases/download/v{version}/sing-box-{version}-{suffix}")
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

    #[cfg(target_os = "windows")]
    async fn download_wintun(bin_dir: &Path) -> Result<(), VpnError> {
        let url = "https://www.wintun.net/builds/wintun-0.14.1.zip";
        let response = reqwest::get(url)
            .await
            .map_err(|e| VpnError::Install(format!("Failed to download wintun: {e}")))?;
        let bytes = response
            .bytes()
            .await
            .map_err(|e| VpnError::Install(format!("Failed to read wintun response: {e}")))?;

        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
            .map_err(|e| VpnError::Install(format!("Failed to open wintun zip: {e}")))?;

        for i in 0..archive.len() {
            let mut entry = archive
                .by_index(i)
                .map_err(|e| VpnError::Install(format!("Failed to read wintun zip entry: {e}")))?;

            if entry.is_dir() {
                continue;
            }

            let entry_name = entry.name().to_string();
            // Inside wintun-0.14.1.zip the DLL is at: wintun-0.14.1/amd64/wintun.dll
            if entry_name.ends_with("amd64/wintun.dll") {
                let out_path = bin_dir.join("wintun.dll");
                let mut out = std::fs::File::create(&out_path)
                    .map_err(|e| VpnError::Install(format!("Failed to create wintun.dll: {e}")))?;
                std::io::copy(&mut entry, &mut out)
                    .map_err(|e| VpnError::Install(format!("Failed to extract wintun.dll: {e}")))?;
                return Ok(());
            }
        }

        Err(VpnError::Install("wintun.dll not found in wintun archive".into()))
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

    /// Verify that extract_zip extracts ALL files (binary + wintun.dll + others)
    /// from the archive structure that the official sing-box release uses.
    #[test]
    fn test_extract_zip_all_files() {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zip_writer = zip::ZipWriter::new(&mut buf);
            // Simulate official sing-box archive structure:
            // sing-box-{version}-windows-amd64/sing-box.exe
            // sing-box-{version}-windows-amd64/wintun.dll
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
            "wintun.dll must be extracted"
        );
        assert!(
            temp_dir.join("README.md").exists(),
            "README.md must be extracted"
        );

        // Verify content integrity
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

    /// Verify that extract_zip returns an error when the archive is empty.
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
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Binary not found"));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
