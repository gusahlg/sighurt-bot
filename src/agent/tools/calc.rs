//! Safe arithmetic: a tiny recursive-descent evaluator over f64 with a
//! forgiving normalizer for the way people type math in chat ("6 times 7",
//! "13 squared", "15% of 80", "1,234 x 2"). No `eval`, no crates.

use std::iter::Peekable;
use std::str::Chars;

/// Normalize casual math text into a strict expression.
pub fn normalize(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    s = s.replace('×', "*").replace('÷', "/").replace('−', "-");
    let lower = s.to_lowercase();
    // Question scaffolding people prepend/append.
    let mut cleaned = lower;
    for word in [
        "what's the result of", "what is the result of", "how much is", "what's", "whats",
        "what is", "calculate", "compute", "please", "equals", "evaluate", "solve",
    ] {
        cleaned = cleaned.replace(word, " ");
    }
    cleaned = cleaned.trim().trim_end_matches(['?', '=', '.', '!']).trim().to_string();
    // Word operators.
    cleaned = cleaned
        .replace(" to the power of ", " ^ ")
        .replace(" divided by ", " / ")
        .replace(" times ", " * ")
        .replace(" multiplied by ", " * ")
        .replace(" plus ", " + ")
        .replace(" minus ", " - ")
        .replace(" mod ", " % ")
        .replace(" percent ", " /100 ")
        .replace(" percent", " /100 ");
    // "N squared" / "N cubed" (number or closing paren before the word).
    cleaned = replace_suffix_word(&cleaned, "squared", "^2");
    cleaned = replace_suffix_word(&cleaned, "cubed", "^3");
    // "A% of B" -> "(A/100)*B"; a lone "A%" -> "(A/100)".
    cleaned = rewrite_percent(&cleaned);
    // Thousands separators: a comma between a digit and exactly three digits.
    cleaned = strip_thousands_commas(&cleaned);
    // `x` between two numeric operands is multiply ("1234 x 2", "3x4").
    cleaned = rewrite_x_multiply(&cleaned);
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn replace_suffix_word(s: &str, word: &str, op: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find(word) {
        let (head, tail) = rest.split_at(pos);
        let after = &tail[word.len()..];
        let boundary_ok = after.chars().next().map_or(true, |c| !c.is_alphanumeric());
        // Find the operand just before the word: a number or a parenthesised group.
        let trimmed = head.trim_end();
        let operand_start = if trimmed.ends_with(')') {
            find_matching_open(trimmed)
        } else {
            trimmed
                .rfind(|c: char| !(c.is_ascii_digit() || c == '.'))
                .map(|i| i + 1)
                .unwrap_or(0)
        };
        if boundary_ok && operand_start < trimmed.len() {
            out.push_str(&trimmed[..operand_start]);
            out.push('(');
            out.push_str(&trimmed[operand_start..]);
            out.push_str(op);
            out.push(')');
        } else {
            out.push_str(head);
            out.push_str(word);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

fn find_matching_open(s: &str) -> usize {
    let mut depth = 0i32;
    for (i, c) in s.char_indices().rev() {
        match c {
            ')' => depth += 1,
            '(' => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
    }
    0
}

fn rewrite_percent(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(pos) = rest.find('%') {
        let (head, tail) = rest.split_at(pos);
        let after = &tail[1..];
        let after_trim = after.trim_start();
        if let Some(rest_of) = after_trim.strip_prefix("of ") {
            // Only treat as "percent of" when preceded by a number.
            let trimmed = head.trim_end();
            let start = trimmed
                .rfind(|c: char| !(c.is_ascii_digit() || c == '.'))
                .map(|i| i + 1)
                .unwrap_or(0);
            if start < trimmed.len() {
                out.push_str(&trimmed[..start]);
                out.push('(');
                out.push_str(&trimmed[start..]);
                out.push_str("/100)*");
                rest = rest_of;
                continue;
            }
        }
        // Modulo or a bare percentage: leave `%` for the parser (modulo) unless
        // it is followed by nothing numeric, in which case it means "/100".
        let next_is_operand = after_trim
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit() || c == '(' || c == '.');
        out.push_str(head);
        if next_is_operand {
            out.push('%');
        } else {
            out.push_str("/100");
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

fn strip_thousands_commas(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == ','
            && i > 0
            && chars[i - 1].is_ascii_digit()
            && i + 3 < chars.len() + 0
            && chars[i + 1..].len() >= 3
            && chars[i + 1..i + 4].iter().all(|d| d.is_ascii_digit())
            && !chars.get(i + 4).is_some_and(|d| d.is_ascii_digit())
        {
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

fn rewrite_x_multiply(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for (i, &c) in chars.iter().enumerate() {
        if c == 'x' {
            let prev = chars[..i].iter().rev().find(|ch| !ch.is_whitespace());
            let next = chars[i + 1..].iter().find(|ch| !ch.is_whitespace());
            let prev_ok = prev.is_some_and(|p| p.is_ascii_digit() || *p == ')' || *p == '.');
            let next_ok = next.is_some_and(|n| n.is_ascii_digit() || *n == '(' || *n == '.');
            // Never rewrite when `x` is part of a word (e.g. "exp", "max").
            let prev_alpha = chars.get(i.wrapping_sub(1)).is_some_and(|p| p.is_alphabetic());
            let next_alpha = chars.get(i + 1).is_some_and(|n| n.is_alphabetic());
            if prev_ok && next_ok && !prev_alpha && !next_alpha {
                out.push('*');
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// Evaluate a normalized expression.
pub fn evaluate(expr: &str) -> Result<f64, String> {
    let expr = expr.trim();
    if expr.is_empty() {
        return Err("empty expression".to_string());
    }
    let mut p = Parser {
        it: expr.chars().peekable(),
        depth: 0,
    };
    let v = p.expr()?;
    p.skip_ws();
    if let Some(c) = p.it.peek() {
        return Err(format!("unexpected '{c}'"));
    }
    if !v.is_finite() {
        return Err("result is not a finite number".to_string());
    }
    Ok(v)
}

struct Parser<'a> {
    it: Peekable<Chars<'a>>,
    depth: u32,
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while matches!(self.it.peek(), Some(c) if c.is_whitespace()) {
            self.it.next();
        }
    }

    fn expr(&mut self) -> Result<f64, String> {
        let mut v = self.term()?;
        loop {
            self.skip_ws();
            match self.it.peek() {
                Some('+') => {
                    self.it.next();
                    v += self.term()?;
                }
                Some('-') => {
                    self.it.next();
                    v -= self.term()?;
                }
                _ => return Ok(v),
            }
        }
    }

    fn term(&mut self) -> Result<f64, String> {
        let mut v = self.unary()?;
        loop {
            self.skip_ws();
            match self.it.peek() {
                Some('*') => {
                    self.it.next();
                    v *= self.unary()?;
                }
                Some('/') => {
                    self.it.next();
                    let d = self.unary()?;
                    if d == 0.0 {
                        return Err("division by zero".to_string());
                    }
                    v /= d;
                }
                Some('%') => {
                    self.it.next();
                    let d = self.unary()?;
                    if d == 0.0 {
                        return Err("division by zero (modulo)".to_string());
                    }
                    v %= d;
                }
                _ => return Ok(v),
            }
        }
    }

    fn unary(&mut self) -> Result<f64, String> {
        self.skip_ws();
        match self.it.peek() {
            Some('-') => {
                self.it.next();
                Ok(-self.unary()?)
            }
            Some('+') => {
                self.it.next();
                self.unary()
            }
            _ => self.power(),
        }
    }

    fn power(&mut self) -> Result<f64, String> {
        let base = self.atom()?;
        self.skip_ws();
        if self.it.peek() == Some(&'^') {
            self.it.next();
            // Right associative; the exponent may itself carry a unary minus.
            let exp = self.unary()?;
            if exp.abs() > 1000.0 {
                return Err("exponent too large (limit 1000)".to_string());
            }
            return Ok(base.powf(exp));
        }
        Ok(base)
    }

    fn atom(&mut self) -> Result<f64, String> {
        self.skip_ws();
        match self.it.peek().copied() {
            Some('(') => {
                self.it.next();
                self.depth += 1;
                if self.depth > 64 {
                    return Err("expression nested too deeply".to_string());
                }
                let v = self.expr()?;
                self.skip_ws();
                if self.it.next() != Some(')') {
                    return Err("missing ')'".to_string());
                }
                self.depth -= 1;
                Ok(v)
            }
            Some(c) if c.is_ascii_digit() || c == '.' => self.number(),
            Some(c) if c.is_alphabetic() => self.ident(),
            Some(c) => Err(format!("unexpected '{c}'")),
            None => Err("unexpected end of expression".to_string()),
        }
    }

    fn number(&mut self) -> Result<f64, String> {
        let mut s = String::new();
        while let Some(&c) = self.it.peek() {
            if c.is_ascii_digit() || c == '.' {
                s.push(c);
                self.it.next();
            } else if (c == 'e' || c == 'E') && !s.is_empty() {
                // Scientific notation only if followed by a digit or sign+digit.
                let mut probe = self.it.clone();
                probe.next();
                let mut sign = String::new();
                if let Some(&p) = probe.peek() {
                    if p == '+' || p == '-' {
                        sign.push(p);
                        probe.next();
                    }
                }
                if probe.peek().is_some_and(|d| d.is_ascii_digit()) {
                    s.push('e');
                    s.push_str(&sign);
                    self.it.next();
                    for _ in 0..sign.len() {
                        self.it.next();
                    }
                    continue;
                }
                break;
            } else {
                break;
            }
        }
        s.parse::<f64>().map_err(|_| format!("bad number '{s}'"))
    }

    fn ident(&mut self) -> Result<f64, String> {
        let mut name = String::new();
        while let Some(&c) = self.it.peek() {
            if c.is_alphanumeric() || c == '_' {
                name.push(c);
                self.it.next();
            } else {
                break;
            }
        }
        let lname = name.to_lowercase();
        self.skip_ws();
        if self.it.peek() == Some(&'(') {
            self.it.next();
            let mut args = Vec::new();
            self.skip_ws();
            if self.it.peek() == Some(&')') {
                self.it.next();
            } else {
                loop {
                    args.push(self.expr()?);
                    self.skip_ws();
                    match self.it.next() {
                        Some(',') => continue,
                        Some(')') => break,
                        _ => return Err(format!("bad argument list for {name}")),
                    }
                    #[allow(unreachable_code)]
                    {
                        if args.len() > 8 {
                            return Err("too many arguments".to_string());
                        }
                    }
                }
            }
            return call(&lname, &args);
        }
        match lname.as_str() {
            "pi" => Ok(std::f64::consts::PI),
            "e" => Ok(std::f64::consts::E),
            "tau" => Ok(std::f64::consts::TAU),
            _ => Err(format!("unknown name '{name}'")),
        }
    }
}

fn call(name: &str, args: &[f64]) -> Result<f64, String> {
    let one = |f: fn(f64) -> f64| -> Result<f64, String> {
        match args {
            [a] => Ok(f(*a)),
            _ => Err(format!("{name} takes exactly one argument")),
        }
    };
    match name {
        "sqrt" => one(f64::sqrt),
        "abs" => one(f64::abs),
        "round" => one(f64::round),
        "floor" => one(f64::floor),
        "ceil" => one(f64::ceil),
        "sin" => one(f64::sin),
        "cos" => one(f64::cos),
        "tan" => one(f64::tan),
        "asin" => one(f64::asin),
        "acos" => one(f64::acos),
        "atan" => one(f64::atan),
        "ln" => one(f64::ln),
        "log" | "log10" => one(f64::log10),
        "log2" => one(f64::log2),
        "exp" => one(f64::exp),
        "degrees" => one(f64::to_degrees),
        "radians" => one(f64::to_radians),
        "factorial" => match args {
            [n] if *n >= 0.0 && *n <= 170.0 && n.fract() == 0.0 => {
                Ok((1..=(*n as u64)).fold(1.0f64, |acc, k| acc * k as f64))
            }
            _ => Err("factorial needs one integer in 0..=170".to_string()),
        },
        "gcd" => match args {
            [a, b] if a.fract() == 0.0 && b.fract() == 0.0 => {
                let (mut x, mut y) = ((a.abs()) as u64, (b.abs()) as u64);
                while y != 0 {
                    let t = x % y;
                    x = y;
                    y = t;
                }
                Ok(x as f64)
            }
            _ => Err("gcd needs two integers".to_string()),
        },
        "min" if !args.is_empty() => Ok(args.iter().cloned().fold(f64::INFINITY, f64::min)),
        "max" if !args.is_empty() => Ok(args.iter().cloned().fold(f64::NEG_INFINITY, f64::max)),
        _ => Err(format!("unknown function '{name}'")),
    }
}

/// Format a result: integers without ".0", otherwise up to 12 significant
/// digits with trailing zeros trimmed.
pub fn format_number(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    if v.abs() >= 1e15 || (v != 0.0 && v.abs() < 1e-6) {
        return format!("{v:e}");
    }
    let s = format!("{:.12}", v);
    let s = s.trim_end_matches('0').trim_end_matches('.').to_string();
    if s.is_empty() || s == "-" {
        "0".to_string()
    } else {
        s
    }
}

/// Exact evaluation for integer-only expressions with + - * ( ), so products
/// like 987654321*123456789 print every digit instead of 1.2193e17.
fn evaluate_exact(expr: &str) -> Option<i128> {
    if expr.is_empty() || !expr.chars().all(|c| c.is_ascii_digit() || " +-*()".contains(c)) {
        return None;
    }
    fn skip(s: &[u8], i: &mut usize) {
        while *i < s.len() && s[*i] == b' ' {
            *i += 1;
        }
    }
    fn atom(s: &[u8], i: &mut usize) -> Option<i128> {
        skip(s, i);
        match s.get(*i)? {
            b'(' => {
                *i += 1;
                let v = expr_(s, i)?;
                skip(s, i);
                if s.get(*i) != Some(&b')') {
                    return None;
                }
                *i += 1;
                Some(v)
            }
            b'-' => {
                *i += 1;
                atom(s, i).and_then(|v| v.checked_neg())
            }
            b'+' => {
                *i += 1;
                atom(s, i)
            }
            c if c.is_ascii_digit() => {
                let start = *i;
                while *i < s.len() && s[*i].is_ascii_digit() {
                    *i += 1;
                }
                std::str::from_utf8(&s[start..*i]).ok()?.parse().ok()
            }
            _ => None,
        }
    }
    fn term(s: &[u8], i: &mut usize) -> Option<i128> {
        let mut v = atom(s, i)?;
        loop {
            skip(s, i);
            if s.get(*i) == Some(&b'*') {
                *i += 1;
                v = v.checked_mul(atom(s, i)?)?;
            } else {
                return Some(v);
            }
        }
    }
    fn expr_(s: &[u8], i: &mut usize) -> Option<i128> {
        let mut v = term(s, i)?;
        loop {
            skip(s, i);
            match s.get(*i) {
                Some(b'+') => {
                    *i += 1;
                    v = v.checked_add(term(s, i)?)?;
                }
                Some(b'-') => {
                    *i += 1;
                    v = v.checked_sub(term(s, i)?)?;
                }
                _ => return Some(v),
            }
        }
    }
    let bytes = expr.as_bytes();
    let mut i = 0;
    let v = expr_(bytes, &mut i)?;
    skip(bytes, &mut i);
    (i == bytes.len()).then_some(v)
}

/// normalize + evaluate + format, as "expr = result".
pub fn calculate(raw: &str) -> Result<String, String> {
    let expr = normalize(raw);
    if expr.is_empty() {
        return Err("no expression given".to_string());
    }
    if let Some(exact) = evaluate_exact(&expr) {
        return Ok(format!("{expr} = {exact}"));
    }
    let v = evaluate(&expr)?;
    Ok(format!("{expr} = {}", format_number(v)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(s: &str) -> f64 {
        evaluate(&normalize(s)).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn basics() {
        assert_eq!(ev("17*23"), 391.0);
        assert_eq!(ev("2^10"), 1024.0);
        assert_eq!(ev("2^3^2"), 512.0);
        assert!((ev("sqrt(2)") - 1.41421356237).abs() < 1e-9);
        assert_eq!(ev("(1+2)*3"), 9.0);
        assert_eq!(ev("-3^2"), -9.0);
        assert_eq!(ev("7 % 3"), 1.0);
        assert_eq!(ev("factorial(5)"), 120.0);
        assert_eq!(ev("gcd(12, 18)"), 6.0);
        assert_eq!(ev("min(3, 1, 2)"), 1.0);
        assert_eq!(ev("log(1000)"), 3.0);
        assert!((ev("ln(e)") - 1.0).abs() < 1e-12);
        assert!((ev("pi*2") - std::f64::consts::TAU).abs() < 1e-12);
        assert_eq!(ev("1e3 + 1"), 1001.0);
    }

    #[test]
    fn casual_phrasing() {
        assert_eq!(ev("what is 6 times 7?"), 42.0);
        assert_eq!(ev("13 squared"), 169.0);
        assert_eq!(ev("3 cubed"), 27.0);
        assert_eq!(ev("15% of 80"), 12.0);
        assert_eq!(ev("1,234 x 2"), 2468.0);
        assert_eq!(ev("2 to the power of 8"), 256.0);
        assert_eq!(ev("100 divided by 8"), 12.5);
        assert_eq!(ev("987654321 * 123456789"), 987654321.0 * 123456789.0);
        assert_eq!(ev("calculate 5 plus 5 minus 3"), 7.0);
        assert_eq!(ev("(2+3) squared"), 25.0);
    }

    #[test]
    fn errors() {
        assert!(evaluate("10 / 0").unwrap_err().contains("division by zero"));
        assert!(evaluate("99999^99999").is_err());
        assert!(evaluate("foo(1)").is_err());
        assert!(evaluate("(1+2").is_err());
        assert!(evaluate("").is_err());
        assert!(evaluate("factorial(500)").is_err());
        assert!(calculate("hello there").is_err());
    }

    #[test]
    fn formatting() {
        assert_eq!(format_number(391.0), "391");
        assert_eq!(format_number(12.5), "12.5");
        assert_eq!(format_number(1.0 / 3.0), "0.333333333333");
        assert_eq!(calculate("17 times 23").unwrap(), "17 * 23 = 391");
        assert!(format_number(987654321.0 * 123456789.0).starts_with("1.2193"));
        assert_eq!(calculate("987654321 * 123456789").unwrap(), "987654321 * 123456789 = 121932631112635269");
        assert_eq!(calculate("(2+3)*4 - 1").unwrap(), "(2+3)*4 - 1 = 19");
        assert_eq!(calculate("2^10").unwrap(), "2^10 = 1024");
    }
}
