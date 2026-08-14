// SPDX-License-Identifier: Apache-2.0
//! ANSI and terminal-control stripping for untrusted check output.

const ESC: char = '\u{1b}';
const BEL: char = '\u{07}';

/// Remove CSI, OSC, two-byte escape sequences, and stray controls.
/// Newlines and tabs are preserved.
#[must_use]
pub fn strip_ansi(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        if character == ESC {
            consume_escape(&mut chars);
        } else if !is_strippable_control(character) {
            output.push(character);
        }
    }
    output
}

fn consume_escape<I>(chars: &mut std::iter::Peekable<I>)
where
    I: Iterator<Item = char>,
{
    match chars.peek().copied() {
        Some('[') => {
            chars.next();
            for character in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&character) {
                    break;
                }
            }
        }
        Some(']') => {
            chars.next();
            while let Some(character) = chars.next() {
                if character == BEL {
                    break;
                }
                if character == ESC {
                    if chars.peek() == Some(&'\\') {
                        chars.next();
                    }
                    break;
                }
            }
        }
        Some(character) if ('\u{20}'..='\u{7e}').contains(&character) => {
            chars.next();
            if matches!(character, '(' | ')' | '*' | '+' | '%' | '#') {
                chars.next();
            }
        }
        _ => {}
    }
}

fn is_strippable_control(character: char) -> bool {
    (character.is_control() && character != '\n' && character != '\t') || character == '\u{7f}'
}
