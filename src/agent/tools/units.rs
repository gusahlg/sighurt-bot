//! Deterministic unit conversion so Sig never guesses 1.6 vs 1.609.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Dim {
    Length,
    Mass,
    Time,
    Data,
    Speed,
    Volume,
    Area,
    Temp,
}

/// (canonical name, factor to the base unit, dimension, aliases)
const TABLE: &[(&str, f64, Dim, &[&str])] = &[
    ("m", 1.0, Dim::Length, &["meter", "meters", "metre", "metres"]),
    ("km", 1000.0, Dim::Length, &["kilometer", "kilometers", "kilometre", "kilometres"]),
    ("cm", 0.01, Dim::Length, &["centimeter", "centimeters", "centimetre", "centimetres"]),
    ("mm", 0.001, Dim::Length, &["millimeter", "millimeters", "millimetre", "millimetres"]),
    ("mi", 1609.344, Dim::Length, &["mile", "miles"]),
    ("yd", 0.9144, Dim::Length, &["yard", "yards"]),
    ("ft", 0.3048, Dim::Length, &["foot", "feet", "'"]),
    ("in", 0.0254, Dim::Length, &["inch", "inches", "\""]),
    ("nmi", 1852.0, Dim::Length, &["nautical mile", "nautical miles"]),
    ("kg", 1.0, Dim::Mass, &["kilogram", "kilograms", "kilo", "kilos"]),
    ("g", 0.001, Dim::Mass, &["gram", "grams"]),
    ("mg", 1e-6, Dim::Mass, &["milligram", "milligrams"]),
    ("t", 1000.0, Dim::Mass, &["tonne", "tonnes", "ton", "tons", "metric ton"]),
    ("lb", 0.453_592_37, Dim::Mass, &["lbs", "pound", "pounds"]),
    ("oz", 0.028_349_523_125, Dim::Mass, &["ounce", "ounces"]),
    ("st", 6.350_293_18, Dim::Mass, &["stone", "stones"]),
    ("s", 1.0, Dim::Time, &["sec", "secs", "second", "seconds"]),
    ("ms", 0.001, Dim::Time, &["millisecond", "milliseconds"]),
    ("min", 60.0, Dim::Time, &["mins", "minute", "minutes"]),
    ("h", 3600.0, Dim::Time, &["hr", "hrs", "hour", "hours"]),
    ("d", 86400.0, Dim::Time, &["day", "days"]),
    ("wk", 604800.0, Dim::Time, &["week", "weeks"]),
    ("yr", 31_557_600.0, Dim::Time, &["year", "years"]),
    ("byte", 1.0, Dim::Data, &["b", "bytes"]),
    ("bit", 0.125, Dim::Data, &["bits"]),
    ("kb", 1e3, Dim::Data, &["kilobyte", "kilobytes"]),
    ("mb", 1e6, Dim::Data, &["megabyte", "megabytes"]),
    ("gb", 1e9, Dim::Data, &["gigabyte", "gigabytes"]),
    ("tb", 1e12, Dim::Data, &["terabyte", "terabytes"]),
    ("kib", 1024.0, Dim::Data, &["kibibyte", "kibibytes"]),
    ("mib", 1_048_576.0, Dim::Data, &["mebibyte", "mebibytes"]),
    ("gib", 1_073_741_824.0, Dim::Data, &["gibibyte", "gibibytes"]),
    ("tib", 1_099_511_627_776.0, Dim::Data, &["tebibyte", "tebibytes"]),
    ("m/s", 1.0, Dim::Speed, &["mps", "meters per second", "metres per second"]),
    ("km/h", 1000.0 / 3600.0, Dim::Speed, &["kph", "kmh", "kilometers per hour", "kilometres per hour"]),
    ("mph", 1609.344 / 3600.0, Dim::Speed, &["miles per hour"]),
    ("knot", 1852.0 / 3600.0, Dim::Speed, &["knots", "kn", "kt", "kts"]),
    ("l", 1.0, Dim::Volume, &["liter", "liters", "litre", "litres"]),
    ("ml", 0.001, Dim::Volume, &["milliliter", "milliliters", "millilitre", "millilitres"]),
    ("gal", 3.785_411_784, Dim::Volume, &["gallon", "gallons"]),
    ("qt", 0.946_352_946, Dim::Volume, &["quart", "quarts"]),
    ("pt", 0.473_176_473, Dim::Volume, &["pint", "pints"]),
    ("cup", 0.236_588_236_5, Dim::Volume, &["cups"]),
    ("floz", 0.029_573_529_562_5, Dim::Volume, &["fl oz", "fluid ounce", "fluid ounces"]),
    ("m2", 1.0, Dim::Area, &["m^2", "sqm", "square meter", "square meters", "square metre", "square metres"]),
    ("km2", 1e6, Dim::Area, &["km^2", "square kilometer", "square kilometers", "square kilometre", "square kilometres"]),
    ("ha", 10_000.0, Dim::Area, &["hectare", "hectares"]),
    ("acre", 4046.856_422_4, Dim::Area, &["acres"]),
    ("ft2", 0.092_903_04, Dim::Area, &["ft^2", "sqft", "square foot", "square feet"]),
    ("c", 0.0, Dim::Temp, &["°c", "celsius", "centigrade", "degc", "degrees c", "degrees celsius"]),
    ("f", 0.0, Dim::Temp, &["°f", "fahrenheit", "degf", "degrees f", "degrees fahrenheit"]),
    ("k", 0.0, Dim::Temp, &["kelvin", "kelvins"]),
];

