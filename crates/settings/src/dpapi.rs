//! SP6b: Windows DPAPI でユーザ束縛の at-rest 暗号化。base64 で保存。別マシン/ユーザでは復号不可→None。
use base64::Engine;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB};
use windows::core::PCWSTR;
use zeroize::Zeroizing;

/// API が割り当てた出力 blob を LocalFree で解放する。
/// pbData が null なら何もしない（API がエラーを返した場合）。
fn free_blob(out: &CRYPT_INTEGER_BLOB) {
    if !out.pbData.is_null() {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(out.pbData as *mut _)));
        }
    }
}

/// `plain` を DPAPI で暗号化し base64 文字列を返す。
/// 空文字列 → None。OS 呼び出し失敗 → None。
pub fn encrypt(plain: &str) -> Option<String> {
    if plain.is_empty() {
        return None;
    }
    // plaintext コピーは Zeroizing で drop 時にゼロ化する（pbData が指すため呼び出し中は生存させる）。
    let mut input = Zeroizing::new(plain.as_bytes().to_vec());
    // cbData は u32。4GiB 超は切り捨てで別データを暗号化してしまうため弾く（実鍵では到達しない）。
    if input.len() > u32::MAX as usize {
        return None;
    }
    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: input.len() as u32,
        pbData: input.as_mut_ptr(),
    };
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(&in_blob, PCWSTR::null(), None, None, None, 0, &mut out).ok()?;
        let slice = std::slice::from_raw_parts(out.pbData, out.cbData as usize);
        let b64 = base64::engine::general_purpose::STANDARD.encode(slice);
        free_blob(&out);
        Some(b64)
    }
}

/// base64 blob を DPAPI で復号し UTF-8 文字列を返す。
/// base64 デコード失敗 / OS 呼び出し失敗 / UTF-8 変換失敗 → None。
/// 返り値は plaintext のため Zeroizing で包み、drop 時にゼロ化する。
pub fn decrypt(b64: &str) -> Option<Zeroizing<String>> {
    // ciphertext は秘密ではないが、害もないので Zeroizing で包む。
    let mut bytes = Zeroizing::new(base64::engine::general_purpose::STANDARD.decode(b64).ok()?);
    if bytes.is_empty() || bytes.len() > u32::MAX as usize {
        return None;
    }
    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_mut_ptr(),
    };
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(&in_blob, None, None, None, None, 0, &mut out).ok()?;
        // 復号された plaintext を Zeroizing にコピー（drop 時にゼロ化）。
        let plain = Zeroizing::new(std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec());
        // OS 所有の blob を LocalFree 前に in-place でゼロ化し、plaintext を残さない。
        if !out.pbData.is_null() {
            std::ptr::write_bytes(out.pbData, 0, out.cbData as usize);
        }
        free_blob(&out);
        // 借用したまま UTF-8 検証し、成功時のみ String を構築する。
        // （from_utf8(Vec) はエラー時に plaintext Vec をエラー値へ移すため避ける。）
        match std::str::from_utf8(&plain) {
            Ok(s) => Some(Zeroizing::new(s.to_owned())),
            Err(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_then_decrypt_roundtrip() {
        let blob = encrypt("sk-secret-123").expect("encrypt");
        assert!(!blob.is_empty());
        assert_ne!(blob, "sk-secret-123");
        // decrypt は Option<Zeroizing<String>>。as_deref で Option<&String> として比較する。
        assert_eq!(
            decrypt(&blob).as_deref().map(|s| s.as_str()),
            Some("sk-secret-123")
        );
    }

    #[test]
    fn decrypt_garbage_is_none() {
        assert!(decrypt("!!!not-base64!!!").is_none());
        assert!(decrypt("").is_none());
    }

    #[test]
    fn empty_plaintext_is_none() {
        assert_eq!(encrypt(""), None);
    }
}
