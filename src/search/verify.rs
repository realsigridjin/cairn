//! Exact-string verification for retrieved candidates.
//!
//! Candidate generation is tokenized (BM25) or approximate (IVF); neither can
//! assert that a chunk literally contains a caller-supplied string. Verifying
//! next to metadata filtering on every execution path keeps the meaning of a
//! returned hit identical cold and warm.

use crate::model::SearchRequest;

/// Literal needles precomputed from a request. Case folding happens once per
/// request instead of once per candidate.
#[derive(Debug, Clone, Default)]
pub struct TextVerifier {
    needles: Vec<String>,
    case_sensitive: bool,
}

impl TextVerifier {
    #[must_use]
    pub fn new(req: &SearchRequest) -> Self {
        let case_sensitive = req.require_text_case_sensitive;
        let needles = req
            .require_text
            .iter()
            .map(|needle| {
                if case_sensitive {
                    needle.clone()
                } else {
                    needle.to_lowercase()
                }
            })
            .collect();
        Self {
            needles,
            case_sensitive,
        }
    }

    /// True when the request asked for no verification, so callers can skip
    /// per-candidate work and report exact (non-narrowed) results.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.needles.is_empty()
    }

    /// True when `text` contains every needle. Always evaluated against the
    /// full, untruncated chunk text: a response-side preview budget must never
    /// change which documents match.
    #[must_use]
    pub fn verify(&self, text: &str) -> bool {
        if self.needles.is_empty() {
            return true;
        }
        if self.case_sensitive {
            return self
                .needles
                .iter()
                .all(|needle| text.contains(needle.as_str()));
        }
        // Both sides are folded with the same mapping, so the comparison stays
        // consistent for multi-byte and length-changing case pairs.
        let folded = text.to_lowercase();
        self.needles
            .iter()
            .all(|needle| folded.contains(needle.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::TextVerifier;
    use crate::model::SearchRequest;
    use std::collections::BTreeMap;

    fn request(needles: &[&str], case_sensitive: bool) -> SearchRequest {
        SearchRequest {
            query: "q".into(),
            query_vector: Vec::new(),
            limit: 10,
            candidate_limit: 32,
            revision: None,
            filters: BTreeMap::new(),
            require_text: needles.iter().map(|n| (*n).to_owned()).collect(),
            require_text_case_sensitive: case_sensitive,
            max_text_bytes: 0,
            max_remote_bytes: 256 * 1024 * 1024,
            max_range_reads: 4096,
        }
    }

    #[test]
    fn empty_needles_accept_everything() {
        let verifier = TextVerifier::new(&request(&[], false));
        assert!(verifier.is_noop());
        assert!(verifier.verify(""));
        assert!(verifier.verify("anything at all"));
    }

    #[test]
    fn needles_are_conjunctive() {
        let verifier = TextVerifier::new(&request(&["alpha", "beta"], false));
        assert!(verifier.verify("alpha and beta"));
        assert!(!verifier.verify("alpha only"));
        assert!(!verifier.verify("beta only"));
    }

    #[test]
    fn case_sensitivity_is_opt_in() {
        assert!(TextVerifier::new(&request(&["HYDRATEprefs"], false)).verify("hydratePrefs()"));
        assert!(!TextVerifier::new(&request(&["HYDRATEprefs"], true)).verify("hydratePrefs()"));
        assert!(TextVerifier::new(&request(&["hydratePrefs"], true)).verify("hydratePrefs()"));
    }

    #[test]
    fn multibyte_needles_match_without_splitting_codepoints() {
        let verifier = TextVerifier::new(&request(&["관련 정보"], false));
        assert!(verifier.verify("문서에서 관련 정보를 찾습니다"));
        assert!(!verifier.verify("문서에서 관련만 찾습니다"));
    }
}