fn lookup(unit: &str) -> Option<(&'static str, f64, Dim)> {
    let u = unit.trim().to_lowercase();
    let u = u.trim_end_matches('.').trim();
    if u.is_empty() {
        return None;
    }
    for (name, factor, dim, aliases) in TABLE {
        if *name == u || aliases.iter().any(|a| *a == u) {
            return Some((name, *factor, *dim));
        }
    }
    None
}

/// Integers without ".0", else up to 8 significant digits, trailing zeros trimmed.
pub fn format_qty(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    let s = format!("{:.8}", v);
    // Keep 8 significant digits rather than 8 decimals for small magnitudes.
    let sig = if v.abs() < 1.0 {
        let leading = format!("{:.8e}", v);
        let mant: f64 = leading.parse().unwrap_or(v);
        format!("{:.10}", mant)
    } else {
        s
    };
    let trimmed = sig.trim_end_matches('0').trim_end_matches('.').to_string();
    if trimmed.is_empty() || trimmed == "-0" {
        "0".to_string()
    } else {
        trimmed
    }
}

fn to_celsius(v: f64, unit: &str) -> f64 {
    match unit {
        "c" => v,
        "f" => (v - 32.0) * 5.0 / 9.0,
        _ => v - 273.15,
    }
}

fn from_celsius(c: f64, unit: &str) -> f64 {
    match unit {
        "c" => c,
        "f" => c * 9.0 / 5.0 + 32.0,
        _ => c + 273.15,
    }
}

/// Convert `value` from `from` to `to`, e.g. `convert(100.0, "km", "mi")`.
pub fn convert(value: f64, from: &str, to: &str) -> Result<String, String> {
    let (fname, ffac, fdim) = lookup(from).ok_or_else(|| format!("unknown unit '{}'", from.trim()))?;
    let (tname, tfac, tdim) = lookup(to).ok_or_else(|| format!("unknown unit '{}'", to.trim()))?;
    if fdim != tdim {
        return Err(format!(
            "can't convert {fname} to {tname}: different kinds of quantity"
        ));
    }
    let out = if fdim == Dim::Temp {
        from_celsius(to_celsius(value, fname), tname)
    } else {
        value * ffac / tfac
    };
    if !out.is_finite() {
        return Err("result is not finite".to_string());
    }
    Ok(format!("{} {fname} = {} {tname}", format_qty(value), format_qty(out)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn num(s: &str) -> f64 {
        s.split(" = ").nth(1).unwrap().split(' ').next().unwrap().parse().unwrap()
    }

    #[test]
    fn conversions() {
        assert!((num(&convert(100.0, "km", "mi").unwrap()) - 62.137119).abs() < 1e-5);
        assert_eq!(convert(0.0, "c", "f").unwrap(), "0 c = 32 f");
        assert!((num(&convert(100.0, "F", "C").unwrap()) - 37.777778).abs() < 1e-5);
        assert!((num(&convert(300.0, "k", "c").unwrap()) - 26.85).abs() < 1e-9);
        assert!((num(&convert(5.0, "kg", "lbs").unwrap()) - 11.023113).abs() < 1e-5);
        assert_eq!(convert(1.0, "mile", "feet").unwrap(), "1 mi = 5280 ft");
        assert!((num(&convert(1.0, "gib", "mb").unwrap()) - 1073.7418).abs() < 1e-3);
        assert!((num(&convert(60.0, "mph", "km/h").unwrap()) - 96.56064).abs() < 1e-4);
        assert_eq!(convert(2.0, "hours", "minutes").unwrap(), "2 h = 120 min");
        assert!((num(&convert(1.0, "gallon", "liters").unwrap()) - 3.7854118).abs() < 1e-6);
        assert!((num(&convert(1.0, "acre", "m2").unwrap()) - 4046.8564).abs() < 1e-3);
        assert!(convert(1.0, "kilometers", "°C").is_err());
        assert!(convert(1.0, "parsecs2", "m").unwrap_err().contains("parsecs2"));
        let err = convert(1.0, "km", "kg").unwrap_err();
        assert!(err.contains("km") && err.contains("kg"));
    }

    #[test]
    fn qty_format() {
        assert_eq!(format_qty(5280.0), "5280");
        assert_eq!(format_qty(0.5), "0.5");
        assert_eq!(format_qty(62.137119224), "62.13711922");
    }
}
