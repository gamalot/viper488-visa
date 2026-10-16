//! VISA resource names for the daemon's devices -- GPIB instruments and boards, USBTMC instruments --
//! and the VISA search-expression matcher `viFindRsrc` needs. (`TCPIP` names are `lan/name.rs`.)
//!
//! Each adapter is a GPIB board: `GPIB<board>::<primary>[::<secondary>]::INSTR` for an instrument
//! on it, and `GPIB<board>::INTFC` for the board itself (the interface level). A secondary address
//! is 0..=30, as VISA writes it (not the 96..=126 of the bus byte); a search lists primaries only,
//! since nothing on the bus says which secondaries answer. A USBTMC instrument is
//! `USB<board>::<vid>::<pid>::<serial>[::<interface>]::INSTR`.

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct Instr {
    pub board: u16,
    pub pad: u8,
    /// `backend::NO_SAD` for none.
    pub sad: u8,
}

impl Instr {
    pub fn primary(board: u16, pad: u8) -> Instr {
        Instr { board, pad, sad: crate::backend::NO_SAD }
    }
}

fn address(s: &str) -> Option<u8> {
    let a: u8 = if s.bytes().all(|c| c.is_ascii_digit()) { s.parse().ok()? } else { return None };
    (a <= 30).then_some(a)
}

/// Parse `GPIB[board]::primary[::secondary][::INSTR]` (case-insensitive; board defaults to 0; INSTR
/// is the class a name without one has, as VPP-4.3's grammar has it).
pub fn parse(name: &str) -> Option<Instr> {
    let mut parts: Vec<&str> = name.split("::").collect();
    if parts.len() > 2 && parts.last()?.eq_ignore_ascii_case("INSTR") {
        parts.pop();
    }
    let board = board_of(parts[0])?;
    match parts[1..] {
        [p] => Some(Instr::primary(board, address(p)?)),
        [p, s] => Some(Instr { board, pad: address(p)?, sad: address(s)? }),
        _ => None,
    }
}

/// The instrument a GPIB, USB or ASRL `INSTR` resource name stands for, as the daemon knows it
/// (the interface level, `TCPIP` and anything else is None).
pub fn target_of(name: &str) -> Option<crate::locks::Target> {
    use crate::locks::Target;
    if let Some(i) = parse(name) {
        return Some(Target::Gpib(i.board, i.pad, i.sad));
    }
    if let Some(u) = parse_usb(name) {
        return Some(Target::Usb(u.board));
    }
    parse_asrl(name).map(|(n, _)| Target::Asrl(n))
}

/// `s` without `prefix`, compared without case; None if it does not start so. Cut with `get`, so a
/// name that does not start with ASCII is "no", not a cut through a character (which panics).
pub(crate) fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    s.get(..prefix.len()).filter(|h| h.eq_ignore_ascii_case(prefix)).map(|_| &s[prefix.len()..])
}

/// `GPIB` and an optional board number (default 0), case-insensitive.
fn board_of(head: &str) -> Option<u16> {
    match strip_prefix_ci(head, "GPIB")? {
        "" => Some(0),
        d if d.bytes().all(|c| c.is_ascii_digit()) => d.parse().ok(),
        _ => None,
    }
}

/// Parse `GPIB[board]::INTFC` (case-insensitive; board defaults to 0) to the board number.
pub fn parse_intfc(name: &str) -> Option<u16> {
    let parts: Vec<&str> = name.split("::").collect();
    if parts.len() != 2 || !parts[1].eq_ignore_ascii_case("INTFC") {
        return None;
    }
    board_of(parts[0])
}

pub fn format_intfc(board: u16) -> String {
    format!("GPIB{board}::INTFC")
}

/// The boards whose INTFC resource `expr` matches. A board's interface exists whenever the board
/// does, so these need no probing.
pub fn intfc_candidates(boards: u16, expr: &str) -> Vec<u16> {
    let Some(re) = compile(expr) else { return Vec::new() };
    (0..boards).filter(|b| re.is_match(&format_intfc(*b))).collect()
}

pub fn format(i: Instr) -> String {
    if i.sad == crate::backend::NO_SAD {
        format!("GPIB{}::{}::INSTR", i.board, i.pad)
    } else {
        format!("GPIB{}::{}::{}::INSTR", i.board, i.pad, i.sad)
    }
}

/// A USBTMC instrument's VISA name: `USB[board]::<manufacturer ID>::<model code>::<serial>
/// [::<USB interface number>][::INSTR]`. The IDs are numbers, `0x`-hex or decimal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbName {
    pub board: u16,
    pub vid: u16,
    pub pid: u16,
    pub serial: String,
    pub iface: Option<u8>,
}

fn number(s: &str) -> Option<u16> {
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => u16::from_str_radix(h, 16).ok(),
        None => s.parse().ok(),
    }
}

