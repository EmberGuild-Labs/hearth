//! Units as dimension vectors.
//!
//! A `.emx` column can declare a unit, and the point of declaring one is that
//! arithmetic across columns is then checkable. `distance / time` has to
//! produce something in `m/s`; if the column it is assigned to says `m`, that
//! is an error, and it is an error the file can detect on its own rather than
//! one that surfaces as a wrong number in a report six months later.
//!
//! A unit is a scale factor and a map from base dimension to exponent. `km`
//! is `1000 × m¹`; `m/s²` is `1 × m¹s⁻²`. Two units are compatible when their
//! dimension maps are equal, whatever their scales, and converting between
//! them is one multiplication.
//!
//! Symbols this module does not know become their own base dimension with a
//! scale of 1. That is deliberate: `USD`, `requests`, `widgets` and `bushels`
//! are perfectly good units for checking arithmetic with, and a table that
//! refused unknown symbols would be useless for the data people actually
//! have. The cost is that `USD` and `EUR` are simply different dimensions,
//! which is correct — there is no fixed conversion between them.

use std::collections::BTreeMap;

/// Symbols with a known conversion to a base dimension.
///
/// Kept small on purpose. Every entry is a claim about the world that has to
/// be right, and a long table of half-remembered conversions is worse than a
/// short one plus honest unknowns.
const KNOWN: &[(&str, f64, &str)] = &[
    // length, base metre
    ("m", 1.0, "m"),
    ("km", 1000.0, "m"),
    ("cm", 0.01, "m"),
    ("mm", 0.001, "m"),
    ("mi", 1609.344, "m"),
    ("ft", 0.3048, "m"),
    ("in", 0.0254, "m"),
    // time, base second
    ("s", 1.0, "s"),
    ("ms", 0.001, "s"),
    ("us", 1e-6, "s"),
    ("ns", 1e-9, "s"),
    ("min", 60.0, "s"),
    ("h", 3600.0, "s"),
    ("d", 86400.0, "s"),
    // mass, base kilogram
    ("kg", 1.0, "kg"),
    ("g", 0.001, "kg"),
    ("mg", 1e-6, "kg"),
    ("t", 1000.0, "kg"),
    ("lb", 0.45359237, "kg"),
    ("oz", 0.028349523125, "kg"),
    // information, base byte
    ("B", 1.0, "B"),
    ("KB", 1024.0, "B"),
    ("MB", 1048576.0, "B"),
    ("GB", 1073741824.0, "B"),
    ("TB", 1099511627776.0, "B"),
];

#[derive(Clone, Debug)]
pub struct Unit {
    /// Multiplier to get to the base dimensions.
    pub scale: f64,
    /// Base dimension to exponent. Empty means dimensionless.
    pub dims: BTreeMap<String, i32>,
    /// The symbol as it was written. Carried only so that error messages can
    /// say "cannot add km and min" rather than naming the base dimensions
    /// those reduce to, which is correct but unrecognisable to whoever wrote
    /// the formula.
    pub label: String,
}

// Equality is dimensional. Two units that mean the same thing are the same
// unit whatever they were spelled as, so the label must not take part.
impl PartialEq for Unit {
    fn eq(&self, other: &Self) -> bool {
        self.scale == other.scale && self.dims == other.dims
    }
}

impl Unit {
    pub fn dimensionless() -> Self {
        Unit {
            scale: 1.0,
            dims: BTreeMap::new(),
            label: String::new(),
        }
    }

    pub fn is_dimensionless(&self) -> bool {
        self.dims.is_empty()
    }

