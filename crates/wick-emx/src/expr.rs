//! Expressions for computed columns, checked for unit consistency.
//!
//! A computed column stores a formula, not a result: `speed = distance /
//! elapsed`. Two things follow from that. The formula can be re-evaluated
//! when the inputs change, and — the part that matters — its units can be
//! checked *without evaluating it at all*. Adding metres to seconds is an
//! error in the expression, discoverable at validate time on an empty table,
//! not a wrong number discovered later.
//!
//! The grammar is small and total: numbers, column references, the four
//! arithmetic operators, integer powers, unary minus and parentheses. There
//! are no functions, no conditionals and no lookups, because a formula that
//! can do arbitrary work is a formula nobody can check.
//!
//! ```text
//! expr   := term (('+' | '-') term)*
//! term   := unary (('*' | '/') unary)*
//! unary  := '-'? power
//! power  := atom ('^' integer)?
//! atom   := number | identifier | '(' expr ')'
//! ```

use crate::units::Unit;
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Num(f64),
    Col(String),
    Neg(Box<Expr>),
    Add(Box<Expr>, Box<Expr>),
    Sub(Box<Expr>, Box<Expr>),
    Mul(Box<Expr>, Box<Expr>),
    Div(Box<Expr>, Box<Expr>),
    Pow(Box<Expr>, i32),
}

pub fn parse(src: &str) -> Result<Expr, String> {
    let tokens = lex(src)?;
    let mut p = Parser { tokens, at: 0 };
    let e = p.expr()?;
    if p.at < p.tokens.len() {
        return Err(format!(
            "unexpected {:?} after the end of the expression",
            p.tokens[p.at]
        ));
    }
    Ok(e)
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Num(f64),
    Ident(String),
    Op(char),
}

fn lex(src: &str) -> Result<Vec<Tok>, String> {
    let b: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_digit()
            || (c == '.' && b.get(i + 1).is_some_and(|d| d.is_ascii_digit()))
        {
            let start = i;
            while i < b.len()
                && (b[i].is_ascii_digit() || b[i] == '.' || b[i] == 'e' || b[i] == 'E')
            {
                // Scientific notation's sign belongs to the exponent, not to
                // a following term: `1e-3` is one number, not `1e` minus `3`.
                if (b[i] == 'e' || b[i] == 'E') && matches!(b.get(i + 1), Some('-') | Some('+')) {
                    i += 1;
                }
                i += 1;
            }
            let s: String = b[start..i].iter().collect();
            out.push(Tok::Num(
                s.parse().map_err(|_| format!("'{s}' is not a number"))?,
            ));
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_') {
                i += 1;
            }
            out.push(Tok::Ident(b[start..i].iter().collect()));
        } else if "+-*/^()".contains(c) {
            out.push(Tok::Op(c));
            i += 1;
        } else {
            return Err(format!("'{c}' has no meaning in an expression"));
        }
    }
    Ok(out)
}

struct Parser {
    tokens: Vec<Tok>,
    at: usize,
}

impl Parser {
    fn peek_op(&self) -> Option<char> {
        match self.tokens.get(self.at) {
            Some(Tok::Op(c)) => Some(*c),
            _ => None,
        }
    }

    fn expr(&mut self) -> Result<Expr, String> {
        let mut lhs = self.term()?;
        while let Some(op @ ('+' | '-')) = self.peek_op() {
            self.at += 1;
            let rhs = self.term()?;
            lhs = if op == '+' {
                Expr::Add(Box::new(lhs), Box::new(rhs))
            } else {
                Expr::Sub(Box::new(lhs), Box::new(rhs))
            };
        }
        Ok(lhs)
    }

    fn term(&mut self) -> Result<Expr, String> {
        let mut lhs = self.unary()?;
        while let Some(op @ ('*' | '/')) = self.peek_op() {
            self.at += 1;
            let rhs = self.unary()?;
            lhs = if op == '*' {
                Expr::Mul(Box::new(lhs), Box::new(rhs))
            } else {
                Expr::Div(Box::new(lhs), Box::new(rhs))
            };
        }
        Ok(lhs)
    }

