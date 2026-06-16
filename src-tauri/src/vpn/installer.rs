use crate::vpn::VpnError;
use std::path::PathBuf;

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
        bin_dir.join(Self::binary_name()).exists()
    }

    pub async fn install(bin_dir: &PathBuf, version: &str) -> Result<PathBuf, VpnError> {
        let url = Self::download_url(version);
        let suffix = Self::platform_suffix();
        let binary_name = Self::binary_name();

        std::fs::create_dir_all(bin_dir)?;

        let response = reqwest::get(&url)
            .await
            .map_err(|e| VpnError::Install(format!("Failed to download: {e}")))?;

        let bytes = response
            .bytes()
            .await
            .map_err(|e| VpnError::Install(format!("Failed to read response: {e}")))?;

        if suffix.ends_with(".zip") {
            Self::extract_zip(&bytes, bin_dir, binary_name)?;
        } else {
            Self::extract_tar_gz(&bytes, bin_dir, binary_name)?;
        }

        let binary_path = bin_dir.join(binary_name);

        #[cfg(not(target_os = "windows"))]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&binary_path, std::fs::Permissions::from_mode(0o755))
        }

        Ok(binary_path)
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
                let mut out = std::fs::File::create(bin_dir.join(entry_name))
                    .map_err(|e| VpnError::Install(format!("Failed to create binary: {e}")))?;
                std::io::copy(&mut entry, &mut out)
                    .map_err(|e| VpnError::Install(format!("Failed to extract: {e}")))?;
                return Ok(());
            }
        }
        Err(VpnError::Install("Binary not found in archive".into()))
    }

    fn extract_tar_gz(bytes: &[u8], bin_dir: &PathBuf, binary_name: &str) -> Result<(), VpnError> {
        let decompressed = flate2::read::GzDecoder::new(bytes);
        let mut archive = tar::Archive::new(decompressed);

        for entry in archive
            .entries()
            .map_err(|e| VpnError::Install(format!("Failed to read tar: {e}")))?
        {
            let mut entry =
                entry.map_err(|e| VpnError::Install(format!("Failed to read entry: {e}")))?;
            let path = entry.path().map_err(|e| VpnError::Install(e.to_string()))?;
            let file_name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            if file_name == binary_name || file_name == "sing-box" {
                entry
                    .unpack(bin_dir.join(binary_name))
                    .map_err(|e| VpnError::Install(format!("Failed to extract: {e}")))?;
                return Ok(());
            }
        }
        Err(VpnError::Install("Binary not found in archive".into()))
    }

    fn download_url(version: &str) -> String {
        let suffix = Self::platform_suffix();
        format!("https://github.com/SagerNet/sing-box/releases/download/{version}/sing-box-{version}-{suffix}")
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
