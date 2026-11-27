//! Compare an execution trace recorded on a real device against the emulator's own trace and
//! report the first step where they diverge (PLAN.md F17). Steps are module-relative (a module
//! name and an offset into it), so the device's load addresses need not match the emulator's;
//! the first difference points at the instruction whose behaviour differed between the two.

/// One executed instruction, as a module and an offset into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub module: String,
    pub offset: u64,
}

impl Step {
    pub fn new(module: impl Into<String>, offset: u64) -> Step {
        Step { module: module.into(), offset }
    }
}

/// The first place two traces differ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    /// Index of the first differing step.
    pub index: usize,
    /// The reference step there, or None if the reference trace ended first.
    pub reference: Option<Step>,
    /// The emulator step there, or None if the emulator trace ended first.
    pub actual: Option<Step>,
}

impl std::fmt::Display for Divergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let show = |s: &Option<Step>| match s {
            Some(s) => format!("{}+{:#x}", s.module, s.offset),
            None => "<end of trace>".to_string(),
        };
        write!(f, "step {}: device {} vs emulator {}", self.index, show(&self.reference), show(&self.actual))
    }
}

/// The first step where `actual` (the emulator) departs from `reference` (the device), or None if
/// `actual` follows `reference` for as far as the shorter of the two runs.
pub fn first_divergence(reference: &[Step], actual: &[Step]) -> Option<Divergence> {
    for index in 0..reference.len().max(actual.len()) {
        let r = reference.get(index);
        let a = actual.get(index);
        if r != a {
            return Some(Divergence { index, reference: r.cloned(), actual: a.cloned() });
        }
    }
    None
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("line {line}: expected `module+0xoffset`, got {text:?}")]
    Malformed { line: usize, text: String },
    #[error("line {line}: offset {text:?} is not a hex or decimal number")]
    BadOffset { line: usize, text: String },
}

/// Parses a trace: one `module+0xoffset` per line (hex with `0x`, else decimal). Blank lines and
/// lines starting with `#` are ignored, so a device log can carry comments. A leading field
/// before whitespace (a timestamp or index a logger prints) is skipped.
pub fn parse(text: &str) -> Result<Vec<Step>, ParseError> {
    let mut steps = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Allow "<anything> module+0xoff": take the last whitespace-separated field.
        let token = line.rsplit(char::is_whitespace).next().unwrap_or(line);
        let (module, offset) = token.rsplit_once('+').ok_or_else(|| ParseError::Malformed { line: i + 1, text: token.to_string() })?;
        if module.is_empty() {
            return Err(ParseError::Malformed { line: i + 1, text: token.to_string() });
        }
        let offset = parse_offset(offset).ok_or_else(|| ParseError::BadOffset { line: i + 1, text: offset.to_string() })?;
        steps.push(Step::new(module, offset));
    }
    Ok(steps)
}

fn parse_offset(text: &str) -> Option<u64> {
    match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => text.parse().ok(),
    }
}

/// The text form `parse` reads: one `module+0xoffset` per line.
pub fn to_text(steps: &[Step]) -> String {
    let mut out = String::new();
    for s in steps {
        out.push_str(&format!("{}+{:#x}\n", s.module, s.offset));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_points_at_the_first_difference() {
        let a = vec![Step::new("libx.so", 0x10), Step::new("libx.so", 0x14), Step::new("libx.so", 0x18)];
        assert_eq!(first_divergence(&a, &a), None);

        let mut b = a.clone();
        b[1] = Step::new("libx.so", 0x20); // a data-dependent branch went the other way
        let d = first_divergence(&a, &b).unwrap();
        assert_eq!(d.index, 1);
        assert_eq!(d.reference, Some(Step::new("libx.so", 0x14)));
        assert_eq!(d.actual, Some(Step::new("libx.so", 0x20)));

        // A shorter actual trace diverges where it ends.
        let d = first_divergence(&a, &a[..2]).unwrap();
        assert_eq!((d.index, d.actual), (2, None));
    }

    #[test]
    fn round_trips_through_text() {
        let steps = vec![Step::new("libc.so", 0x1234), Step::new("libfoo.so", 0xab)];
        let parsed = parse(&to_text(&steps)).unwrap();
        assert_eq!(parsed, steps);
        // Tolerates comments, blanks, decimal offsets and a leading field.
        let parsed = parse("# frida stalker\n\n1699999 libc.so+0x10\nlibc.so+32\n").unwrap();
        assert_eq!(parsed, vec![Step::new("libc.so", 0x10), Step::new("libc.so", 32)]);
        assert!(parse("libc.so 0x10").is_err());
    }
}
