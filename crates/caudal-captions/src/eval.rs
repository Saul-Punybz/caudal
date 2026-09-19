//! Word error rate, for measuring the recogniser against a known script
//! (the engine test, the `transcribe` example and the end-to-end test).

/// Lower-case words with punctuation stripped; accents are kept
/// ("mañana" and "manana" are different words).
pub fn words(text: &str) -> Vec<String> {
    text.split(|c: char| c.is_whitespace() || (c.is_ascii_punctuation() && c != '\'') || "¿¡«»“”…".contains(c))
        .map(|w| w.trim_matches('\'').to_lowercase())
        .filter(|w| !w.is_empty())
        .collect()
}

/// (substitutions + deletions + insertions) / reference words.
pub fn wer(reference: &str, hypothesis: &str) -> f64 {
    let (r, h) = (words(reference), words(hypothesis));
    if r.is_empty() {
        return if h.is_empty() { 0.0 } else { 1.0 };
    }
    let mut prev: Vec<usize> = (0..=h.len()).collect();
    for (i, rw) in r.iter().enumerate() {
        let mut cur = vec![i + 1; h.len() + 1];
        for (j, hw) in h.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(rw != hw)).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[h.len()] as f64 / r.len() as f64
}

/// Share of `expected` words that appear anywhere in `text` (order and
/// timing ignored): a fuzzy "did the captions say it" check.
pub fn recall(expected: &str, text: &str) -> f64 {
    let got: std::collections::HashSet<String> = words(text).into_iter().collect();
    let want = words(expected);
    if want.is_empty() {
        return 1.0;
    }
    want.iter().filter(|w| got.contains(*w)).count() as f64 / want.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wer_counts_edits_per_reference_word() {
        assert_eq!(wer("hola mundo", "Hola, mundo."), 0.0);
        assert_eq!(wer("a b c d", "a x c d"), 0.25);
        assert_eq!(wer("a b c d", "a c d"), 0.25);
        assert_eq!(wer("a b", "a b c d"), 1.0);
        assert_eq!(wer("¿Qué tal? Bien, gracias.", "que tal bien gracias"), 0.25);
    }

    #[test]
    fn recall_ignores_order() {
        assert_eq!(recall("lluvias fuertes jueves", "el jueves habrá lluvias fuertes"), 1.0);
        assert!((recall("a b c d", "a b") - 0.5).abs() < 1e-9);
    }
}