    /// Parse `m`, `m/s`, `kg*m/s^2`, `USD/month`, `1`, or the empty string.
    ///
    /// Everything after the first `/` is inverted, which is how people
    /// actually write units — `kg*m/s^2*K` means `kg·m·s⁻²·K⁻¹`.
    pub fn parse(s: &str) -> Result<Unit, String> {
        let s = s.trim();
        if s.is_empty() || s == "1" || s == "-" {
            return Ok(Unit::dimensionless());
        }

        let (num, den) = match s.split_once('/') {
            Some((a, b)) => (a, b),
            None => (s, ""),
        };
        let mut u = Unit::dimensionless();
        u.label = s.to_string();
        for (part, sign) in [(num, 1), (den, -1)] {
            for factor in part
                .split(['*', '·'])
                .map(str::trim)
                .filter(|f| !f.is_empty())
            {
                let (sym, exp) = match factor.split_once('^') {
                    Some((sym, e)) => (
                        sym.trim(),
                        e.trim()
                            .parse::<i32>()
                            .map_err(|_| format!("'{e}' is not an exponent in unit '{s}'"))?,
                    ),
                    None => (factor, 1),
                };
                if sym.is_empty() {
                    return Err(format!("unit '{s}' has an empty factor"));
                }
                if !sym.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    return Err(format!("'{sym}' is not a valid unit symbol"));
                }
                u.mul_symbol(sym, exp * sign);
            }
        }
        u.dims.retain(|_, e| *e != 0);
        Ok(u)
    }

    fn mul_symbol(&mut self, sym: &str, exp: i32) {
        let (scale, base) = KNOWN
            .iter()
            .find(|(s, _, _)| *s == sym)
            .map(|(_, sc, b)| (*sc, *b))
            // An unrecognised symbol is its own base dimension. `USD` is a
            // real unit; there is simply nothing to convert it to.
            .unwrap_or((1.0, sym));
        self.scale *= scale.powi(exp);
        *self.dims.entry(base.to_string()).or_insert(0) += exp;
    }

    /// Same physical dimensions, whatever the scale. `km` and `m` are
    /// compatible; `m` and `s` are not.
    pub fn compatible(&self, other: &Unit) -> bool {
        self.dims == other.dims
    }

    /// Multiply this value by this to express it in `to`.
    pub fn factor_to(&self, to: &Unit) -> Option<f64> {
        self.compatible(to).then(|| self.scale / to.scale)
    }

    pub fn mul(&self, other: &Unit) -> Unit {
        let mut d = self.dims.clone();
        for (k, e) in &other.dims {
            *d.entry(k.clone()).or_insert(0) += e;
        }
        d.retain(|_, e| *e != 0);
        Unit {
            scale: self.scale * other.scale,
            dims: d,
            label: join(&self.label, "*", &other.label),
        }
    }

    pub fn div(&self, other: &Unit) -> Unit {
        let mut d = self.dims.clone();
        for (k, e) in &other.dims {
            *d.entry(k.clone()).or_insert(0) -= e;
        }
        d.retain(|_, e| *e != 0);
        Unit {
            scale: self.scale / other.scale,
            dims: d,
            label: join(&self.label, "/", &other.label),
        }
    }

    pub fn powi(&self, n: i32) -> Unit {
        let mut d = self.dims.clone();
        for e in d.values_mut() {
            *e *= n;
        }
        d.retain(|_, e| *e != 0);
        Unit {
            scale: self.scale.powi(n),
            dims: d,
            label: if self.label.is_empty() {
                String::new()
            } else {
                format!("{}^{n}", self.label)
            },
        }
    }

    /// How to name this unit to a person: what they wrote, with the base
    /// dimensions in brackets when the two differ.
    pub fn describe(&self) -> String {
        let base = self.describe_dims();
        match self.label.trim() {
            "" => base,
            l if l == base => base,
            l => format!("{l} [{base}]"),
        }
    }

    /// Canonical spelling of the base dimensions alone.
    pub fn describe_dims(&self) -> String {
        if self.dims.is_empty() {
            return "dimensionless".into();
        }
        let mut num: Vec<String> = Vec::new();
        let mut den: Vec<String> = Vec::new();
        for (k, e) in &self.dims {
            let (target, e) = if *e > 0 {
                (&mut num, *e)
            } else {
                (&mut den, -*e)
            };
            target.push(if e == 1 {
                k.clone()
            } else {
                format!("{k}^{e}")
            });
        }
        match (num.is_empty(), den.is_empty()) {
            (_, true) => num.join("*"),
            (true, false) => format!("1/{}", den.join("*")),
            (false, false) => format!("{}/{}", num.join("*"), den.join("*")),
        }
    }
}

/// Compose two unit labels, keeping the result readable when one side is
/// dimensionless and has nothing to contribute.
fn join(a: &str, op: &str, b: &str) -> String {
    match (a.trim().is_empty(), b.trim().is_empty()) {
        (true, true) => String::new(),
        (true, false) => format!("1{op}{b}"),
        (false, true) => a.to_string(),
        (false, false) => format!("{a}{op}{b}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_and_compound_units_parse() {
        assert!(Unit::parse("").unwrap().is_dimensionless());
        assert_eq!(Unit::parse("m").unwrap().describe(), "m");
        assert_eq!(Unit::parse("m/s").unwrap().describe(), "m/s");
        assert_eq!(Unit::parse("kg*m/s^2").unwrap().describe_dims(), "kg*m/s^2");
        assert_eq!(Unit::parse("1/s").unwrap().describe_dims(), "1/s");
    }

    #[test]
    fn prefixes_carry_their_scale() {
        let km = Unit::parse("km").unwrap();
        let m = Unit::parse("m").unwrap();
        assert!(km.compatible(&m));
        assert_eq!(km.factor_to(&m), Some(1000.0));
        assert_eq!(m.factor_to(&km), Some(0.001));
    }

    #[test]
    fn incompatible_dimensions_have_no_conversion() {
        let m = Unit::parse("m").unwrap();
        let s = Unit::parse("s").unwrap();
        assert!(!m.compatible(&s));
        assert_eq!(m.factor_to(&s), None);
    }

    #[test]
    fn unknown_symbols_become_their_own_dimension() {
        let usd = Unit::parse("USD").unwrap();
        let eur = Unit::parse("EUR").unwrap();
        assert_eq!(usd.describe(), "USD");
        // Correct: there is no fixed conversion between currencies, so they
        // must not be silently interchangeable.
        assert!(!usd.compatible(&eur));
        assert!(usd.compatible(&Unit::parse("USD").unwrap()));
    }

    #[test]
    fn arithmetic_composes_dimensions() {
        let m = Unit::parse("m").unwrap();
        let s = Unit::parse("s").unwrap();
        assert!(m.div(&s).compatible(&Unit::parse("m/s").unwrap()));
        assert!(m.div(&s).div(&s).compatible(&Unit::parse("m/s^2").unwrap()));
        assert!(m.mul(&m).compatible(&Unit::parse("m^2").unwrap()));
        assert!(m.div(&m).is_dimensionless());
    }

    #[test]
    fn scales_compose_correctly() {
        // km/h expressed in m/s is the familiar 1/3.6.
        let kmh = Unit::parse("km/h").unwrap();
        let ms = Unit::parse("m/s").unwrap();
        let f = kmh.factor_to(&ms).unwrap();
        assert!((f - 1.0 / 3.6).abs() < 1e-12, "{f}");
    }

    #[test]
    fn malformed_units_are_reported() {
        assert!(Unit::parse("m^x").is_err());
        assert!(Unit::parse("m s").is_err());
    }
}
