//! Validated immutable assets embedded in the TIP. No external model path or runtime training.
use crate::classify::{
    edge::{DictLayer, Dictionary},
    model::ClassifyModel,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const MANIFEST: &[u8] = include_bytes!("../data/manifest.toml");
const MODEL: &[u8] = include_bytes!("../data/model.txt");
const GENERAL: &[u8] = include_bytes!("../data/dictionary/general.txt");
const TECH: &[u8] = include_bytes!("../data/dictionary/tech.txt");
const MAX_MODEL_BYTES: usize = 1_048_576;
const MAX_DICTIONARY_BYTES: usize = 65_536;
const MAX_WORDS: usize = 4096;
const MAX_WORD_SCALARS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssetError {
    Size,
    Manifest,
    Integrity,
    Model,
    Dictionary,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Asset {
    bytes: usize,
    sha256: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u32,
    kind: String,
    model_format: u32,
    generation: String,
    license: String,
    source: String,
    model: Asset,
    general: Asset,
    tech: Asset,
}
pub struct Bundle {
    pub model: ClassifyModel,
    pub dictionary: Dictionary,
    pub generation: String,
}
impl Bundle {
    pub fn embedded() -> Result<Self, AssetError> {
        Self::parse(MANIFEST, MODEL, GENERAL, TECH)
    }
    fn parse(
        manifest: &[u8],
        model: &[u8],
        general: &[u8],
        tech: &[u8],
    ) -> Result<Self, AssetError> {
        if manifest.len() > 8192
            || model.len() > MAX_MODEL_BYTES
            || general.len() > MAX_DICTIONARY_BYTES
            || tech.len() > MAX_DICTIONARY_BYTES
        {
            return Err(AssetError::Size);
        }
        let manifest: Manifest =
            toml::from_str(std::str::from_utf8(manifest).map_err(|_| AssetError::Manifest)?)
                .map_err(|_| AssetError::Manifest)?;
        if manifest.schema != 1
            || manifest.model_format != 1
            || manifest.kind != "char-ngram-linear"
            || manifest.license != "MIT"
            || manifest.source.is_empty()
            || manifest.generation.is_empty()
            || manifest.generation.len() > 64
            || !manifest
                .generation
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
        {
            return Err(AssetError::Manifest);
        }
        for (bytes, expected) in [
            (model, &manifest.model),
            (general, &manifest.general),
            (tech, &manifest.tech),
        ] {
            if bytes.len() != expected.bytes
                || format!("{:x}", Sha256::digest(bytes)) != expected.sha256
            {
                return Err(AssetError::Integrity);
            }
        }
        let model = ClassifyModel::from_artifact(
            std::str::from_utf8(model).map_err(|_| AssetError::Model)?,
        )
        .map_err(|_| AssetError::Model)?;
        let mut dictionary = Dictionary::new();
        for (bytes, layer) in [(general, DictLayer::General), (tech, DictLayer::Tech)] {
            let text = std::str::from_utf8(bytes).map_err(|_| AssetError::Dictionary)?;
            let mut count = 0;
            for word in text
                .lines()
                .map(str::trim)
                .filter(|w| !w.is_empty() && !w.starts_with('#'))
            {
                count += 1;
                if count > MAX_WORDS
                    || word.chars().count() > MAX_WORD_SCALARS
                    || word.chars().any(|c| c.is_whitespace() || c.is_control())
                {
                    return Err(AssetError::Dictionary);
                }
                dictionary.insert(word, layer);
            }
            if count == 0 {
                return Err(AssetError::Dictionary);
            }
        }
        Ok(Self {
            model,
            dictionary,
            generation: manifest.generation,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn embedded_assets_are_valid_and_match_offline_dictionary() {
        let bundle = Bundle::embedded().unwrap();
        let baseline = crate::classify::synth::default_dictionary();
        assert_eq!(bundle.dictionary.len(), baseline.len());
        for (word, layer) in baseline.words() {
            assert_eq!(bundle.dictionary.layer_of(word), Some(layer));
        }
        assert_eq!(bundle.model.thresholds.auto_margin, 64.0);
    }
    #[test]
    fn missing_corrupt_oversize_and_unknown_assets_fail_closed() {
        assert!(matches!(
            Bundle::parse(MANIFEST, b"", GENERAL, TECH),
            Err(AssetError::Integrity)
        ));
        let mut corrupt = MODEL.to_vec();
        corrupt[20] ^= 1;
        assert!(matches!(
            Bundle::parse(MANIFEST, &corrupt, GENERAL, TECH),
            Err(AssetError::Integrity)
        ));
        assert!(matches!(
            Bundle::parse(MANIFEST, MODEL, TECH, GENERAL),
            Err(AssetError::Integrity)
        ));
        assert!(matches!(
            Bundle::parse(MANIFEST, &vec![0; MAX_MODEL_BYTES + 1], GENERAL, TECH),
            Err(AssetError::Size)
        ));
        let newer = String::from_utf8(MANIFEST.to_vec())
            .unwrap()
            .replace("schema = 1", "schema = 2");
        assert!(matches!(
            Bundle::parse(newer.as_bytes(), MODEL, GENERAL, TECH),
            Err(AssetError::Manifest)
        ));
    }
    #[test]
    fn checksummed_but_truncated_model_disables_assets() {
        let invalid =
            b"format=mixed-input-classify-model\nversion=1\nkind=char-ngram-linear\nchecksum=0";
        let updated = String::from_utf8(MANIFEST.to_vec())
            .unwrap()
            .replace(
                &format!("bytes = {}", MODEL.len()),
                &format!("bytes = {}", invalid.len()),
            )
            .replace(
                &format!("{:x}", Sha256::digest(MODEL)),
                &format!("{:x}", Sha256::digest(invalid)),
            );
        assert!(matches!(
            Bundle::parse(updated.as_bytes(), invalid, GENERAL, TECH),
            Err(AssetError::Model)
        ));
    }

    #[test]
    fn checksummed_but_invalid_dictionary_is_rejected() {
        for invalid in [
            "a".repeat(MAX_WORD_SCALARS + 1),
            "two words".into(),
            "# no words\n".into(),
            "a\n".repeat(MAX_WORDS + 1),
        ] {
            let updated = String::from_utf8(MANIFEST.to_vec())
                .unwrap()
                .replace(
                    &format!("bytes = {}", GENERAL.len()),
                    &format!("bytes = {}", invalid.len()),
                )
                .replace(
                    &format!("{:x}", Sha256::digest(GENERAL)),
                    &format!("{:x}", Sha256::digest(invalid.as_bytes())),
                );
            assert!(matches!(
                Bundle::parse(updated.as_bytes(), MODEL, invalid.as_bytes(), TECH),
                Err(AssetError::Dictionary)
            ));
        }
    }
}
