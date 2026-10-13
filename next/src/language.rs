//! Language profiles presented by the picker.

use crate::picker::{ModelEntry, DISTIL_SMALL_EN, LARGE_V3_TURBO};

/// User-facing language and the model pair that implements it.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub struct LanguageProfile {
    pub code: &'static str,
    #[serde(skip_serializing)]
    pub live: &'static ModelEntry,
    #[serde(skip_serializing)]
    pub refine: &'static ModelEntry,
    #[serde(skip_serializing)]
    pub required: &'static [&'static ModelEntry],
}

impl LanguageProfile {
    /// Required assets, including native acceleration sidecars on macOS.
    pub fn models(&self) -> &'static [&'static ModelEntry] {
        self.required
    }
}

#[cfg(all(target_os = "macos", feature = "local-engine"))]
const EN_REQUIRED: &[&ModelEntry] = &[
    &DISTIL_SMALL_EN,
    &LARGE_V3_TURBO,
    &crate::picker::DISTIL_SMALL_EN_ENCODER,
    &crate::picker::LARGE_V3_TURBO_ENCODER,
];
#[cfg(not(all(target_os = "macos", feature = "local-engine")))]
const EN_REQUIRED: &[&ModelEntry] = &[&DISTIL_SMALL_EN, &LARGE_V3_TURBO];

pub const LANGUAGES: &[LanguageProfile] = &[LanguageProfile {
    code: "en",
    live: &DISTIL_SMALL_EN,
    refine: &LARGE_V3_TURBO,
    required: EN_REQUIRED,
}];

pub fn find(code: &str) -> Option<&'static LanguageProfile> {
    LANGUAGES.iter().find(|profile| profile.code == code)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::picker::CATALOG;

    #[test]
    fn registry_is_nonempty_and_codes_are_unique() {
        assert!(!LANGUAGES.is_empty());
        let codes: BTreeSet<_> = LANGUAGES.iter().map(|profile| profile.code).collect();
        assert_eq!(codes.len(), LANGUAGES.len());
    }

    #[test]
    fn every_profile_model_is_in_the_download_catalog() {
        for profile in LANGUAGES {
            for model in profile.models() {
                assert!(CATALOG.iter().any(|entry| entry.id == model.id));
            }
        }
    }

    #[test]
    fn live_and_refine_models_are_distinct() {
        for profile in LANGUAGES {
            assert_ne!(profile.live, profile.refine);
        }
    }

    #[test]
    fn find_returns_the_registered_profile() {
        assert_eq!(find("en"), Some(&LANGUAGES[0]));
        assert_eq!(find("missing"), None);
    }
}
