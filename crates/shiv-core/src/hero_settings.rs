//! Span-preserving reader and editor for Steam's `citadel_hero_settings.lst`
//! (KV3 text). Ported from the old app's `mod_manager/hero_settings.rs`.
//!
//! Only the values being changed are touched: every other byte (comments,
//! other settings, the `heroes` overrides, line endings) is kept. New keys are
//! added at the end of the `base` object.

use std::collections::BTreeMap;
use std::ops::Range;

/// A settings file with empty `base` and `heroes`, used when there is none.
pub const EMPTY_SETTINGS: &str = "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\n{\n\tbase = {}\n\theroes = {}\n}\n";

/// The text is not a KV3 settings file this parser understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Invalid;

#[derive(Debug)]
struct Field {
    name: String,
    value: Range<usize>,
    children: Option<Vec<Field>>,
}

/// The `base` object of a parsed settings file.
pub struct HeroSettings<'a> {
    source: &'a str,
    fields: Vec<Field>,
    /// Byte index of `base`'s closing brace.
    base_end: usize,
}

const PUNCTUATION: &[u8] = b"{}[]=,";

struct Parser<'a> {
    source: &'a str,
    position: usize,
}

impl Parser<'_> {
    fn token(&mut self) -> Result<Range<usize>, Invalid> {
        loop {
            let rest = &self.source[self.position..];
            let trimmed = rest.trim_start();
            self.position += rest.len() - trimmed.len();
            if trimmed.starts_with("//") {
                self.position += trimmed.find('\n').unwrap_or(trimmed.len());
            } else if trimmed.starts_with("/*") {
                self.position += trimmed.find("*/").ok_or(Invalid)? + 2;
            } else {
                break;
            }
        }
        let start = self.position;
        let bytes = self.source.as_bytes();
        let first = *bytes.get(start).ok_or(Invalid)?;
        self.position += 1;
        if first == b'"' {
            loop {
                match bytes.get(self.position) {
                    Some(b'"') => {
                        self.position += 1;
                        break;
                    }
                    Some(b'\\') if self.position + 1 < bytes.len() => self.position += 2,
                    Some(_) => self.position += 1,
                    None => return Err(Invalid),
                }
            }
        } else if !PUNCTUATION.contains(&first) {
            while let Some(byte) = bytes.get(self.position) {
                if byte.is_ascii_whitespace() || b"{}[]=,\"/".contains(byte) {
                    break;
                }
                self.position += 1;
            }
        }
        // Tokens end on ASCII bytes, so `position` stays on a char boundary.
        if !self.source.is_char_boundary(self.position) {
            return Err(Invalid);
        }
        Ok(start..self.position)
    }

    fn expect(&mut self, expected: &str) -> Result<Range<usize>, Invalid> {
        let token = self.token()?;
        if &self.source[token.clone()] != expected {
            return Err(Invalid);
        }
        Ok(token)
    }

    fn object(&mut self, depth: usize) -> Result<Vec<Field>, Invalid> {
        if depth > 32 {
            return Err(Invalid);
        }
        let mut fields = Vec::<Field>::new();
        loop {
            let key = self.token()?;
            let raw_key = &self.source[key.clone()];
            if raw_key == "}" {
                return Ok(fields);
            }
            let name = if raw_key.starts_with('"') {
                serde_json::from_str::<String>(raw_key).map_err(|_| Invalid)?
            } else if raw_key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                raw_key.to_string()
            } else {
                return Err(Invalid);
            };
            if fields.iter().any(|field| field.name == name) {
                return Err(Invalid);
            }
            self.expect("=")?;
            let mut value = self.token()?;
            let children = match &self.source[value.clone()] {
                "{" => {
                    let children = self.object(depth + 1)?;
                    value.end = self.position;
                    Some(children)
                }
                "}" | "=" | "[" | "]" | "," => return Err(Invalid),
                _ => None,
            };
            fields.push(Field {
                name,
                value,
                children,
            });
        }
    }
}

