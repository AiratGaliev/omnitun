use crate::logging::{log, LogLevel, Logger, LogSource};
use crate::vpn::VpnError;
use std::path::PathBuf;
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

    pub fn is_installed(bin_dir: &PathBuf) -> bool {
        let binary_path = bin_dir.join(Self::binary_name());
        binary_path.exists()
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

    fn extract_zip(bytes: &[u8], bin_dir: &PathBuf, binary_name: &str) -> Result<(), VpnError> {
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
            .map_err(|e| VpnError::Install(format!("Failed to open zip: {e}")))?;

        for i in 0..archive.len() {
            let mut entry = archive
                .by_index(i)
                .map_err(|e| VpnError::Install(format!("Failed to read zip entry: {e}")))?;

            let entry_name = entry.name().to_string();
            if entry_name.ends_with(binary_name) || entry_name.ends_with("sing-box") {
                let mut out = std::fs::File::create(bin_dir.join(binary_name))
                    .map_err(|e| VpnError::Install(format!("Failed to create binary file: {e}")))?;
                std::io::copy(&mut entry, &mut out)
                    .map_err(|e| VpnError::Install(format!("Failed to extract binary: {e}")))?;
                return Ok(());
            }
        }

        Err(VpnError::Install("Binary not found in zip archive".into()))
    }

    fn extract_tar_gz(bytes: &[u8], bin_dir: &PathBuf, binary_name: &str) -> Result<(), VpnError> {
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