pub fn parse_usb(name: &str) -> Option<UsbName> {
    let mut parts: Vec<&str> = name.split("::").collect();
    let head = parts.first()?;
    let board = match strip_prefix_ci(head, "USB")? {
        "" => 0,
        d if d.bytes().all(|c| c.is_ascii_digit()) => d.parse().ok()?,
        _ => return None,
    };
    if parts.last().is_some_and(|c| c.eq_ignore_ascii_case("INSTR")) {
        parts.pop();
    }
    let (vid, pid, serial, iface) = match parts.as_slice() {
        [_, v, p, s] => (*v, *p, *s, None),
        [_, v, p, s, i] => (*v, *p, *s, Some(i.parse::<u8>().ok()?)),
        _ => return None,
    };
    if serial.is_empty() || serial.eq_ignore_ascii_case("RAW") {
        return None;
    }
    Some(UsbName { board, vid: number(vid)?, pid: number(pid)?, serial: serial.to_string(), iface })
}

pub fn format_usb(u: &UsbName) -> String {
    let iface = u.iface.map(|i| format!("::{i}")).unwrap_or_default();
    format!("USB{}::0x{:04X}::0x{:04X}::{}{iface}::INSTR", u.board, u.vid, u.pid, u.serial)
}

/// A serial port's VISA name, `ASRL<n>[::INSTR]` (case-insensitive), or `COM<n>`, which is taken as an
/// alias of it, as NI-VISA takes it: the port number and whether the name was the alias. On Linux
/// also `ASRL/dev/ttyACM0[::INSTR]`, the form NI-VISA and pyvisa-py give a Linux port.
pub fn parse_asrl(name: &str) -> Option<(u16, bool)> {
    if let Some(n) = crate::serial::com_number(&name.to_ascii_uppercase()) {
        return Some((n, true));
    }
    let mut parts = name.split("::");
    let head = parts.next()?;
    let d = strip_prefix_ci(head, "ASRL").filter(|d| !d.is_empty())?;
    let n: u16 = if d.starts_with('/') {
        crate::serial::port_number(d)?
    } else if d.bytes().all(|c| c.is_ascii_digit()) {
        d.parse().ok().filter(|n| *n > 0)?
    } else {
        return None;
    };
    match (parts.next(), parts.next()) {
        (None, _) => Some((n, false)),
        (Some(c), None) if c.eq_ignore_ascii_case("INSTR") => Some((n, false)),
        _ => None,
    }
}

/// `ASRL<n>::INSTR`; on Linux a port other than a ttyS by its path, `ASRL/dev/ttyACM0::INSTR`.
pub fn format_asrl(n: u16) -> String {
    if cfg!(target_os = "linux") && n >= 1000 {
        if let Some(k) = crate::serial::tty_name(n) {
            return format!("ASRL/dev/{k}::INSTR");
        }
    }
    format!("ASRL{n}::INSTR")
}

/// Any resource name this runtime has, in its canonical form (`gpib1::6` is `GPIB1::6::INSTR`,
/// `COM3` is `ASRL3::INSTR`); None if it is none of them.
pub fn canonical(name: &str) -> Option<String> {
    if let Some(i) = parse(name) {
        return Some(format(i));
    }
    if let Some(b) = parse_intfc(name) {
        return Some(format_intfc(b));
    }
    if let Some((n, _)) = parse_asrl(name) {
        return Some(format_asrl(n));
    }
    if let Some(u) = parse_usb(name) {
        return Some(format_usb(&u));
    }
    crate::lan::name::parse(name).map(|t| t.name())
}

/// Every address on `boards` boards whose resource name `expr` matches, in board then address order: the only ones a
/// search needs to probe. Probing costs bus time, so a search for one board's instruments does not probe the rest. An
/// expression that is not valid matches nothing here; `valid` is how a caller tells.
pub fn candidates(boards: u16, expr: &str) -> Vec<Instr> {
    let Some(re) = compile(expr) else { return Vec::new() };
    (0..boards)
        .flat_map(|board| (0..=30u8).map(move |pad| Instr::primary(board, pad)))
        .filter(|i| re.is_match(&format(*i)))
        .collect()
}

/// Could `expr` match a GPIB resource on any board a machine plausibly has? If not, a search needs
/// no daemon: `TCPIP?*` lists LAN instruments without starting one (and taking the adapters).
pub fn may_match_gpib(expr: &str) -> bool {
    const BOARDS: u16 = 64;
    !candidates(BOARDS, expr).is_empty() || !intfc_candidates(BOARDS, expr).is_empty()
}

/// Could `expr` match a `TCPIP` name? LAN names are only known once found, so this looks at the
/// literal start of each top-level alternative: `GPIB?*` cannot, `?*::INSTR` and `TCPIP0::?*` can.
/// Discovery (lan/discover.rs) puts packets on the network, so a search that cannot want its
/// results does not run it.
pub fn may_match_lan(expr: &str) -> bool {
    may_match_prefix(expr, "TCPIP")
}

