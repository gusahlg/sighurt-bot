//! Dice: NdM(+K). Real randomness so the model can't invent a face.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_DICE: u64 = 100;
const MAX_SIDES: u64 = 1000;
const MAX_MOD: i64 = 10_000;
const USAGE: &str = "use NdM or NdM+K (e.g. 2d6, d20, 4d6+3). max 100 dice, 2-1000 sides";

struct Parsed {
    n: u64,
    sides: u64,
    modifier: i64,
}

fn parse(spec: &str) -> Result<Parsed, String> {
    let s: String = spec.trim().to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
    let (dice_part, mod_part) = match s.find(['+', '-']) {
        Some(i) if i > 0 => (&s[..i], &s[i..]),
        _ => (s.as_str(), ""),
    };
    let (n_str, sides_str) = dice_part.split_once('d').ok_or_else(|| USAGE.to_string())?;
    let n: u64 = if n_str.is_empty() { 1 } else { n_str.parse().map_err(|_| USAGE.to_string())? };
    let sides: u64 = sides_str.parse().map_err(|_| USAGE.to_string())?;
    let modifier: i64 = if mod_part.is_empty() {
        0
    } else {
        mod_part.parse().map_err(|_| USAGE.to_string())?
    };
    if !(1..=MAX_DICE).contains(&n) || !(2..=MAX_SIDES).contains(&sides) || modifier.abs() > MAX_MOD {
        return Err(USAGE.to_string());
    }
    Ok(Parsed { n, sides, modifier })
}

/// Roll with a caller-provided source of randomness (`rng(sides)` returns 0..sides).
pub(crate) fn roll_with<R: FnMut(u64) -> u64>(spec: &str, mut rng: R) -> Result<String, String> {
    let p = parse(spec)?;
    let faces: Vec<u64> = (0..p.n).map(|_| rng(p.sides) % p.sides + 1).collect();
    let total: i64 = faces.iter().sum::<u64>() as i64 + p.modifier;
    let shown = faces.iter().map(u64::to_string).collect::<Vec<_>>().join("+");
    let label = format!("{}d{}", p.n, p.sides);
    if p.modifier != 0 {
        let sign = if p.modifier > 0 { "+" } else { "-" };
        return Ok(format!(
            "{label}{sign}{}: [{shown}] {sign}{} = {total}",
            p.modifier.abs(),
            p.modifier.abs()
        ));
    }
    if p.n == 1 {
        return Ok(format!("d{} = {}", p.sides, faces[0]));
    }
    Ok(format!("{label}: [{shown}] = {total}"))
}

fn seed() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ COUNTER.fetch_add(0x2545_F491_4F6C_DD1D, Ordering::Relaxed)
}

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Roll `spec` = NdM, NdM+K, NdM-K, dM. Output like "2d6: [3+5] = 8".
pub fn roll(spec: &str) -> Result<String, String> {
    let mut state = seed();
    roll_with(spec, |sides| splitmix(&mut state) % sides)
}

/// Extract the first dice spec like "2d6", "d20", "3d8+2" from free text.
pub fn find_spec(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'd' {
            // Optional digits before, digits after, optional +/-digits.
            let mut start = i;
            while start > 0 && bytes[start - 1].is_ascii_digit() {
                start -= 1;
            }
            let word_before = start > 0 && (bytes[start - 1] as char).is_alphabetic();
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 && !word_before && (start < i || i == 0 || !(bytes[i - 1] as char).is_alphanumeric()) {
                let mut end = j;
                if end < bytes.len() && (bytes[end] == b'+' || bytes[end] == b'-') {
                    let mut k = end + 1;
                    while k < bytes.len() && bytes[k].is_ascii_digit() {
                        k += 1;
                    }
                    if k > end + 1 {
                        end = k;
                    }
                }
                let after_ok = end >= bytes.len() || !(bytes[end] as char).is_alphanumeric();
                if after_ok && parse(&lower[start..end]).is_ok() {
                    return Some(lower[start..end].to_string());
                }
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        let mut seq = [2u64, 4].into_iter();
        assert_eq!(roll_with("2d6", |_| seq.next().unwrap()).unwrap(), "2d6: [3+5] = 8");
        assert_eq!(roll_with("d20", |_| 16).unwrap(), "d20 = 17");
        let mut seq = [0u64, 5, 1, 3].into_iter();
        assert_eq!(roll_with("4d6+3", |_| seq.next().unwrap()).unwrap(), "4d6+3: [1+6+2+4] +3 = 16");
        let mut seq = [2u64, 4].into_iter();
        assert_eq!(roll_with("2d6-1", |_| seq.next().unwrap()).unwrap(), "2d6-1: [3+5] -1 = 7");
    }

    #[test]
    fn ranges_and_caps() {
        for _ in 0..200 {
            let out = roll("3d6").unwrap();
            let total: i64 = out.rsplit("= ").next().unwrap().parse().unwrap();
            assert!((3..=18).contains(&total), "{out}");
        }
        assert!(roll("101d6").is_err());
        assert!(roll("1d1").is_err());
        assert!(roll("2d6+99999").is_err());
        assert!(roll("banana").is_err());
    }

    #[test]
    fn spec_extraction() {
        assert_eq!(find_spec("roll 2d6 pls"), Some("2d6".into()));
        assert_eq!(find_spec("d20"), Some("d20".into()));
        assert_eq!(find_spec("gimme a 3d8+2 roll"), Some("3d8+2".into()));
        assert_eq!(find_spec("2024 dodge"), None);
        assert_eq!(find_spec("hello world"), None);
    }
}
