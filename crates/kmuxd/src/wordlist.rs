use rand::{Rng, RngExt};

const WORDLIST_RAW: &str = include_str!("eff_long_wordlist.txt");

/// Manages the pool of available word IDs.
///
/// Words are drawn uniformly at random and removed from the available pool to
/// guarantee uniqueness. When a session closes, its word is returned to the pool.
pub struct WordlistSampler {
    available: Vec<&'static str>,
}

impl WordlistSampler {
    /// Build the sampler from the embedded EFF long wordlist (7776 words).
    pub fn new() -> Self {
        let available: Vec<&'static str> = WORDLIST_RAW
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        Self { available }
    }

    /// Draw one word at random from the pool. Returns `None` if the pool is
    /// exhausted (should never happen with the 1000-session limit).
    pub fn draw(&mut self, rng: &mut impl Rng) -> Option<String> {
        if self.available.is_empty() {
            return None;
        }
        let idx = rng.random_range(0..self.available.len());
        let word = self.available.swap_remove(idx);
        Some(word.to_string())
    }

    /// Return a word to the pool when its session is closed.
    pub fn release(&mut self, word: &str) {
        // Look for the word in the original static slice.
        // Since WORDLIST_RAW is `'static`, all lines are `&'static str`.
        for line in WORDLIST_RAW.lines() {
            let w = line.trim();
            if w == word {
                self.available.push(w);
                return;
            }
        }
    }

    /// Remove a specific word from the available pool, marking it as in use.
    ///
    /// Used when restoring persisted sessions: the word IDs that were active at
    /// checkpoint time need to be reserved so the daemon does not hand them out
    /// to new sessions.
    ///
    /// Returns `true` if the word was found and removed, `false` if it was
    /// already absent (already reserved or not a valid wordlist entry).
    pub fn reserve(&mut self, word: &str) -> bool {
        if let Some(pos) = self.available.iter().position(|&w| w == word) {
            self.available.swap_remove(pos);
            true
        } else {
            false
        }
    }

    /// Number of words still available for allocation.
    #[cfg(test)]
    pub fn available_count(&self) -> usize {
        self.available.len()
    }
}

impl Default for WordlistSampler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    #[test]
    fn wordlist_loads() {
        let sampler = WordlistSampler::new();
        assert_eq!(sampler.available_count(), 7776);
    }

    #[test]
    fn draw_reduces_pool() {
        let mut sampler = WordlistSampler::new();
        let mut rng = rand::rngs::SmallRng::seed_from_u64(42);
        let word = sampler.draw(&mut rng).expect("should draw a word");
        assert!(!word.is_empty());
        assert_eq!(sampler.available_count(), 7775);
        assert!(sampler.available.iter().all(|&w| w != word.as_str()));
    }

    #[test]
    fn release_returns_word_to_pool() {
        let mut sampler = WordlistSampler::new();
        let mut rng = rand::rngs::SmallRng::seed_from_u64(0);
        let word = sampler.draw(&mut rng).unwrap();
        assert_eq!(sampler.available_count(), 7775);
        sampler.release(&word);
        assert_eq!(sampler.available_count(), 7776);
        assert!(sampler.available.contains(&word.as_str()));
    }

    #[test]
    fn draws_are_unique() {
        let mut sampler = WordlistSampler::new();
        let mut rng = rand::rngs::SmallRng::seed_from_u64(123);
        let mut drawn = std::collections::HashSet::new();
        for _ in 0..100 {
            let word = sampler.draw(&mut rng).unwrap();
            assert!(drawn.insert(word), "duplicate word drawn");
        }
    }
}
