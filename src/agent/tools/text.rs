//! Deterministic string utilities (counting, reversing, casing).

const OPS: &str = "count (needs of=), word_count, char_count, reverse, upper, lower, title";

/// `op` ∈ {count, word_count, char_count, reverse, upper, lower, title}.
pub fn text_util(op: &str, text: &str, of: Option<&str>) -> Result<String, String> {
    let op = op.trim().to_lowercase();
    match op.as_str() {
        "count" | "occurrences" | "count_occurrences" => {
            let needle = of.map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| {
                "count needs of=<substring> (what to count)".to_string()
            })?;
            let n = text.to_lowercase().matches(&needle.to_lowercase()).count();
            Ok(format!("'{needle}' occurs {n} time(s) in \"{text}\""))
        }
        "word_count" | "words" | "count_words" => Ok(format!("word count: {}", text.split_whitespace().count())),
        "char_count" | "length" | "letters" | "count_letters" => Ok(format!(
            "{} characters, {} letters",
            text.chars().count(),
            text.chars().filter(|c| c.is_alphabetic()).count()
        )),
        "reverse" => Ok(format!("reversed: {}", text.chars().rev().collect::<String>())),
        "upper" | "uppercase" => Ok(text.to_uppercase()),
        "lower" | "lowercase" => Ok(text.to_lowercase()),
        "title" | "titlecase" => Ok(text
            .split(' ')
            .map(|w| {
                let mut c = w.chars();
                match c.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(" ")),
        _ => Err(format!("unknown operation '{op}' (valid: {OPS})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ops() {
        assert_eq!(
            text_util("count", "strawberry", Some("r")).unwrap(),
            "'r' occurs 3 time(s) in \"strawberry\""
        );
        assert_eq!(text_util("word_count", "hello big world", None).unwrap(), "word count: 3");
        assert_eq!(text_util("reverse", "stressed", None).unwrap(), "reversed: desserts");
        assert_eq!(text_util("reverse", "héllo", None).unwrap(), "reversed: olléh");
        assert_eq!(text_util("upper", "abc", None).unwrap(), "ABC");
        assert_eq!(text_util("lower", "ABC", None).unwrap(), "abc");
        assert_eq!(text_util("title", "hello world", None).unwrap(), "Hello World");
        assert_eq!(text_util("char_count", "ab c", None).unwrap(), "4 characters, 3 letters");
        assert!(text_util("count", "abc", None).is_err());
        assert!(text_util("explode", "abc", None).unwrap_err().contains("word_count"));
    }
}
