//! A self-contained, deterministic BM25 search-within so a 50k-token original
//! can be partially retrieved. Passages are the document's own non-empty lines
//! (a single giant line is windowed by words); IDF is computed over this
//! document's passages only. Pure — no I/O, no clock, proptest-able.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

const K1: f64 = 1.2;
const B: f64 = 0.75;
const DEFAULT_TOP_N: usize = 5;
const WINDOW_WORDS: usize = 40;

fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn passages(doc: &str) -> Vec<String> {
    match doc
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>()
    {
        lines if lines.len() > 1 => lines.into_iter().map(str::to_owned).collect(),
        _ => window_words(doc),
    }
}

fn window_words(doc: &str) -> Vec<String> {
    match doc.split_whitespace().collect::<Vec<_>>() {
        words if words.len() > WINDOW_WORDS => words
            .chunks(WINDOW_WORDS)
            .map(|chunk| chunk.join(" "))
            .collect(),
        _ => vec![doc.to_owned()],
    }
}

/// Return the top-N passages of `doc` most relevant to `query`, joined by `\n`
/// in original order. An empty/no-match query returns the first N passages (or
/// the whole short document).
pub fn search_within(doc: &str, query: &str) -> String {
    let passages = passages(doc);
    let query_terms = tokenize(query);
    let take_default = |n: usize| {
        passages
            .iter()
            .take(n)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    };

    if query_terms.is_empty() {
        return take_default(DEFAULT_TOP_N);
    }

    let tokenized: Vec<Vec<String>> = passages.iter().map(|p| tokenize(p)).collect();
    let avg_len = match tokenized.iter().map(Vec::len).sum::<usize>() {
        0 => return take_default(DEFAULT_TOP_N),
        total => total as f64 / tokenized.len() as f64,
    };
    let n = tokenized.len() as f64;

    let scored: Vec<(usize, f64)> = tokenized
        .iter()
        .enumerate()
        .map(|(i, terms)| (i, bm25_score(terms, &query_terms, &tokenized, avg_len, n)))
        .filter(|(_, score)| *score > 0.0)
        .collect();

    match scored {
        ref hits if hits.is_empty() => take_default(DEFAULT_TOP_N),
        mut hits => {
            hits.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            hits.iter()
                .take(DEFAULT_TOP_N)
                .map(|(i, _)| *i)
                .collect::<std::collections::BTreeSet<_>>()
                .iter()
                .map(|&i| passages[i].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        }
    }
}

fn bm25_score(
    terms: &[String],
    query_terms: &[String],
    corpus: &[Vec<String>],
    avg_len: f64,
    n: f64,
) -> f64 {
    let len = terms.len() as f64;
    query_terms
        .iter()
        .map(|q| {
            let tf = terms.iter().filter(|t| *t == q).count() as f64;
            if tf == 0.0 {
                return 0.0;
            }
            let df = corpus.iter().filter(|p| p.contains(q)).count() as f64;
            let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
            idf * (tf * (K1 + 1.0)) / (tf + K1 * (1.0 - B + B * len / avg_len))
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "The cat sat on the mat.\n\
        Dogs are loyal companions to humans.\n\
        Quantum entanglement links distant particles.\n\
        The weather today is sunny and warm.\n\
        Rust ownership prevents data races at compile time.\n\
        Bananas are a good source of potassium.";

    #[test]
    fn query_selects_relevant_passage() {
        let result = search_within(DOC, "quantum particles entanglement");
        assert!(result.contains("Quantum entanglement"));
    }

    #[test]
    fn rust_query_selects_rust_line() {
        let result = search_within(DOC, "rust ownership compile");
        assert!(result.contains("Rust ownership"));
    }

    #[test]
    fn empty_query_returns_leading_passages() {
        let result = search_within(DOC, "");
        assert!(result.starts_with("The cat sat on the mat."));
        assert_eq!(result.lines().count(), DEFAULT_TOP_N);
    }

    #[test]
    fn no_match_falls_back_to_default() {
        let result = search_within(DOC, "zzzzz nonexistent xyzzy");
        assert!(result.starts_with("The cat sat on the mat."));
    }

    #[test]
    fn short_doc_returns_whole() {
        let result = search_within("one short line", "anything");
        assert_eq!(result, "one short line");
    }

    #[test]
    fn results_preserve_original_order() {
        let doc = "alpha keyword here\nbeta nothing\ngamma keyword again\ndelta keyword too";
        let result = search_within(doc, "keyword");
        let lines: Vec<&str> = result.lines().collect();
        let alpha = lines.iter().position(|l| l.starts_with("alpha"));
        let gamma = lines.iter().position(|l| l.starts_with("gamma"));
        assert!(matches!((alpha, gamma), (Some(a), Some(g)) if a < g));
    }

    #[test]
    fn deterministic() {
        assert_eq!(
            search_within(DOC, "rust ownership"),
            search_within(DOC, "rust ownership")
        );
    }
}