/// Could `expr` match a name that starts with `prefix` (upper case)? The same look at the literal
/// start of each top-level alternative as `may_match_lan`: USBTMC instruments are listed only for a
/// search that could want them.
pub fn may_match_prefix(expr: &str, prefix: &str) -> bool {
    let mut alts = Vec::new();
    let (mut depth, mut class, mut start) = (0i32, false, 0usize);
    let b = expr.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'[' if !class => class = true,
            b']' if class => class = false,
            b'(' if !class => depth += 1,
            b')' if !class => depth -= 1,
            b'|' if !class && depth == 0 => {
                alts.push(&expr[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    alts.push(&expr[start..]);
    alts.iter().any(|a| {
        // The literal characters before anything special, and what a character followed by a
        // quantifier contributes (nothing certain).
        let lit: String = a.chars().take_while(|c| !"?*+[](|)\\{".contains(*c)).collect();
        let lit = match a[lit.len()..].chars().next() {
            // Without its last character (by character: it need not be ASCII).
            Some('*' | '+') => lit.char_indices().last().map_or("", |(i, _)| &lit[..i]),
            _ => &lit[..],
        };
        let (l, t) = (lit.to_ascii_uppercase(), prefix);
        l.starts_with(t) || t.starts_with(&l)
    })
}

/// Whether `expr` is a search expression this implementation accepts: its regular expression,
/// and its attribute expression if it has one.
pub fn valid(expr: &str) -> bool {
    split(expr).is_some_and(|(re, attrs)| compile_regex(re).is_some() && attrs.is_none_or(|a| AttrExpr::parse(a).is_some()))
}

/// The attribute expression of a valid `expr` (`{...}`), if it has one: what a resource whose name
/// matches must also satisfy.
pub fn attr_filter(expr: &str) -> Option<AttrExpr> {
    split(expr).and_then(|(_, a)| a).and_then(AttrExpr::parse)
}

/// A search expression's two parts (VPP-4.3 4.4.4, viFindRsrc): the regular expression over the
/// resource string, and what is between the braces that may follow it. The first `{` that is not
/// escaped or in a `[list]` starts the attribute part, which ends at the last character, a `}`.
/// None for an attribute part that does not end so.
fn split(expr: &str) -> Option<(&str, Option<&str>)> {
    let b = expr.as_bytes();
    let (mut i, mut class) = (0, false);
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'[' if !class => class = true,
            b']' if class => class = false,
            b'{' if !class => {
                let rest = &expr[i + 1..];
                return rest.strip_suffix('}').map(|a| (&expr[..i], Some(a)));
            }
            _ => {}
        }
        i += 1;
    }
    Some((expr, None))
}

/// A viFindRsrc attribute expression (VPP-4.3 4.4.4, Table 4.4.7):
///
/// ```text
/// attrExpr     := attrTerm | attrExpr '||' attrTerm
/// attrTerm     := attrFactor | attrTerm '&&' attrFactor
/// attrFactor   := '(' attrExpr ')' | '!' attrFactor | relationExpr
/// relationExpr := attributeId compareOp numValue | attributeId equalityOp stringValue
/// ```
///
/// An attribute is named as visa.h spells it and must be a global one (RULE 4.4.7: a local one is
/// a session's, not the resource's); `>`, `<`, `>=`, `<=` only compare numbers. A number is decimal,
/// `-` decimal or `0x` hex; a string is between double quotes, compared exactly. A string that is a
/// number may be compared with a numeric attribute (`VI_ATTR_MANF_ID == "0x1AB1"`, as pyvisa's own
/// filter takes it). Anything else -- an unknown or local attribute, a number for a string
/// attribute -- makes the expression invalid (VI_ERROR_INV_EXPR).
#[derive(Debug, Clone, PartialEq)]
pub enum AttrExpr {
    Or(Box<AttrExpr>, Box<AttrExpr>),
    And(Box<AttrExpr>, Box<AttrExpr>),
    Not(Box<AttrExpr>),
    Rel { id: u32, kind: crate::attrs::Kind, op: CmpOp, value: AttrValue },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Gt,
    Lt,
    Ge,
    Le,
}

/// An attribute's value, as an expression states it or a resource has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttrValue {
    Num(i128),
    Str(String),
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Open,
    Close,
    Not,
    And,
    Or,
    Cmp(CmpOp),
    Id(String),
    Num(i128),
    Str(String),
}

fn attr_number(s: &str) -> Option<i128> {
    let (neg, digits) = match s.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, s),
    };
    let v = match digits.strip_prefix("0x").or_else(|| digits.strip_prefix("0X")) {
        Some(h) if !neg && !h.is_empty() && h.bytes().all(|c| c.is_ascii_hexdigit()) => u64::from_str_radix(h, 16).ok()? as i128,
        Some(_) => return None,
        None if !digits.is_empty() && digits.bytes().all(|c| c.is_ascii_digit()) => digits.parse::<u64>().ok()? as i128,
        None => return None,
    };
    Some(if neg { -v } else { v })
}

