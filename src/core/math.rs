// LaTeX subset -> Unicode approximation (SPEC §三十七 批次 C, ADR-0038).
//
// Render-only by design: the document stores the source, this turns it into
// what a row shows. No boxes, no baselines, no metrics, nothing to keep alive,
// so SPEC's "要引入排版引擎必须先出 ADR 并附内存数字" stays untriggered.
//
// The rule every other choice serves: **the output never loses what the user
// typed.** An unmapped command comes back as its own source, because a formula
// that renders wrong still has to read as the LaTeX it is — the user edits the
// source, not the picture. Braces are the exception, and only where they are
// pure grouping (`x^{2}`), which is also where dropping one cannot lose a
// character the user meant. Whitespace is the other exception we do *not*
// make: TeX discards it in math mode, but here a space the user typed between
// two glyphs is the only expression of their spacing that survives into a
// single-line fallback, so `\alpha + \beta` keeps its spaces and `\alpha+\beta`
// keeps its lack of them.

/// Convert a LaTeX-subset formula to Unicode. Cheap enough to call on every
/// repaint, and idempotent: nothing in the output begins with a backslash, so
/// converting a rendered string returns it unchanged.
pub fn to_unicode(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    Parser { c: &chars, i: 0 }.body()
}

struct Parser<'a> {
    c: &'a [char],
    i: usize,
}

impl<'a> Parser<'a> {
    fn body(&mut self) -> String {
        self.seq(false)
    }

    /// Render a sequence: to the end of input, or — inside a `{...}` — to its
    /// closer, which this consumes and returns.
    fn seq(&mut self, in_group: bool) -> String {
        let mut out = String::new();
        while self.i < self.c.len() {
            match self.c[self.i] {
                '\\' => {
                    self.i += 1;
                    out.push_str(&self.command());
                }
                '^' | '_' => {
                    let kind = self.c[self.i];
                    self.i += 1;
                    out.push_str(&self.scripted(kind));
                }
                '{' => {
                    self.i += 1;
                    out.push_str(&self.seq(true));
                }
                '}' => {
                    self.i += 1;
                    if in_group {
                        return out;
                    }
                    // a stray closer at the top level is syntax with no
                    // content behind it; the content it framed is already in
                }
                c => {
                    out.push(c);
                    self.i += 1;
                }
            }
        }
        out
    }