    // Unary minus binds looser than `^`, so `-2^2` is -(2²) = -4, matching
    // ordinary mathematical notation rather than the other reading.
    fn unary(&mut self) -> Result<Expr, String> {
        if self.peek_op() == Some('-') {
            self.at += 1;
            return Ok(Expr::Neg(Box::new(self.unary()?)));
        }
        self.power()
    }

    fn power(&mut self) -> Result<Expr, String> {
        let base = self.atom()?;
        if self.peek_op() == Some('^') {
            self.at += 1;
            // Only integer exponents: a fractional power of a unit is not a
            // unit this model can express, so it is refused rather than
            // rounded into one.
            let n = match self.tokens.get(self.at) {
                Some(Tok::Num(n)) if n.fract() == 0.0 => *n as i32,
                Some(Tok::Op('-')) => {
                    self.at += 1;
                    match self.tokens.get(self.at) {
                        Some(Tok::Num(n)) if n.fract() == 0.0 => -(*n as i32),
                        _ => return Err("^ needs a whole-number exponent".into()),
                    }
                }
                _ => return Err("^ needs a whole-number exponent".into()),
            };
            self.at += 1;
            return Ok(Expr::Pow(Box::new(base), n));
        }
        Ok(base)
    }

    fn atom(&mut self) -> Result<Expr, String> {
        match self.tokens.get(self.at).cloned() {
            Some(Tok::Num(n)) => {
                self.at += 1;
                Ok(Expr::Num(n))
            }
            Some(Tok::Ident(name)) => {
                self.at += 1;
                Ok(Expr::Col(name))
            }
            Some(Tok::Op('(')) => {
                self.at += 1;
                let e = self.expr()?;
                if self.peek_op() != Some(')') {
                    return Err("unclosed parenthesis".into());
                }
                self.at += 1;
                Ok(e)
            }
            other => Err(match other {
                Some(t) => format!("expected a value, found {t:?}"),
                None => "expression ended early".into(),
            }),
        }
    }
}

impl Expr {
    /// Every column this expression reads.
    pub fn columns(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<String>) {
        match self {
            Expr::Col(c) => {
                if !out.contains(c) {
                    out.push(c.clone())
                }
            }
            Expr::Num(_) => {}
            Expr::Neg(e) | Expr::Pow(e, _) => e.collect(out),
            Expr::Add(a, b) | Expr::Sub(a, b) | Expr::Mul(a, b) | Expr::Div(a, b) => {
                a.collect(out);
                b.collect(out);
            }
        }
    }

    /// Work out the unit this expression produces, or say why it cannot.
    ///
    /// This is the whole point of the module: it runs on the schema alone,
    /// so a unit error is found when the file is validated rather than when
    /// someone notices the numbers look wrong.
    pub fn unit(&self, cols: &HashMap<String, Unit>) -> Result<Unit, String> {
        Ok(match self {
            Expr::Num(_) => Unit::dimensionless(),
            Expr::Col(name) => cols
                .get(name)
                .cloned()
                .ok_or_else(|| format!("no column named '{name}'"))?,
            Expr::Neg(e) => e.unit(cols)?,
            Expr::Pow(e, n) => e.unit(cols)?.powi(*n),
            Expr::Mul(a, b) => a.unit(cols)?.mul(&b.unit(cols)?),
            Expr::Div(a, b) => a.unit(cols)?.div(&b.unit(cols)?),
            Expr::Add(a, b) | Expr::Sub(a, b) => {
                let (ua, ub) = (a.unit(cols)?, b.unit(cols)?);
                if !ua.compatible(&ub) {
                    let op = if matches!(self, Expr::Add(..)) {
                        "add"
                    } else {
                        "subtract"
                    };
                    return Err(format!(
                        "cannot {op} {} and {}",
                        ua.describe(),
                        ub.describe()
                    ));
                }
                // The left operand's scale wins; the right is converted.
                ua
            }
        })
    }