fn tokens(s: &str) -> Option<Vec<Tok>> {
    let b = s.as_bytes();
    let (mut out, mut i) = (Vec::new(), 0);
    while i < b.len() {
        let two = &b[i..(i + 2).min(b.len())];
        let (t, n) = match b[i] {
            c if c.is_ascii_whitespace() => {
                i += 1;
                continue;
            }
            b'(' => (Tok::Open, 1),
            b')' => (Tok::Close, 1),
            _ if two == b"&&" => (Tok::And, 2),
            _ if two == b"||" => (Tok::Or, 2),
            _ if two == b"==" => (Tok::Cmp(CmpOp::Eq), 2),
            _ if two == b"!=" => (Tok::Cmp(CmpOp::Ne), 2),
            _ if two == b">=" => (Tok::Cmp(CmpOp::Ge), 2),
            _ if two == b"<=" => (Tok::Cmp(CmpOp::Le), 2),
            b'>' => (Tok::Cmp(CmpOp::Gt), 1),
            b'<' => (Tok::Cmp(CmpOp::Lt), 1),
            b'!' => (Tok::Not, 1),
            b'"' => {
                let end = s[i + 1..].find('"')? + i + 1;
                (Tok::Str(s[i + 1..end].to_string()), end + 1 - i)
            }
            c if c.is_ascii_alphabetic() => {
                let n = b[i..].iter().take_while(|c| c.is_ascii_alphanumeric() || **c == b'_').count();
                (Tok::Id(s[i..i + n].to_string()), n)
            }
            c if c.is_ascii_digit() || c == b'-' => {
                let n = 1 + b[i + 1..].iter().take_while(|c| c.is_ascii_alphanumeric()).count();
                (Tok::Num(attr_number(&s[i..i + n])?), n)
            }
            _ => return None,
        };
        out.push(t);
        i += n;
    }
    Some(out)
}

/// How deep `(` and `!` may nest in an attribute expression. The parser and the evaluation recurse,
/// so without a bound an expression like `!(!(!(...` would overflow the stack and abort the program
/// that asked; deeper than this is not an expression anyone writes.
const ATTR_DEPTH: usize = 64;
/// And how many tokens it may have: `a || b || ...` is a tree as deep as it is long.
const ATTR_TOKENS: usize = 1024;

struct Parser {
    toks: Vec<Tok>,
    at: usize,
    depth: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.at)
    }
    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.at).cloned();
        self.at += 1;
        t
    }
    fn expr(&mut self) -> Option<AttrExpr> {
        let mut e = self.term()?;
        while self.peek() == Some(&Tok::Or) {
            self.at += 1;
            e = AttrExpr::Or(Box::new(e), Box::new(self.term()?));
        }
        Some(e)
    }
    fn term(&mut self) -> Option<AttrExpr> {
        let mut e = self.factor()?;
        while self.peek() == Some(&Tok::And) {
            self.at += 1;
            e = AttrExpr::And(Box::new(e), Box::new(self.factor()?));
        }
        Some(e)
    }
    fn factor(&mut self) -> Option<AttrExpr> {
        match self.next()? {
            Tok::Open | Tok::Not if self.depth >= ATTR_DEPTH => None,
            Tok::Open => {
                self.depth += 1;
                let e = self.expr()?;
                self.depth -= 1;
                (self.next()? == Tok::Close).then_some(e)
            }
            Tok::Not => {
                self.depth += 1;
                let e = self.factor()?;
                self.depth -= 1;
                Some(AttrExpr::Not(Box::new(e)))
            }
            Tok::Id(name) => {
                use crate::attrs::Kind;
                let (id, kind, local) = crate::attrs::by_name(&name)?;
                let Tok::Cmp(op) = self.next()? else { return None };
                let value = match (self.next()?, kind) {
                    (_, Kind::Ptr) => return None,
                    (Tok::Str(s), Kind::Str) => AttrValue::Str(s),
                    (Tok::Str(s), _) => AttrValue::Num(attr_number(s.trim())?),
                    (Tok::Num(n), k) if k != Kind::Str => AttrValue::Num(n),
                    _ => return None,
                };
                let ordering = !matches!(op, CmpOp::Eq | CmpOp::Ne);
                if local || (ordering && kind == Kind::Str) {
                    return None;
                }
                Some(AttrExpr::Rel { id, kind, op, value })
            }
            _ => None,
        }
    }
}

impl AttrExpr {
    /// What is between the braces; None if it is not a valid attribute expression.
    pub fn parse(s: &str) -> Option<AttrExpr> {
        let toks = tokens(s).filter(|t| t.len() <= ATTR_TOKENS)?;
        let mut p = Parser { toks, at: 0, depth: 0 };
        let e = p.expr()?;
        (p.at == p.toks.len()).then_some(e)
    }

    /// The attributes it names, each once, with their kinds: what has to be read from a resource.
    pub fn attrs(&self) -> Vec<(u32, crate::attrs::Kind)> {
        let mut v = Vec::new();
        self.collect(&mut v);
        v
    }