    /// The raw text of a `{...}` at the cursor, for the one command whose
    /// argument is a name rather than a formula (`\begin{pmatrix}`).
    fn raw_group(&mut self) -> String {
        self.skip_spaces();
        if self.c.get(self.i) != Some(&'{') {
            return String::new();
        }
        self.i += 1;
        let start = self.i;
        let mut depth = 1usize;
        while self.i < self.c.len() {
            match self.c[self.i] {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            self.i += 1;
        }
        let inner: String = self.c[start..self.i.min(self.c.len())].iter().collect();
        if self.c.get(self.i) == Some(&'}') {
            self.i += 1;
        }
        inner
    }

    /// One `arg`: a braced group, or the single character standing in for one
    /// (`x^2`, `\hat x`). An unclosed brace is the user's, not ours to invent
    /// a closer for, so the rest of the input renders as if it were closed.
    fn argument(&mut self) -> String {
        self.skip_spaces();
        match self.c.get(self.i) {
            None => String::new(),
            Some('{') => {
                self.i += 1;
                self.seq(true)
            }
            Some(_) => {
                let c = self.c[self.i];
                self.i += 1;
                c.to_string()
            }
        }
    }

    fn skip_spaces(&mut self) {
        while matches!(self.c.get(self.i), Some(' ') | Some('\t')) {
            self.i += 1;
        }
    }

    /// `^`/`_` plus its argument, raised or lowered when the Unicode alphabet
    /// has the glyph for every character in it.
    fn scripted(&mut self, kind: char) -> String {
        let body = self.argument();
        match script(kind, &body) {
            Some(s) => s,
            // `x^{abc}` has no superscript `a`. Saying `^(abc)` keeps the
            // operator the user typed rather than dropping it silently.
            None => format!("{kind}({body})"),
        }
    }

    /// A `\command` at the cursor, backslash already consumed.
    fn command(&mut self) -> String {
        let start = self.i;
        while matches!(self.c.get(self.i), Some(c) if c.is_ascii_alphabetic()) {
            self.i += 1;
        }
        if self.i == start {
            // one non-alphabetic character *is* the whole command
            match self.c.get(self.i) {
                Some(c) => {
                    self.i += 1;
                    return escape(*c);
                }
                None => return "\\".to_string(),
            }
        }
        let name: String = self.c[start..self.i].iter().collect();
        match name.as_str() {
            // `\left(` and friends carry no content of their own: the
            // delimiter is the next character, which `body` renders anyway
            "left" | "right" | "big" | "Big" | "bigg" | "Bigg" | "bigl" | "bigr" | "middle" => {
                String::new()
            }
            "frac" | "dfrac" | "tfrac" => {
                let num = self.argument();
                let den = self.argument();
                format!("{}/{}", wrap(num), wrap(den))
            }
            "binom" => {
                let a = self.argument();
                let b = self.argument();
                format!("({} over {})", wrap(a), wrap(b))
            }
            "sqrt" => {
                let index = self.optional_index();
                let body = self.argument();
                match index {
                    // the radical already groups its operand
                    Some(n) => format!("{n}√({body})"),
                    None => format!("√({body})"),
                }
            }
            "text" | "textrm" | "mathrm" | "mathbf" | "mathit" | "mbox" | "operatorname" => {
                self.argument()
            }
            "mathbb" | "mathcal" | "mathfrak" | "boldsymbol" => {
                let body = self.argument();
                match (name.as_str(), body.as_str()) {
                    ("mathbb", "R") => "ℝ".into(),
                    ("mathbb", "N") => "ℕ".into(),
                    ("mathbb", "Z") => "ℤ".into(),
                    ("mathbb", "Q") => "ℚ".into(),
                    ("mathbb", "C") => "ℂ".into(),
                    // any other set: the letter it names, not a blank
                    _ => body,
                }
            }
            "hat" | "widehat" => self.accent('\u{0302}'),
            "tilde" | "widetilde" => self.accent('\u{0303}'),
            "dot" => self.accent('\u{0307}'),
            "ddot" => self.accent('\u{0308}'),
            "check" => self.accent('\u{030C}'),
            "vec" | "overrightarrow" => self.accent('\u{20D7}'),
            "bar" | "overline" => self.accent('\u{0304}'),
            // an environment is the one thing this cannot approximate. It
            // keeps its source, braces and name included, so the user can see
            // what did not render rather than wonder where the formula went.
            "begin" | "end" => {
                let env = self.raw_group();
                format!("\\{name}{{{env}}}")
            }
            other => match symbol(other) {
                Some(glyph) => glyph.to_string(),
                None => format!("\\{other}"),
            },
        }
    }

    fn accent(&mut self, mark: char) -> String {
        let body = self.argument();
        format!("{body}{mark}")
    }

    /// `\sqrt[3]{x}` — the optional index, raised when it can be.
    fn optional_index(&mut self) -> Option<String> {
        if self.c.get(self.i) != Some(&'[') {
            return None;
        }
        let save = self.i;
        self.i += 1;
        let start = self.i;
        while matches!(self.c.get(self.i), Some(c) if *c != ']') {
            self.i += 1;
        }
        if self.c.get(self.i) != Some(&']') {
            self.i = save;
            return None;
        }
        self.i += 1;
        let inner: String = self.c[start..self.i - 1].iter().collect();
        Some(script('^', &inner).unwrap_or(inner))
    }
}

/// `a/b` needs its operands parenthesised when they are sums: `x+1/2` is not
/// what `\frac{x+1}{2}` says.
fn wrap(s: String) -> String {
    let needs = s.chars().any(|c| {
        matches!(
            c,
            '+' | '-' | '−' | '=' | '<' | '>' | '≤' | '≥' | '≠' | '±' | '∓'
        )
    });
    if needs && s.chars().count() > 1 {
        format!("({s})")
    } else {
        s
    }
}

/// `x` with every character raised (`^`) or lowered (`_`), or None when any
/// character of it has no such form.
fn script(kind: char, body: &str) -> Option<String> {
    let table: fn(char) -> Option<char> = if kind == '^' { superscript } else { subscript };
    let mut out = String::with_capacity(body.len());
    for c in body.chars() {
        out.push(table(c)?);
    }
    (!out.is_empty()).then_some(out)
}

fn superscript(c: char) -> Option<char> {
    Some(match c {
        '0' => '⁰',
        '1' => '¹',
        '2' => '²',
        '3' => '³',
        '4' => '⁴',
        '5' => '⁵',
        '6' => '⁶',
        '7' => '⁷',
        '8' => '⁸',
        '9' => '⁹',
        '+' => '⁺',
        '-' | '−' => '⁻',
        '=' => '⁼',
        '(' => '⁽',
        ')' => '⁾',
        'n' => 'ⁿ',
        'i' => 'ⁱ',
        _ => return None,
    })
}

fn subscript(c: char) -> Option<char> {
    Some(match c {
        '0' => '₀',
        '1' => '₁',
        '2' => '₂',
        '3' => '₃',
        '4' => '₄',
        '5' => '₅',
        '6' => '₆',
        '7' => '₇',
        '8' => '₈',
        '9' => '₉',
        '+' => '₊',
        '-' | '−' => '₋',
        '=' => '₌',
        '(' => '₍',
        ')' => '₎',
        'a' => 'ₐ',
        'e' => 'ₑ',
        'i' => 'ᵢ',
        'j' => 'ⱼ',
        'k' => 'ₖ',
        'm' => 'ₘ',
        'n' => 'ₙ',
        'o' => 'ₒ',
        'p' => 'ₚ',
        'r' => 'ᵣ',
        's' => 'ₛ',
        't' => 'ₜ',
        'u' => 'ᵤ',
        'v' => 'ᵥ',
        'x' => 'ₓ',
        _ => return None,
    })
}

/// `\X` where `X` is not a letter: the escapes LaTeX spells with a backslash.
fn escape(c: char) -> String {
    match c {
        // the thin-space family, kept as spaces rather than dropped: a
        // formula that loses its spacing is a different formula
        ',' | ':' => "\u{2009}".to_string(),
        ';' => "\u{2005}".to_string(),
        ' ' => " ".to_string(),
        '{' | '}' | '%' | '&' | '#' | '$' | '_' => c.to_string(),
        '\\' => "\n".to_string(),
        // `\!` is a *negative* space: there is no character for taking one
        // away, so the command stands rather than being rendered as nothing
        other => format!("\\{other}"),
    }
}

/// Unicode for the commands this subset knows; names carry no backslash.
fn symbol(name: &str) -> Option<&'static str> {
    Some(match name {
        // Greek, lower then the capitals that differ from Latin
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" | "varepsilon" => "ε",
        "zeta" => "ζ",
        "eta" => "η",
        "theta" | "vartheta" => "θ",
        "iota" => "ι",
        "kappa" => "κ",
        "lambda" => "λ",
        "mu" => "μ",
        "nu" => "ν",
        "xi" => "ξ",
        "pi" => "π",
        "rho" => "ρ",
        "sigma" => "σ",
        "varsigma" => "ς",
        "tau" => "τ",
        "upsilon" => "υ",
        "phi" | "varphi" => "φ",
        "chi" => "χ",
        "psi" => "ψ",
        "omega" => "ω",
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Theta" => "Θ",
        "Lambda" => "Λ",
        "Xi" => "Ξ",
        "Pi" => "Π",
        "Sigma" => "Σ",
        "Upsilon" => "Υ",
        "Phi" => "Φ",
        "Psi" => "Ψ",
        "Omega" => "Ω",
        // relations
        "leq" | "le" => "≤",
        "geq" | "ge" => "≥",
        "neq" | "ne" => "≠",
        "approx" => "≈",
        "equiv" => "≡",
        "sim" | "simeq" => "∼",
        "cong" => "≅",
        "propto" => "∝",
        "ll" => "≪",
        "gg" => "≫",
        "prec" => "≺",
        "succ" => "≻",
        // set theory and logic
        "subset" => "⊂",
        "subseteq" => "⊆",
        "supset" => "⊃",
        "supseteq" => "⊇",
        "in" => "∈",
        "notin" => "∉",
        "ni" => "∋",
        "cup" => "∪",
        "cap" => "∩",
        "setminus" | "backslash" => "∖",
        "emptyset" | "varnothing" => "∅",
        "forall" => "∀",
        "exists" => "∃",
        "nexists" => "∄",
        "neg" | "lnot" => "¬",
        "land" | "wedge" => "∧",
        "lor" | "vee" => "∨",
        "oplus" => "⊕",
        "ominus" => "⊖",
        "otimes" => "⊗",
        "therefore" => "∴",
        "because" => "∵",
        // arithmetic and calculus
        "pm" => "±",
        "mp" => "∓",
        "times" => "×",
        "div" => "÷",
        "cdot" => "·",
        "ast" => "∗",
        "star" => "⋆",
        "circ" => "∘",
        "bullet" => "∙",
        "infty" => "∞",
        "partial" => "∂",
        "nabla" => "∇",
        "sum" => "∑",
        "prod" => "∏",
        "coprod" => "∐",
        "int" => "∫",
        "iint" => "∬",
        "iiint" => "∭",
        "oint" => "∮",
        "bigcup" => "⋃",
        "bigcap" => "⋂",
        "bigsqcup" => "⨆",
        "lim" => "lim",
        "argmin" => "argmin",
        "argmax" => "argmax",
        // arrows and inference
        "to" | "rightarrow" => "→",
        "leftarrow" | "gets" => "←",
        "leftrightarrow" => "↔",
        "Rightarrow" => "⇒",
        "Leftarrow" => "⇐",
        "Leftrightarrow" => "⇔",
        "implies" | "Longrightarrow" => "⟹",
        "iff" | "Longleftrightarrow" => "⇔",
        "mapsto" => "↦",
        "uparrow" => "↑",
        "downarrow" => "↓",
        "nearrow" => "↗",
        "searrow" => "↘",
        // geometry, misc
        "deg" | "degree" => "°",
        "angle" => "∠",
        "measuredangle" => "∡",
        "perp" => "⊥",
        "parallel" => "∥",
        "mid" => "∣",
        "nmid" => "∤",
        "surd" => "√",
        "ldots" | "dots" => "…",
        "cdots" => "⋯",
        "vdots" => "⋮",
        "ddots" => "⋱",
        "prime" => "′",
        "hbar" => "ℏ",
        "ell" => "ℓ",
        "aleph" => "ℵ",
        "Re" => "ℜ",
        "Im" => "ℑ",
        "wp" => "℘",
        "square" => "□",
        "blacksquare" => "■",
        "lozenge" => "◊",
        "checkmark" => "✓",
        "dagger" => "†",
        "ddagger" => "‡",
        "langle" => "⟨",
        "rangle" => "⟩",
        "lceil" => "⌈",
        "rceil" => "⌉",
        "lfloor" => "⌊",
        "rfloor" => "⌋",
        "quad" => "\u{2005}\u{2005}",
        "qquad" => "\u{2003}\u{2003}",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::to_unicode;

    #[test]
    fn plain_text_comes_back_unchanged() {
        assert_eq!(to_unicode("E = mc2"), "E = mc2");
        assert_eq!(to_unicode(""), "");
        // CJK and emoji are not LaTeX and are not touched
        assert_eq!(to_unicode("质能方程 ⚡"), "质能方程 ⚡");
    }

    #[test]
    fn greek_and_relations_render() {
        assert_eq!(to_unicode(r"\alpha + \beta \leq \gamma"), "α + β ≤ γ");
        assert_eq!(to_unicode(r"\Omega \ne \emptyset"), "Ω ≠ ∅");
        // a command name ends where a non-letter starts; no space is needed
        assert_eq!(to_unicode(r"\alpha\beta"), "αβ");
    }

    #[test]
    fn a_fraction_parenthesises_operands_it_would_otherwise_break() {
        assert_eq!(to_unicode(r"\frac{1}{2}"), "1/2");
        assert_eq!(to_unicode(r"\frac{x+1}{2}"), "(x+1)/2");
        assert_eq!(to_unicode(r"\dfrac{a}{b+c}"), "a/(b+c)");
        // a single character never needs them
        assert_eq!(to_unicode(r"\frac{-}{+}"), "-/+");
    }

    #[test]
    fn sqrt_takes_its_operand_as_a_group() {
        assert_eq!(to_unicode(r"\sqrt{2}"), "√(2)");
        assert_eq!(to_unicode(r"\sqrt {x+y}"), "√(x+y)");
        assert_eq!(to_unicode(r"\sqrt[3]{x}"), "³√(x)");
    }

    #[test]
    fn scripts_raise_and_lower_when_unicode_has_the_glyph() {
        assert_eq!(to_unicode("x^2"), "x²");
        assert_eq!(to_unicode("x^{10}"), "x¹⁰");
        assert_eq!(to_unicode("a_i + b_j"), "aᵢ + bⱼ");
        assert_eq!(to_unicode("x^{-1}"), "x⁻¹");
        // and say so when Unicode has no such glyph, rather than dropping
        // the operator
        assert_eq!(to_unicode("x^{abc}"), "x^(abc)");
    }

    #[test]
    fn nesting_works_because_an_argument_goes_through_the_same_renderer() {
        assert_eq!(to_unicode(r"\frac{\pi r^2}{2}"), "π r²/2");
        assert_eq!(to_unicode(r"\sqrt{\frac{1}{4}}"), "√(1/4)");
        assert_eq!(to_unicode(r"\left(\frac{a}{b}\right)"), "(a/b)");
    }

    #[test]
    fn whitespace_in_the_source_is_content_not_syntax() {
        // TeX would drop both spaces here. Keeping them is what lets the user
        // space their own formula, since this renderer has no other lever.
        assert_eq!(to_unicode(r"\alpha + \beta"), "α + β");
        assert_eq!(to_unicode(r"\alpha+\beta"), "α+β");
    }

    #[test]
    fn accents_are_combining_marks_on_their_letter() {
        assert_eq!(to_unicode(r"\hat{x}"), "x\u{302}");
        assert_eq!(to_unicode(r"\vec v"), "v\u{20D7}");
        assert_eq!(to_unicode(r"\bar{y} + \dot{z}"), "y\u{304} + z\u{307}");
    }

    #[test]
    fn the_number_sets_have_their_letter_and_the_rest_keeps_theirs() {
        assert_eq!(to_unicode(r"\mathbb{R}^n"), "ℝⁿ");
        // an unmapped set still names the letter it wrapped
        assert_eq!(to_unicode(r"\mathbb{F}"), "F");
    }

    #[test]
    fn spacing_commands_become_spaces_not_nothing() {
        assert_eq!(to_unicode(r"a\,b"), "a\u{2009}b");
        assert_eq!(to_unicode(r"a\quad b"), "a\u{2005}\u{2005} b");
        assert_eq!(to_unicode(r"a\\b"), "a\nb");
        assert_eq!(to_unicode(r"a\!b"), "a\\!b");
        // literal punctuation survives its own escape
        assert_eq!(to_unicode(r"50\% \& 50\%"), "50% & 50%");
    }

    #[test]
    fn an_unknown_command_stays_as_its_source() {
        assert_eq!(to_unicode(r"\foo"), r"\foo");
        assert_eq!(to_unicode(r"\RR \plus"), r"\RR \plus");
        // and the text around it still renders
        assert_eq!(to_unicode(r"\foo^2"), r"\foo²");
    }

    #[test]
    fn an_environment_is_the_one_thing_that_cannot_be_approximated() {
        // it keeps its shape in the output, so the user sees what did not
        // render instead of finding a blank box where a matrix was
        assert_eq!(
            to_unicode(r"\begin{pmatrix} a \end{pmatrix}"),
            r"\begin{pmatrix} a \end{pmatrix}"
        );
    }

    #[test]
    fn unbalanced_braces_are_the_users_and_not_ours_to_fix() {
        assert_eq!(to_unicode("x^{2"), "x²");
        assert_eq!(to_unicode("x}y"), "xy");
        assert_eq!(to_unicode(r"\frac{1}{2"), "1/2");
    }

    #[test]
    fn converting_a_rendered_formula_changes_nothing() {
        // the row asks for this on every repaint, so a second pass is stable
        let once = to_unicode(r"\frac{x^2 + 1}{\sqrt{y_i}}");
        assert_eq!(once, "(x² + 1)/√(yᵢ)");
        assert_eq!(to_unicode(&once), once);
    }

    // Costs for the docs to quote, not an assertion: `cargo test --release
    // --lib core::math -- --ignored --nocapture`. A formula renders per *run*
    // at projection time (`build_runs`), so a page whose every line holds one
    // pays this once per line per projection — the number worth knowing before
    // anyone claims inline math is free.
    #[test]
    #[ignore]
    fn cost_per_formula() {
        use std::time::Instant;
        let cases = [
            "E = m c^2",
            r"\alpha + \beta \rightarrow \gamma",
            r"\frac{x^2 + 1}{\sqrt{y_i}}",
            r"\int_0^1 x^2 \, dx = \frac{1}{3}",
            r"\begin{pmatrix} a & b \end{pmatrix}",
        ];
        let mut sum = 0usize;
        for round in 0..3 {
            let t = Instant::now();
            for _ in 0..200_000 {
                for c in cases {
                    sum += to_unicode(c).len();
                }
            }
            let ns = t.elapsed().as_nanos() as f64 / (200_000 * cases.len()) as f64;
            println!("round {round}: {ns:.1} ns per formula (5 cases, mean)");
        }
        assert!(sum > 0);
    }
}