    /// Evaluate against one row. `None` for a column means the cell was
    /// empty, and any expression touching an empty cell is empty rather than
    /// zero — treating a missing measurement as zero is how averages lie.
    pub fn eval(&self, row: &HashMap<String, f64>, cols: &HashMap<String, Unit>) -> Option<f64> {
        Some(match self {
            Expr::Num(n) => *n,
            Expr::Col(name) => *row.get(name)?,
            Expr::Neg(e) => -e.eval(row, cols)?,
            Expr::Pow(e, n) => e.eval(row, cols)?.powi(*n),
            Expr::Mul(a, b) => a.eval(row, cols)? * b.eval(row, cols)?,
            Expr::Div(a, b) => a.eval(row, cols)? / b.eval(row, cols)?,
            Expr::Add(a, b) | Expr::Sub(a, b) => {
                let (ua, ub) = (a.unit(cols).ok()?, b.unit(cols).ok()?);
                // Scale the right operand into the left's units before
                // combining, so `1 km + 500 m` is 1.5 km rather than 501.
                let f = ub.factor_to(&ua)?;
                let (x, y) = (a.eval(row, cols)?, b.eval(row, cols)? * f);
                if matches!(self, Expr::Add(..)) {
                    x + y
                } else {
                    x - y
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(pairs: &[(&str, &str)]) -> HashMap<String, Unit> {
        pairs
            .iter()
            .map(|(n, u)| (n.to_string(), Unit::parse(u).unwrap()))
            .collect()
    }

    #[test]
    fn precedence_and_grouping() {
        assert_eq!(
            parse("1 + 2 * 3").unwrap(),
            Expr::Add(
                Box::new(Expr::Num(1.0)),
                Box::new(Expr::Mul(
                    Box::new(Expr::Num(2.0)),
                    Box::new(Expr::Num(3.0))
                ))
            )
        );
        let row = HashMap::new();
        let cols = HashMap::new();
        assert_eq!(parse("1 + 2 * 3").unwrap().eval(&row, &cols), Some(7.0));
        assert_eq!(parse("(1 + 2) * 3").unwrap().eval(&row, &cols), Some(9.0));
        assert_eq!(parse("-2^2").unwrap().eval(&row, &cols), Some(-4.0));
        assert_eq!(parse("2e-3 * 1000").unwrap().eval(&row, &cols), Some(2.0));
    }

    #[test]
    fn division_composes_units() {
        let cols = units(&[("distance", "m"), ("elapsed", "s")]);
        let u = parse("distance / elapsed").unwrap().unit(&cols).unwrap();
        assert!(u.compatible(&Unit::parse("m/s").unwrap()));
    }

    #[test]
    fn adding_incompatible_units_fails_loudly() {
        let cols = units(&[("distance", "m"), ("elapsed", "s")]);
        let err = parse("distance + elapsed")
            .unwrap()
            .unit(&cols)
            .unwrap_err();
        assert!(err.contains("cannot add m and s"), "{err}");
    }

    #[test]
    fn adding_the_same_dimension_at_a_different_scale_is_fine_and_converts() {
        let cols = units(&[("a", "km"), ("b", "m")]);
        let e = parse("a + b").unwrap();
        assert!(e
            .unit(&cols)
            .unwrap()
            .compatible(&Unit::parse("m").unwrap()));

        let row: HashMap<String, f64> = [("a".into(), 1.0), ("b".into(), 500.0)].into();
        // 1 km + 500 m, expressed in km.
        assert_eq!(e.eval(&row, &cols), Some(1.5));
    }

    #[test]
    fn a_missing_cell_propagates_rather_than_becoming_zero() {
        let cols = units(&[("a", "m"), ("b", "s")]);
        let row: HashMap<String, f64> = [("a".into(), 10.0)].into();
        assert_eq!(parse("a / b").unwrap().eval(&row, &cols), None);
    }

    #[test]
    fn unknown_columns_are_named() {
        let cols = units(&[("a", "m")]);
        let err = parse("a / missing").unwrap().unit(&cols).unwrap_err();
        assert!(err.contains("no column named 'missing'"), "{err}");
    }

    #[test]
    fn malformed_expressions_are_rejected() {
        assert!(parse("1 +").is_err());
        assert!(parse("(1 + 2").is_err());
        assert!(parse("a $ b").is_err());
        assert!(parse("a ^ 1.5").is_err());
        assert!(parse("1 2").is_err());
    }

    #[test]
    fn columns_are_listed_once_each() {
        let e = parse("(a + b) / a").unwrap();
        assert_eq!(e.columns(), vec!["a", "b"]);
    }
}