    fn collect(&self, v: &mut Vec<(u32, crate::attrs::Kind)>) {
        match self {
            AttrExpr::Or(a, b) | AttrExpr::And(a, b) => {
                a.collect(v);
                b.collect(v);
            }
            AttrExpr::Not(a) => a.collect(v),
            AttrExpr::Rel { id, kind, .. } => {
                if !v.iter().any(|(i, _)| i == id) {
                    v.push((*id, *kind));
                }
            }
        }
    }

    /// Does a resource whose attributes are `value` satisfy it? `value` has every attribute
    /// `attrs` names (a resource without one of them is not a match at all, which is the caller's).
    pub fn eval(&self, value: &dyn Fn(u32) -> Option<AttrValue>) -> bool {
        match self {
            AttrExpr::Or(a, b) => a.eval(value) || b.eval(value),
            AttrExpr::And(a, b) => a.eval(value) && b.eval(value),
            AttrExpr::Not(a) => !a.eval(value),
            AttrExpr::Rel { id, op, value: want, .. } => {
                let Some(have) = value(*id) else { return false };
                let ord = match (&have, want) {
                    (AttrValue::Num(h), AttrValue::Num(w)) => h.cmp(w),
                    (AttrValue::Str(h), AttrValue::Str(w)) => h.as_str().cmp(w.as_str()),
                    _ => return false,
                };
                use std::cmp::Ordering::*;
                match op {
                    CmpOp::Eq => ord == Equal,
                    CmpOp::Ne => ord != Equal,
                    CmpOp::Gt => ord == Greater,
                    CmpOp::Lt => ord == Less,
                    CmpOp::Ge => ord != Less,
                    CmpOp::Le => ord != Greater,
                }
            }
        }
    }
}

/// Does the search expression `expr` match the whole of `s`?
pub fn matches(expr: &str, s: &str) -> bool {
    compile(expr).is_some_and(|re| re.is_match(s))
}

/// A VISA search expression (VPP-4.3's regular expressions, as viFindRsrc takes them) as a regex.
///
/// VISA's `?` is any one character; `*` and `+` repeat the character or group BEFORE them, as in any regex -- `?*` is
/// "anything", while `GPIB*` is "GPI" and then any number of B, not "starts with GPIB". `[list]`, `[^list]`, `(exp)`,
/// `exp|exp` and `\` escapes mean what they do in a regex. Everything else is literal, including the characters a regex
/// would treat specially. The match is case-insensitive (`gpib0::20::instr` is the same resource as `GPIB0::20::INSTR`)
/// and covers the whole resource string. The attribute part (`{...}`) is not the regex's: `split` takes it off, and the
/// caller filters by it (`attr_filter`).
fn compile(expr: &str) -> Option<regex_lite::Regex> {
    compile_regex(split(expr)?.0)
}

