//! Text normalisation and tokenisation shared by indexing and querying.

use unicode_normalization::UnicodeNormalization;

const STOPWORDS_EN: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "from", "has", "have", "he",
    "if", "in", "into", "is", "it", "its", "of", "on", "or", "she", "so", "than", "that", "the",
    "their", "then", "there", "these", "they", "this", "to", "was", "we", "were", "which", "will",
    "with", "you",
];

const STOPWORDS_AR: &[&str] = &[
    "في", "من", "على", "الى", "عن", "ان", "هذا", "هذه", "ذلك", "تلك", "التي", "الذي", "كان",
    "كانت", "او", "ثم", "مع", "قد", "لا", "ما", "هو", "هي", "به", "بها", "كل", "بعد", "قبل",
];

/// True for the marks `normalize` deletes (harakat, dagger alef, tatweel).
pub fn is_stripped(c: char) -> bool {
    matches!(c, '\u{064B}'..='\u{065F}' | '\u{0670}' | '\u{0640}')
}

/// NFKC, lowercase, strip Arabic diacritics and tatweel, fold alef variants to bare alef.
pub fn normalize(s: &str) -> String {
    s.nfkc()
        .flat_map(char::to_lowercase)
        .filter(|c| !is_stripped(*c))
        .map(|c| match c {
            '\u{0622}' | '\u{0623}' | '\u{0625}' | '\u{0671}' => '\u{0627}',
            c => c,
        })
        .collect()
}

pub fn is_stopword(token: &str) -> bool {
    STOPWORDS_EN.contains(&token) || STOPWORDS_AR.contains(&token)
}

pub fn tokenize(s: &str) -> Vec<String> {
    normalize(s)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= 2 && !is_stopword(t))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_marks_and_folds_alef() {
        assert_eq!(normalize("المُوافَقة"), "الموافقة");
        assert_eq!(normalize("إشعار أ آ"), "اشعار ا ا");
        assert_eq!(normalize("رســالة"), "رسالة");
    }

    #[test]
    fn tokenize_drops_stopwords_and_short_tokens() {
        assert_eq!(
            tokenize("The Opt-out of a Consent"),
            ["opt", "out", "consent"]
        );
        assert!(tokenize("the and of").is_empty());
    }
}
