//! The Plural-Forms rule of a catalog: which msgstr[n] a count selects.
//!
//! The rule is a C expression over `n`, for example
//! `nplurals=3; plural=(n==1 ? 0 : n>=2 && n<=4 ? 1 : 2);`. It is parsed once
//! when the catalog loads.

#[derive(Debug, Clone, PartialEq)]
enum Expr {
    N,
    Number(u64),
    Not(Box<Expr>),
    Binary(Op, Box<Expr>, Box<Expr>),
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plural(Expr);

impl Default for Plural {
    /// The English rule, also the one gettext assumes for a file with no header.
    fn default() -> Self {
        Plural(Expr::Binary(Op::Ne, Box::new(Expr::N), Box::new(Expr::Number(1))))
    }
}

impl Plural {
    /// Reads a Plural-Forms header value. None when the rule does not parse.
    pub fn parse(header: &str) -> Option<Plural> {
        let rule = header.split(';').find_map(|part| part.trim().strip_prefix("plural"))?.trim_start().strip_prefix('=')?;
        let mut parser = Parser { tokens: tokenize(rule)?, at: 0 };
        let expr = parser.ternary()?;
        (parser.at == parser.tokens.len()).then_some(Plural(expr))
    }

    /// The index of the form to use for `n`.
    pub fn index(&self, n: u64) -> usize {
        eval(&self.0, n) as usize
    }
}

fn eval(expr: &Expr, n: u64) -> u64 {
    match expr {
        Expr::N => n,
        Expr::Number(value) => *value,
        Expr::Not(inner) => (eval(inner, n) == 0) as u64,
        Expr::Ternary(test, yes, no) => eval(if eval(test, n) != 0 { yes } else { no }, n),
        Expr::Binary(op, left, right) => {
            let (a, b) = (eval(left, n), eval(right, n));
            match op {
                Op::Or => (a != 0 || b != 0) as u64,
                Op::And => (a != 0 && b != 0) as u64,
                Op::Eq => (a == b) as u64,
                Op::Ne => (a != b) as u64,
                Op::Lt => (a < b) as u64,
                Op::Le => (a <= b) as u64,
                Op::Gt => (a > b) as u64,
                Op::Ge => (a >= b) as u64,
                Op::Add => a.wrapping_add(b),
                Op::Sub => a.wrapping_sub(b),
                Op::Mul => a.wrapping_mul(b),
                // A rule dividing by zero is broken. Form 0 beats a panic.
                Op::Div => a.checked_div(b).unwrap_or(0),
                Op::Rem => a.checked_rem(b).unwrap_or(0),
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Token {
    N,
    Number(u64),
    Op(Op),
    Not,
    Question,
    Colon,
    Open,
    Close,
}

fn tokenize(text: &str) -> Option<Vec<Token>> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let two = bytes.get(at..at + 2).unwrap_or_default();
        let (token, len) = match (bytes[at], two) {
            (c, _) if c.is_ascii_whitespace() => {
                at += 1;
                continue;
            }
            (_, b"||") => (Token::Op(Op::Or), 2),
            (_, b"&&") => (Token::Op(Op::And), 2),
            (_, b"==") => (Token::Op(Op::Eq), 2),
            (_, b"!=") => (Token::Op(Op::Ne), 2),
            (_, b"<=") => (Token::Op(Op::Le), 2),
            (_, b">=") => (Token::Op(Op::Ge), 2),
            (b'<', _) => (Token::Op(Op::Lt), 1),
            (b'>', _) => (Token::Op(Op::Gt), 1),
            (b'+', _) => (Token::Op(Op::Add), 1),
            (b'-', _) => (Token::Op(Op::Sub), 1),
            (b'*', _) => (Token::Op(Op::Mul), 1),
            (b'/', _) => (Token::Op(Op::Div), 1),
            (b'%', _) => (Token::Op(Op::Rem), 1),
            (b'!', _) => (Token::Not, 1),
            (b'?', _) => (Token::Question, 1),
            (b':', _) => (Token::Colon, 1),
            (b'(', _) => (Token::Open, 1),
            (b')', _) => (Token::Close, 1),
            (b'n', _) => (Token::N, 1),
            (c, _) if c.is_ascii_digit() => {
                let len = bytes[at..].iter().take_while(|b| b.is_ascii_digit()).count();
                (Token::Number(text[at..at + len].parse().ok()?), len)
            }
            _ => return None,
        };
        tokens.push(token);
        at += len;
    }
    Some(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
}

/// Binary operators from loosest to tightest, as C ranks them.
const LEVELS: [&[Op]; 6] = [&[Op::Or], &[Op::And], &[Op::Eq, Op::Ne], &[Op::Lt, Op::Le, Op::Gt, Op::Ge], &[Op::Add, Op::Sub], &[Op::Mul, Op::Div, Op::Rem]];

impl Parser {
    fn eat(&mut self, token: Token) -> bool {
        let hit = self.tokens.get(self.at) == Some(&token);
        self.at += hit as usize;
        hit
    }

    fn ternary(&mut self) -> Option<Expr> {
        let test = self.binary(0)?;
        if !self.eat(Token::Question) {
            return Some(test);
        }
        let yes = self.ternary()?;
        if !self.eat(Token::Colon) {
            return None;
        }
        let no = self.ternary()?;
        Some(Expr::Ternary(Box::new(test), Box::new(yes), Box::new(no)))
    }

    fn binary(&mut self, level: usize) -> Option<Expr> {
        let Some(ops) = LEVELS.get(level) else { return self.unary() };
        let mut left = self.binary(level + 1)?;
        while let Some(Token::Op(op)) = self.tokens.get(self.at).copied().filter(|t| matches!(t, Token::Op(op) if ops.contains(op))) {
            self.at += 1;
            let right = self.binary(level + 1)?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Some(left)
    }

    fn unary(&mut self) -> Option<Expr> {
        if self.eat(Token::Not) {
            return Some(Expr::Not(Box::new(self.unary()?)));
        }
        let token = *self.tokens.get(self.at)?;
        self.at += 1;
        match token {
            Token::N => Some(Expr::N),
            Token::Number(value) => Some(Expr::Number(value)),
            Token::Open => {
                let inner = self.ternary()?;
                self.eat(Token::Close).then_some(inner)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forms(header: &str, counts: &[u64]) -> Vec<usize> {
        let plural = Plural::parse(header).unwrap();
        counts.iter().map(|n| plural.index(*n)).collect()
    }

    #[test]
    fn english_and_french() {
        assert_eq!(forms("nplurals=2; plural=(n != 1);", &[0, 1, 2]), [1, 0, 1]);
        assert_eq!(forms("nplurals=2; plural=n > 1;", &[0, 1, 2]), [0, 0, 1]);
        assert_eq!(forms("nplurals=1; plural=0;", &[0, 1, 5]), [0, 0, 0]);
    }

    #[test]
    fn russian() {
        let header = "nplurals=3; plural=(n%10==1 && n%100!=11 ? 0 : n%10>=2 && n%10<=4 && (n%100<10 || n%100>=20) ? 1 : 2);";
        assert_eq!(forms(header, &[1, 2, 5, 11, 21, 22, 25, 111]), [0, 1, 2, 2, 0, 1, 2, 2]);
    }

    #[test]
    fn arabic() {
        let header = "nplurals=6; plural=(n==0 ? 0 : n==1 ? 1 : n==2 ? 2 : n%100>=3 && n%100<=10 ? 3 : n%100>=11 ? 4 : 5);";
        assert_eq!(forms(header, &[0, 1, 2, 3, 10, 11, 99, 100, 102]), [0, 1, 2, 3, 3, 4, 4, 5, 5]);
    }

    #[test]
    fn default_is_the_english_rule() {
        assert_eq!([0, 1, 2].map(|n| Plural::default().index(n)), [1, 0, 1]);
    }

    #[test]
    fn broken_rules_do_not_parse() {
        assert_eq!(Plural::parse("nplurals=2;"), None);
        assert_eq!(Plural::parse("nplurals=2; plural=(n != 1"), None);
        assert_eq!(Plural::parse("nplurals=2; plural=n $ 1;"), None);
        assert_eq!(Plural::parse("nplurals=2; plural=n 1;"), None);
    }
}
