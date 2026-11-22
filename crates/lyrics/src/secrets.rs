//! API keys and tokens, kept out of config.json in a DPAPI-protected file
//! (`secrets.dat` in the data folder) that only the current Windows user can decrypt.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use syllable_core::log;

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Secrets {
    pub spicy_api_key: Option<String>,

    // Spotify web player sign-in (WebView2). The sign-in cookie itself lives in the WebView2 profile.
    pub spotify_web_signed_in: bool,
    pub spotify_web_token: Option<String>,
    /// Unix seconds the token was captured.
    pub spotify_web_token_time: f64,
    pub spotify_user_name: Option<String>,

    // The user's own Spotify app (Authorization Code with PKCE).
    pub spotify_client_id: Option<String>,
    pub spotify_access_token: Option<String>,
    /// Unix seconds.
    pub spotify_access_expires: f64,
    pub spotify_refresh_token: Option<String>,
}

/// Never print the values.
impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secrets")
            .field("spicy_api_key", &self.spicy_api_key.is_some())
            .field("spotify_web_signed_in", &self.spotify_web_signed_in)
            .field("spotify_client_id", &self.spotify_client_id)
            .field("spotify_refresh_token", &self.spotify_refresh_token.is_some())
            .finish_non_exhaustive()
    }
}

const ENTROPY: &[u8] = b"Syllable.secrets.v1";

pub fn path_in(dir: &Path) -> PathBuf {
    dir.join("secrets.dat")
}

impl Secrets {
    /// Empty when the file is missing or unreadable; a damaged file is logged.
    pub fn load(path: &Path) -> Self {
        Self::try_load(path).unwrap_or_else(|e| {
            log!("secrets: load failed ({e}); starting empty");
            Self::default()
        })
    }

    /// Like `load`, but returns the error instead of logging it (tests use this so
    /// they do not write into the app's real log).
    pub fn try_load(path: &Path) -> anyhow::Result<Self> {
        let Ok(data) = fs::read(path) else { return Ok(Self::default()) };
        let plain = dpapi::unprotect(&data, ENTROPY)?;
        Ok(serde_json::from_slice(&plain)?)
    }

    pub fn save(&self, path: &Path) {
        let result = (|| -> anyhow::Result<()> {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)?;
            }
            let data = dpapi::protect(&serde_json::to_vec(self)?, ENTROPY)?;
            let tmp = path.with_extension("dat.tmp");
            fs::write(&tmp, data)?;
            fs::rename(&tmp, path)?;
            Ok(())
        })();
        if let Err(e) = result {
            log!("secrets: save failed: {e}");
        }
    }
}

mod dpapi {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    fn blob(b: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB { cbData: b.len() as u32, pbData: b.as_ptr() as *mut u8 }
    }

    /// Copies the output blob and frees it.
    fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        // SAFETY: DPAPI returned a LocalAlloc'd buffer of cbData bytes.
        unsafe {
            let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            let _ = LocalFree(Some(HLOCAL(out.pbData as *mut _)));
            v
        }
    }

    pub fn protect(plain: &[u8], entropy: &[u8]) -> anyhow::Result<Vec<u8>> {
        let (input, ent) = (blob(plain), blob(entropy));
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: the input blobs point at live slices; out is written by the call.
        unsafe { CryptProtectData(&input, PCWSTR::null(), Some(&ent), None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)? };
        Ok(take(out))
    }

    pub fn unprotect(data: &[u8], entropy: &[u8]) -> anyhow::Result<Vec<u8>> {
        let (input, ent) = (blob(data), blob(entropy));
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: as above.
        unsafe { CryptUnprotectData(&input, None, Some(&ent), None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)? };
        Ok(take(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_encrypted() {
        let dir = std::env::temp_dir().join(format!("syllable-lyrics-secrets-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let p = path_in(&dir);
        assert_eq!(Secrets::load(&p), Secrets::default());
        let s = Secrets { spicy_api_key: Some("sk-test-123".into()), spotify_client_id: Some("abc".into()), ..Default::default() };
        s.save(&p);
        let raw = fs::read(&p).unwrap();
        assert!(!raw.windows(11).any(|w| w == b"sk-test-123"), "stored encrypted");
        assert_eq!(Secrets::load(&p), s);
        assert!(!format!("{s:?}").contains("sk-test"));
        // Garbage is an error, not a panic (`load` turns it into empty secrets).
        fs::write(&p, b"nope").unwrap();
        assert!(Secrets::try_load(&p).is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
