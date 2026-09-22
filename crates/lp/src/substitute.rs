//! Substitution of the loop variable inside a command block.
//!
//! The variable can be referenced as `$var`, `${var}` or `%var%`. Bare `$var`
//! only matches when not followed by another identifier character, so `$x`
//! will not clobber a longer name such as `$xyz`.

/// Replace every reference to `var` in `block` with `value`.
pub fn apply(block: &str, var: &str, value: &str) -> String {
    if var.is_empty() {
        return block.to_string();
    }

    let chars: Vec<char> = block.chars().collect();
    let name: Vec<char> = var.chars().collect();
    let mut out = String::with_capacity(block.len());
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '$' || chars[i] == '%' {
            if let Some(next) = match_reference(&chars, i, &name) {
                out.push_str(value);
                i = next;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }

    out
}

/// Try to match a variable reference starting at `start`.
///
/// Returns the index just past the reference when it matches.
fn match_reference(chars: &[char], start: usize, name: &[char]) -> Option<usize> {
    let sigil = chars[start];
    let mut j = start + 1;
    let braced = sigil == '$' && chars.get(j) == Some(&'{');

    if braced {
        j += 1;
    }

    if chars.get(j..j + name.len()) != Some(name) {
        return None;
    }

    let after = j + name.len();

    match sigil {
        '$' if braced => {
            (chars.get(after) == Some(&'}')).then_some(after + 1)
        }
        '$' => {
            let boundary = !chars
                .get(after)
                .is_some_and(|c| c.is_alphanumeric() || *c == '_');
            boundary.then_some(after)
        }
        '%' => (chars.get(after) == Some(&'%')).then_some(after + 1),
        _ => None,
    }
}
