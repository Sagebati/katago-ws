//! `-override-config` strings (KataGo's config syntax): `key=value,key=value`, ordered.

use std::fmt;

/// `numAnalysisThreads` — positions searched in parallel.
pub const ANALYSIS_THREADS: &str = "numAnalysisThreads";
/// `numSearchThreadsPerAnalysisThread` — MCTS threads per position.
pub const SEARCH_THREADS: &str = "numSearchThreadsPerAnalysisThread";
/// `numNNServerThreadsPerModel` — NN evaluation threads (one batch each).
pub const SERVER_THREADS: &str = "numNNServerThreadsPerModel";
/// `nnMaxBatchSize` — positions per NN batch.
pub const BATCH_SIZE: &str = "nnMaxBatchSize";

/// An ordered set of `key=value` settings. Keys are unique; setting an
/// existing key replaces its value in place.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides(Vec<(String, String)>);

impl Overrides {
    /// Parse `key=value,key=value`. Blank entries are skipped, whitespace is
    /// trimmed, and a repeated key keeps its last value (as KataGo would read
    /// it from a config file).
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        let mut out = Self::default();
        for entry in raw.split(',') {
            let Some((key, value)) = entry.split_once('=') else {
                continue;
            };
            let key = key.trim();
            if !key.is_empty() {
                out.set(key, value.trim());
            }
        }
        out
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(have, _)| have == key)
            .map(|(_, value)| value.as_str())
    }

    /// Set `key`, replacing its value if present.
    pub fn set(&mut self, key: &str, value: &str) {
        match self.0.iter_mut().find(|(have, _)| have == key) {
            Some((_, old)) => value.clone_into(old),
            None => self.0.push((key.to_owned(), value.to_owned())),
        }
    }

    /// Add every key of `other` that is not already set here (existing values
    /// win — this is how pinned keys beat tuned ones).
    pub fn fill(&mut self, other: &Self) {
        for (key, value) in &other.0 {
            if self.get(key).is_none() {
                self.0.push((key.clone(), value.clone()));
            }
        }
    }

    /// The `(key, value)` pairs in order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    /// Keep only the keys `keep` accepts.
    pub fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.0.retain(|(key, _)| keep(key));
    }
}

impl fmt::Display for Overrides {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (idx, (key, value)) in self.0.iter().enumerate() {
            if idx > 0 {
                out.write_str(",")?;
            }
            write!(out, "{key}={value}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_trims_and_round_trips() {
        let over = Overrides::parse(" numAnalysisThreads = 128 ,, junk ,nnMaxBatchSize=32");
        assert_eq!(over.get(ANALYSIS_THREADS), Some("128"));
        assert_eq!(over.get(BATCH_SIZE), Some("32"));
        assert_eq!(over.to_string(), "numAnalysisThreads=128,nnMaxBatchSize=32");
        assert!(Overrides::parse("").is_empty());
        assert!(Overrides::parse("  ").is_empty());
    }

    #[test]
    fn a_repeated_key_keeps_its_last_value() {
        let over = Overrides::parse("nnMaxBatchSize=64,nnMaxBatchSize=32");
        assert_eq!(over.to_string(), "nnMaxBatchSize=32");
    }

    #[test]
    fn fill_never_overwrites_a_pinned_key() {
        let mut pinned = Overrides::parse("numNNServerThreadsPerModel=4,logDir=/tmp/x");
        let tuned = Overrides::parse("numNNServerThreadsPerModel=8,nnMaxBatchSize=32");
        pinned.fill(&tuned);
        assert_eq!(
            pinned.to_string(),
            "numNNServerThreadsPerModel=4,logDir=/tmp/x,nnMaxBatchSize=32"
        );
    }
}
