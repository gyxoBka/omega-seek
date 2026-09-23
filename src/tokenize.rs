use rust_stemmers::{Algorithm, Stemmer};

/// PostgreSQL (BSD licence) Snowball lists
const ENGLISH_STOP: &str = include_str!("stop/english.txt");
const RUSSIAN_STOP: &str = include_str!("stop/russian.txt");

pub struct Tokenizer {
    stemmer: Stemmer,
    /// Comments, strings and error messages are not always English.
    russian: Stemmer,
    stop: std::collections::HashSet<&'static str>,
}

impl std::fmt::Debug for Tokenizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Tokenizer")
    }
}

impl Default for Tokenizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Tokenizer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            stemmer: Stemmer::create(Algorithm::English),
            russian: Stemmer::create(Algorithm::Russian),
            stop: [ENGLISH_STOP, RUSSIAN_STOP]
                .iter()
                .flat_map(|list| list.lines())
                .map(str::trim)
                .filter(|word| !word.is_empty())
                .collect(),
        }
    }

    /// The terms of a text, in order, repeats kept.
    ///
    /// An identifier yields its stemmed parts -- `cleanup_prepared_assets` is
    /// `cleanup`, `prepar`, `asset` -- so prose and code meet on the same
    /// terms, and also itself whole, so asking for it by name still finds it.
    pub fn terms(&self, text: &str, out: &mut Vec<String>) {
        self.terms_of(text, false, out);
    }

    /// `asked` drops the stop words, lowercased and before the stemmer sees
    /// them, so that `какие` never reaches the index as `как`.
    fn terms_of(&self, text: &str, asked: bool, out: &mut Vec<String>) {
        let mut parts: Vec<&str> = Vec::new();
        for word in text.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
            if word.is_empty() {
                continue;
            }
            parts.clear();
            for piece in word.split('_') {
                split_camel(piece, &mut parts);
            }
            let mut emitted = 0;
            for part in &parts {
                if part.chars().count() < 2 || part.bytes().all(|b| b.is_ascii_digit()) {
                    continue;
                }
                let lower = part.to_lowercase();
                if asked && self.stop.contains(lower.as_str()) {
                    continue;
                }
                if lower.is_ascii() {
                    out.push(self.stemmer.stem(&lower).into_owned());
                } else if lower.chars().all(|c| ('а'..='я').contains(&c) || c == 'ё') {
                    out.push(self.russian.stem(&lower).into_owned());
                } else {
                    out.push(lower);
                }
                emitted += 1;
            }
            if emitted > 1 {
                out.push(word.to_lowercase());
            }
        }
    }

    /// The distinct terms of a question, without the words around them.
    #[must_use]
    pub fn query_terms(&self, query: &str) -> Vec<String> {
        let mut all = Vec::new();
        self.terms_of(query, true, &mut all);
        let mut seen = std::collections::HashSet::new();
        all.retain(|term| seen.insert(term.clone()));
        all
    }
}

/// `HTTPServer2` is `HTTP`, `Server`, `2`.
fn split_camel<'a>(piece: &'a str, out: &mut Vec<&'a str>) {
    let chars: Vec<(usize, char)> = piece.char_indices().collect();
    let mut start = 0;
    for i in 1..chars.len() {
        let (prev, cur) = (chars[i - 1].1, chars[i].1);
        let next_lower = chars.get(i + 1).is_some_and(|(_, c)| c.is_lowercase());
        let boundary = (prev.is_lowercase() && cur.is_uppercase())
            || (prev.is_alphabetic() != cur.is_alphabetic())
            || (prev.is_uppercase() && cur.is_uppercase() && next_lower);
        if boundary {
            out.push(&piece[start..chars[i].0]);
            start = chars[i].0;
        }
    }
    if start < piece.len() {
        out.push(&piece[start..]);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_question_keeps_only_the_words_that_name_something() {
        let t = super::Tokenizer::new();
        let asked = t.query_terms("какие статусы бывают у задачи и почему это так, где хранится состояние");
        assert_eq!(asked, ["статус", "быва", "задач", "хран", "состоян"]);
        assert_eq!(t.query_terms("what is the token refresh interceptor"), ["token", "refresh", "interceptor"]);
    }

    use super::*;

    #[test]
    fn identifiers_split_and_stay_whole() {
        let mut out = Vec::new();
        Tokenizer::new().terms("cleanup_prepared_assets HTTPServer2", &mut out);
        assert_eq!(
            out,
            [
                "cleanup",
                "prepar",
                "asset",
                "cleanup_prepared_assets",
                "http",
                "server",
                "httpserver2"
            ]
        );
    }

    #[test]
    fn a_question_loses_its_grammar() {
        let terms = Tokenizer::new().query_terms("Where is the text turned into a vector?");
        assert_eq!(terms, ["text", "turn", "vector"]);
    }
}