fn compile_regex(expr: &str) -> Option<regex_lite::Regex> {
    let mut re = String::from("(?i)^(?:");
    let mut chars = expr.chars();
    let mut in_class = false;
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                let next = chars.next()?;
                re.push_str(&regex_lite::escape(&next.to_string()));
            }
            '[' if !in_class => {
                in_class = true;
                re.push('[');
                // A leading `^` negates the class, as it does in both syntaxes.
            }
            ']' if in_class => {
                in_class = false;
                re.push(']');
            }
            _ if in_class => {
                if c == '^' || c == '-' {
                    re.push(c);
                } else {
                    re.push_str(&regex_lite::escape(&c.to_string()));
                }
            }
            // The attribute part is split off before this (`split`).
            '{' => return None,
            '?' => re.push('.'),
            '*' | '+' | '|' | '(' | ')' => re.push(c),
            _ => re.push_str(&regex_lite::escape(&c.to_string())),
        }
    }
    if in_class {
        return None;
    }
    re.push_str(")$");
    regex_lite::Regex::new(&re).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serial_names() {
        assert_eq!(parse_asrl("ASRL3::INSTR"), Some((3, false)));
        assert_eq!(parse_asrl("asrl12::instr"), Some((12, false)));
        assert_eq!(parse_asrl("ASRL3"), Some((3, false)));
        assert_eq!(parse_asrl("COM3"), Some((3, true)));
        assert_eq!(parse_asrl("com7"), Some((7, true)));
        assert_eq!(parse_asrl("ASRL0::INSTR"), None);
        assert_eq!(parse_asrl("ASRL::INSTR"), None);
        assert_eq!(parse_asrl("ASRL3::INTFC"), None);
        if cfg!(target_os = "linux") {
            assert_eq!(parse_asrl("ASRL/dev/ttyUSB0::INSTR"), Some((1000, false)));
            assert_eq!(parse_asrl("ASRL/dev/ttyACM2"), Some((2002, false)));
            assert_eq!(format_asrl(2002), "ASRL/dev/ttyACM2::INSTR");
            assert_eq!(format_asrl(1), "ASRL1::INSTR", "ttyS0 keeps NI-VISA's number");
            assert!(matches("ASRL?*::INSTR", &format_asrl(2002)));
        } else {
            assert_eq!(parse_asrl("ASRL/dev/ttyUSB0::INSTR"), None);
        }
        assert_eq!(parse_asrl("ASRL/dev/null::INSTR"), None);
        assert_eq!(parse_asrl("GPIB0::3::INSTR"), None);
        assert_eq!(format_asrl(3), "ASRL3::INSTR");
        assert!(matches("ASRL?*::INSTR", &format_asrl(3)));
        assert!(matches("?*", &format_asrl(3)));
    }

    #[test]
    fn parse_and_format() {
        assert_eq!(parse("GPIB0::20::INSTR"), Some(Instr::primary(0, 20)));
        assert_eq!(parse("gpib3::5::instr"), Some(Instr::primary(3, 5)));
        assert_eq!(parse("GPIB::7::INSTR"), Some(Instr::primary(0, 7)));
        assert_eq!(format(Instr::primary(1, 29)), "GPIB1::29::INSTR");
        // A secondary address, with and without the class; VISA writes it 0..=30.
        assert_eq!(parse("GPIB0::20::5::INSTR"), Some(Instr { board: 0, pad: 20, sad: 5 }));
        assert_eq!(parse("gpib1::3::0"), Some(Instr { board: 1, pad: 3, sad: 0 }));
        assert_eq!(format(Instr { board: 2, pad: 9, sad: 30 }), "GPIB2::9::30::INSTR");
        assert_eq!(canonical("gpib2::9::30"), Some("GPIB2::9::30::INSTR".into()));
        for bad in ["GPIB0::31::INSTR", "GPIB0::20::31::INSTR", "GPIB0::20::96::INSTR", "GPIB0::20::5::6::INSTR",
                    "GPIB0::INTFC", "GPIB0::5::INTFC", "TCPIP0::20::INSTR", "GPIBx::1::INSTR", "GPIB0::a::INSTR",
                    "GPIB0::+5::INSTR", "GPIB0::5::INSTR::INSTR", ""] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn search_expressions() {
        assert!(matches("?*::INSTR", "GPIB0::20::INSTR"));
        assert!(matches("GPIB?*INSTR", "GPIB0::20::INSTR"));
        assert!(matches("GPIB0::?*::INSTR", "GPIB0::20::INSTR"));
        assert!(!matches("GPIB1::?*::INSTR", "GPIB0::20::INSTR"));
        assert!(matches("USB?*INSTR|GPIB?*INSTR", "GPIB0::20::INSTR"));
        assert!(!matches("?*::INSTR", "GPIB0::INTFC"));
        assert!(!matches("GPIB0::2?::INSTR", "GPIB0::5::INSTR"));
    }

    #[test]
    fn search_expressions_are_visa_regular_expressions() {
        // Case does not matter.
        assert!(matches("gpib0::?*::instr", "GPIB0::20::INSTR"));
        assert!(matches("GPIB?*INSTR", "gpib0::20::instr"));
        // `*` repeats what precedes it; it is not "any run of characters".
        assert!(!matches("GPIB0::*::INSTR", "GPIB0::20::INSTR"));
        assert!(!matches("GPIB*", "GPIB0::20::INSTR"));
        assert!(matches("GPIB0::20*::INSTR", "GPIB0::2::INSTR"));
        assert!(matches("GPIB0::20+::INSTR", "GPIB0::200::INSTR"));
        assert!(!matches("GPIB0::20+::INSTR", "GPIB0::2::INSTR"));
        // Classes, negated classes, ranges, groups.
        assert!(matches("GPIB[01]::?*", "GPIB1::6::INSTR"));
        assert!(!matches("GPIB[01]::?*", "GPIB2::6::INSTR"));
        assert!(matches("GPIB[^0]::?*", "GPIB2::6::INSTR"));
        assert!(matches("GPIB0::[1-2]?::INSTR", "GPIB0::20::INSTR"));
        assert!(matches("GPIB0::(10|20)::INSTR", "GPIB0::20::INSTR"));
        assert!(!matches("GPIB0::(10|20)::INSTR", "GPIB0::29::INSTR"));
        // The whole string, not a prefix.
        assert!(!matches("GPIB0", "GPIB0::20::INSTR"));
        // A backslash makes the next character literal; a regex's special characters are literal.
        assert!(!matches("GPIB0::2\\?::INSTR", "GPIB0::20::INSTR"));
        assert!(matches("GPIB0::2\\?::INSTR", "GPIB0::2?::INSTR"));
        assert!(!matches("GPIB0.::?*", "GPIB01::5::INSTR"));
        // Not valid: an open class, an unbalanced group, a trailing backslash.
        for bad in ["GPIB[0", "GPIB0::(20", "GPIB0\\"] {
            assert!(!valid(bad), "{bad}");
        }
        assert!(valid("?*::INSTR"));
    }

    #[test]
    fn the_attribute_part() {
        use crate::attrs::Kind;
        // VPP-4.3's own examples (Table 4.4.8; the VXI one's attributes are global there too).
        assert!(valid("GPIB[0-9]*::?*::?*::INSTR{VI_ATTR_GPIB_SECONDARY_ADDR > 0}"));
        assert!(valid("ASRL?*INSTR{VI_ATTR_ASRL_BAUD == 9600}"));
        assert!(valid("?*VXI?*INSTR{VI_ATTR_MANF_ID == 0xFF6 && !(VI_ATTR_VXI_LA == 0 || VI_ATTR_SLOT <= 0)}"));
        // The regular expression still decides the candidates, and only it: GPIB0 is searched, GPIB1 not.
        let e = "GPIB0::?*{VI_ATTR_GPIB_PRIMARY_ADDR == 20}";
        assert!(matches(e, "GPIB0::20::INSTR") && matches(e, "GPIB0::5::INSTR") && !matches(e, "GPIB1::20::INSTR"));
        assert_eq!(candidates(2, e).len(), 31);
        assert!(!may_match_lan(e));
        // A brace inside a list or escaped is the regex's own character.
        assert_eq!(split("A[{]B"), Some(("A[{]B", None)));
        assert_eq!(split("A\\{B"), Some(("A\\{B", None)));
        assert_eq!(split("?*{VI_ATTR_INTF_NUM==0}"), Some(("?*", Some("VI_ATTR_INTF_NUM==0"))));

        let f = attr_filter("?*{VI_ATTR_INTF_NUM == 0 && (VI_ATTR_MANF_NAME == \"Keithley\" || !(VI_ATTR_GPIB_PRIMARY_ADDR < 10))}").unwrap();
        assert_eq!(f.attrs(), [(0x3FFF0176, Kind::U16), (0xBFFF0072, Kind::Str), (0x3FFF0172, Kind::U16)]);
        let has = |intf: i128, manf: &str, pad: i128| {
            let manf = manf.to_string();
            move |id: u32| match id {
                0x3FFF0176 => Some(AttrValue::Num(intf)),
                0xBFFF0072 => Some(AttrValue::Str(manf.clone())),
                0x3FFF0172 => Some(AttrValue::Num(pad)),
                _ => None,
            }
        };
        assert!(f.eval(&has(0, "Keithley", 5)), "the manufacturer");
        assert!(f.eval(&has(0, "Rigol", 15)), "an address of 10 or more");
        assert!(!f.eval(&has(0, "Rigol", 5)));
        assert!(!f.eval(&has(1, "Keithley", 15)), "&& binds tighter than ||, and the board is first");
        assert!(!f.eval(&has(0, "keithley", 5)), "a string is compared exactly");
        // || is the lowest: a || b && c is a || (b && c).
        let g = attr_filter("?*{VI_ATTR_INTF_NUM == 1 || VI_ATTR_INTF_NUM == 0 && VI_ATTR_GPIB_PRIMARY_ADDR == 3}").unwrap();
        assert!(g.eval(&has(1, "", 9)) && g.eval(&has(0, "", 3)) && !g.eval(&has(0, "", 9)));
        // Numbers: hex, negative, and a quoted number for a numeric attribute (as pyvisa's filter takes it).
        let h = attr_filter("?*{VI_ATTR_MANF_ID == \"0x1AB1\" && VI_ATTR_SLOT > -1 && VI_ATTR_MODEL_CODE != 0X10}").unwrap();
        let v = |id: u32| Some(AttrValue::Num(match id { 0x3FFF00D9 => 0x1AB1, 0x3FFF00E8 => 0, _ => 7 }));
        assert!(h.eval(&v));

        // Not valid: an unknown or local attribute (RULE 4.4.7), an ordering of strings, a number for a
        // string, a pointer, no operator, an unclosed group or string, something left over, no closing brace.
        for bad in [
            "?*{VI_ATTR_NONESUCH == 1}",
            "?*{VI_ATTR_TMO_VALUE == 2000}",
            "?*{VI_ATTR_TERMCHAR == 10}",
            "?*{VI_ATTR_MANF_NAME > \"A\"}",
            "?*{VI_ATTR_MANF_NAME == 5}",
            "?*{VI_ATTR_MANF_ID == \"Rigol\"}",
            "?*{VI_ATTR_GPIB_PRIMARY_ADDR}",
            "?*{(VI_ATTR_GPIB_PRIMARY_ADDR == 1}",
            "?*{VI_ATTR_MANF_NAME == \"x}",
            "?*{VI_ATTR_GPIB_PRIMARY_ADDR == 1 VI_ATTR_INTF_NUM == 0}",
            "?*{VI_ATTR_GPIB_PRIMARY_ADDR == 0x}",
            "?*{VI_ATTR_GPIB_PRIMARY_ADDR == 1",
            "?*{}",
        ] {
            assert!(!valid(bad), "{bad}");
        }
    }

    #[test]
    fn a_long_pathological_expression_does_not_take_forever() {
        // A pattern that takes exponential time in a backtracking matcher.
        let expr = "?*?*?*?*?*?*?*?*?*?*?*?*?*?*?*?*?*?*?*?*X";
        let t = std::time::Instant::now();
        assert!(!matches(expr, "GPIB0::20::INSTR"));
        assert!(t.elapsed() < std::time::Duration::from_millis(200));
    }

    #[test]
    fn a_search_probes_only_the_addresses_it_can_match() {
        let all = candidates(3, "?*INSTR");
        assert_eq!(all.len(), 3 * 31);
        let one_board = candidates(3, "GPIB1::?*::INSTR");
        assert_eq!(one_board.len(), 31);
        assert!(one_board.iter().all(|i| i.board == 1));
        let one = candidates(3, "GPIB1::20::INSTR");
        assert_eq!(one.len(), 1);
        assert_eq!((one[0].board, one[0].pad), (1, 20));
        let two = candidates(3, "GPIB0::10::INSTR|GPIB2::6::INSTR");
        assert_eq!(two.iter().map(|i| (i.board, i.pad)).collect::<Vec<_>>(), vec![(0, 10), (2, 6)]);
        assert!(candidates(3, "GPIB7::?*").is_empty());
    }

    #[test]
    fn which_searches_could_want_lan_instruments() {
        for e in ["?*::INSTR", "?*", "TCPIP?*", "tcpip0::?*::INSTR", "TCP?*", "GPIB?*|TCPIP?*", "(GPIB|TCPIP)?*", "[GT]?*", "TCPIP0::198.18.68.63::inst0::INSTR"] {
            assert!(may_match_lan(e), "{e}");
        }
        for e in ["GPIB?*", "GPIB0::?*::INSTR", "USB?*|GPIB?*", "ASRL?*", "GPIB?*INTFC"] {
            assert!(!may_match_lan(e), "{e}");
        }
    }

    #[test]
    fn usb_names() {
        let u = parse_usb("USB0::0xF4EC::0x1103::SDG1EXAMPLE001::INSTR").unwrap();
        assert_eq!((u.board, u.vid, u.pid, u.serial.as_str(), u.iface), (0, 0xF4EC, 0x1103, "SDG1EXAMPLE001", None));
        assert_eq!(format_usb(&u), "USB0::0xF4EC::0x1103::SDG1EXAMPLE001::INSTR");
        let d = parse_usb("usb1::62700::4355::SN::0").unwrap();
        assert_eq!((d.board, d.vid, d.pid, d.iface), (1, 0xF4EC, 0x1103, Some(0)));
        assert_eq!(format_usb(&d), "USB1::0xF4EC::0x1103::SN::0::INSTR");
        assert_eq!(parse_usb("USB::0x0957::0x2C07::MY1").map(|u| u.board), Some(0));
        for bad in ["USB0::0xF4EC::0x1103", "USB0::0xF4EC::0x1103::::INSTR", "USB0::0xZZ::0x1103::S::INSTR",
                    "USB0::0xF4EC::0x1103::S::x::INSTR", "USB0::0xF4EC::0x1103::RAW", "GPIB0::5::INSTR", "USBx::1::2::S"] {
            assert_eq!(parse_usb(bad), None, "{bad}");
        }
        assert!(may_match_prefix("?*::INSTR", "USB") && may_match_prefix("USB?*", "USB"));
        assert!(!may_match_prefix("GPIB?*", "USB") && !may_match_prefix("TCPIP?*", "USB"));
    }

    #[test]
    fn which_searches_need_the_daemon() {
        for e in ["?*::INSTR", "?*", "GPIB?*", "?*INTFC", "GPIB3::5::INSTR", "(GPIB|TCPIP)?*"] {
            assert!(may_match_gpib(e), "{e}");
        }
        for e in ["TCPIP?*", "TCPIP?*::INSTR", "?*SOCKET", "ASRL?*"] {
            assert!(!may_match_gpib(e), "{e}");
        }
    }

    #[test]
    fn interface_names() {
        assert_eq!(parse_intfc("GPIB0::INTFC"), Some(0));
        assert_eq!(parse_intfc("gpib2::intfc"), Some(2));
        assert_eq!(parse_intfc("GPIB::INTFC"), Some(0));
        assert_eq!(parse_intfc("GPIB0::20::INSTR"), None);
        assert_eq!(parse("GPIB0::INTFC"), None, "an interface is not an instrument");
        assert_eq!(format_intfc(1), "GPIB1::INTFC");
        assert_eq!(intfc_candidates(3, "?*INTFC"), vec![0, 1, 2]);
        assert_eq!(intfc_candidates(3, "GPIB1::INTFC"), vec![1]);
        assert!(intfc_candidates(3, "?*::INSTR").is_empty(), "the default search is for instruments only");
        assert!(candidates(3, "?*INTFC").is_empty(), "and an interface search probes no instrument");
    }
}

