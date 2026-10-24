//! In-memory BM25 (k1 1.5, b 0.75) over an inverted index.

use std::collections::HashMap;

const K1: f32 = 1.5;
const B: f32 = 0.75;

#[derive(Default)]
pub struct Bm25 {
    postings: HashMap<String, Vec<(usize, u32)>>,
    lens: Vec<u32>,
    avg_len: f32,
}

impl Bm25 {
    /// One token list per document; document ids are positions in `docs`.
    pub fn build(docs: &[Vec<String>]) -> Bm25 {
        let mut postings: HashMap<String, Vec<(usize, u32)>> = HashMap::new();
        for (id, tokens) in docs.iter().enumerate() {
            let mut tf: HashMap<&str, u32> = HashMap::new();
            for t in tokens {
                *tf.entry(t).or_default() += 1;
            }
            for (t, n) in tf {
                postings.entry(t.to_string()).or_default().push((id, n));
            }
        }
        let lens: Vec<u32> = docs.iter().map(|d| d.len() as u32).collect();
        let avg_len = lens.iter().sum::<u32>() as f32 / lens.len().max(1) as f32;
        Bm25 {
            postings,
            lens,
            avg_len,
        }
    }

    /// Scores of every document matching at least one term, as `(doc id, score)`.
    pub fn scores(&self, terms: &[String]) -> Vec<(usize, f32)> {
        let n = self.lens.len() as f32;
        let mut acc: HashMap<usize, f32> = HashMap::new();
        for term in terms {
            let Some(list) = self.postings.get(term) else {
                continue;
            };
            let df = list.len() as f32;
            let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
            for &(id, tf) in list {
                let tf = tf as f32;
                let norm = 1.0 - B + B * self.lens[id] as f32 / self.avg_len.max(1.0);
                *acc.entry(id).or_default() += idf * tf * (K1 + 1.0) / (tf + K1 * norm);
            }
        }
        acc.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Vec<String> {
        s.split(' ').map(str::to_string).collect()
    }

    #[test]
    fn rarer_term_and_higher_tf_score_higher() {
        let b = Bm25::build(&[d("cat cat dog"), d("cat dog dog"), d("bird dog dog")]);
        let mut s = b.scores(&["cat".into()]);
        s.sort_by(|a, b| b.1.total_cmp(&a.1));
        assert_eq!(s[0].0, 0);
        assert_eq!(s.len(), 2);
        let bird = b.scores(&["bird".into()])[0].1;
        let cat = b.scores(&["cat".into()])[0].1;
        assert!(bird > cat);
    }
}