fn is_key(key: &str) -> bool {
    !key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

impl<'a> HeroSettings<'a> {
    pub fn parse(source: &'a str) -> Result<Self, Invalid> {
        let header = source.trim_start_matches('\u{feff}').trim_start();
        if !header.starts_with("<!-- kv3 encoding:text:") {
            return Err(Invalid);
        }
        let mut parser = Parser {
            source,
            position: source.find("-->").ok_or(Invalid)? + 3,
        };
        parser.expect("{")?;
        let fields = parser.object(0)?;
        if !source[parser.position..].trim().is_empty() {
            return Err(Invalid);
        }
        let base = fields
            .into_iter()
            .find(|field| field.name == "base")
            .ok_or(Invalid)?;
        let fields = base.children.ok_or(Invalid)?;
        Ok(Self {
            source,
            fields,
            base_end: base.value.end - 1,
        })
    }

    /// The raw value token of `key` in `base` (quotes included).
    pub fn get(&self, key: &str) -> Result<Option<&'a str>, Invalid> {
        match self.fields.iter().find(|field| field.name == key) {
            Some(field) if field.children.is_none() => Ok(Some(&self.source[field.value.clone()])),
            Some(_) => Err(Invalid),
            None => Ok(None),
        }
    }

    /// The text with each `changes` key set to its raw value token: existing
    /// values are replaced in place, missing keys are added at the end of
    /// `base` using the file's line ending.
    pub fn update(&self, changes: &BTreeMap<String, String>) -> Result<String, Invalid> {
        let newline = if self.source.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        let mut edits = Vec::new();
        let mut additions = String::new();
        for (key, value) in changes {
            if !is_key(key) {
                return Err(Invalid);
            }
            let mut parser = Parser {
                source: value,
                position: 0,
            };
            let token = parser.token()?;
            if token != (0..value.len())
                || value
                    .bytes()
                    .next()
                    .is_some_and(|b| PUNCTUATION.contains(&b))
            {
                return Err(Invalid);
            }
            match self.fields.iter().find(|field| field.name == *key) {
                Some(field) if field.children.is_none() => {
                    if self.source[field.value.clone()] != *value {
                        edits.push((field.value.clone(), value.clone()));
                    }
                }
                Some(_) => return Err(Invalid),
                None => additions.push_str(&format!("{newline}\t\t{key} = {value}")),
            }
        }
        if !additions.is_empty() {
            // Keep `base`'s closing line as it is when it is on its own line;
            // otherwise (`base = {}`) close it on a new line.
            let before = &self.source[..self.base_end];
            let trailing = before.len() - before.trim_end().len();
            let at = if before[before.len() - trailing..].contains('\n') {
                self.base_end - trailing
            } else {
                additions.push_str(&format!("{newline}\t"));
                self.base_end
            };
            edits.push((at..at, additions));
        }
        edits.sort_by_key(|(range, _)| range.start);
        let mut result = self.source.to_string();
        for (range, value) in edits.into_iter().rev() {
            result.replace_range(range, &value);
        }
        HeroSettings::parse(&result)?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_base_gets_its_own_lines() {
        let s = HeroSettings::parse(EMPTY_SETTINGS).unwrap();
        let changes = BTreeMap::from([("crosshair_pip_gap".to_string(), "\"2\"".to_string())]);
        let out = s.update(&changes).unwrap();
        assert!(out.contains("\tbase = {\n\t\tcrosshair_pip_gap = \"2\"\n\t}\n\theroes = {}"));
        let again = HeroSettings::parse(&out).unwrap();
        assert_eq!(again.get("crosshair_pip_gap").unwrap(), Some("\"2\""));
    }

    #[test]
    fn rejects_what_it_cannot_edit_safely() {
        assert!(HeroSettings::parse("{ base = {} }").is_err());
        let dup = EMPTY_SETTINGS.replace("base = {}", "base = { a = 1 a = 2 }");
        assert!(HeroSettings::parse(&dup).is_err());
        let s = HeroSettings::parse(EMPTY_SETTINGS).unwrap();
        for bad in ["{", "a b", "", "\"open"] {
            let changes = BTreeMap::from([("crosshair_x".to_string(), bad.to_string())]);
            assert!(s.update(&changes).is_err(), "{bad:?}");
        }
        let nested = EMPTY_SETTINGS.replace("base = {}", "base = { crosshair_x = { a = 1 } }");
        let s = HeroSettings::parse(&nested).unwrap();
        assert!(s.get("crosshair_x").is_err());
    }
}
