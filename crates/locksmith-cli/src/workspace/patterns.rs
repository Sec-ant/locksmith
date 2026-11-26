//! Workspace glob variants searched by pnpm's different directory walkers.
use crate::Result;
use glob::Pattern;
use node_semver::Version;
use std::{iter::Peekable, str::CharIndices};

const MAX_ALTERNATIVES: usize = 1024;

fn end_of_class(chars: &mut Peekable<CharIndices<'_>>) -> Option<usize> {
    chars.next_if(|(_, c)| *c == '!');
    // A closing bracket in the first position is a literal class member.
    chars.next()?;
    chars.find(|(_, c)| *c == ']').map(|(end, _)| end + 1)
}

fn normalize(pattern: &str, on_disk: bool) -> Result<String> {
    let mut parts = Vec::new();
    for part in components(pattern) {
        match part {
            "" | "." => (),
            ".." if parts.last().is_some_and(|&p| p != "..") => {
                parts.pop();
            }
            ".." if !on_disk => {
                return Err(format!("workspace glob escapes the root: {pattern:?}").into());
            }
            _ => parts.push(part),
        }
    }
    Ok(parts.join("/"))
}

pub(super) fn expand(
    pattern: &str,
    negative: bool,
    runner: Option<&Version>,
    on_disk: bool,
) -> Result<Vec<String>> {
    let overflow = || format!("too many alternatives in workspace glob {pattern:?}");
    let expanded = brace_expansion::expand_with_max(pattern, MAX_ALTERNATIVES + 1);
    if expanded.len() > MAX_ALTERNATIVES {
        return Err(overflow().into());
    }

    // A slash inside a brace group makes the Rust walker search recursively.
    let mut braces = 0usize;
    let slash_in_group = pattern.chars().any(|ch| {
        match ch {
            '{' => braces += 1,
            '}' => braces = braces.saturating_sub(1),
            '/' => return braces > 0,
            _ => {}
        }
        false
    });
    let mut result = Vec::new();
    for alternative in expanded {
        let alt = normalize(&alternative, on_disk)?;
        if alt.is_empty() {
            continue;
        }
        if result.len() >= MAX_ALTERNATIVES {
            return Err(overflow().into());
        }
        if !alt.contains('[') {
            result.push(alt);
            continue;
        }
        let mut chars = alt.char_indices().peekable();
        let mut classes = Vec::new();
        while let Some((start, _)) = chars.find(|(_, c)| *c == '[') {
            let end = end_of_class(&mut chars)
                .ok_or_else(|| format!("invalid character class in workspace glob {pattern:?}"))?;
            classes.push(start..end);
        }
        if !classes.iter().any(|class| alt[class.clone()].contains('/')) {
            result.push(alt);
            continue;
        }
        let recursive = components(&alt).contains(&"**");
        // Since 12.15 the bounded walker counts separators even inside classes.
        // Its recursive walk and exclusion filter still match the complete glob.
        if !negative
            && !recursive
            && !slash_in_group
            && runner.is_some_and(|v| v >= &Version::from((12, 15, 0)))
        {
            continue;
        }
        if runner.is_none_or(|v| v.major() >= 12) {
            result.push(alt);
            continue;
        }
        // Picomatch also recognizes the written brackets literally. Before 10.6
        // only recursive walks and exclusions can reach a class spanning directories.
        let crosses_dirs =
            negative || recursive || runner.is_some_and(|v| v >= &Version::from((10, 6, 0)));
        let choices =
            javascript_class_paths(&alt, classes, crosses_dirs, MAX_ALTERNATIVES - result.len())
                .ok_or_else(overflow)?;
        result.extend(choices);
    }
    Ok(result)
}

fn javascript_class_paths(
    alt: &str,
    classes: Vec<std::ops::Range<usize>>,
    crosses_dirs: bool,
    limit: usize,
) -> Option<Vec<String>> {
    let mut variants = vec![String::new()];
    let mut last = 0;
    for class in classes {
        let text = &alt[class.clone()];
        let members = &text[1..text.len() - 1];
        let literal = (!members.contains('/') || crosses_dirs)
            && !members.contains([
                '-', '*', '+', '?', '.', '^', '$', '{', '}', '(', ')', '|', '[', ']',
            ]);
        let separator = crosses_dirs
            && members.contains('/')
            && !members.starts_with(['!', '^'])
            && class.start > 0
            && class.end < alt.len()
            && !alt[..class.start].ends_with('/')
            && !alt[class.end..].starts_with('/');
        let mut choices = vec![text.to_owned()];
        if literal {
            choices.push(Pattern::escape(text));
        }
        if separator {
            choices.push("/".to_owned());
        }
        if variants.len() * choices.len() > limit {
            return None;
        }
        let mut next = Vec::new();
        for mut prefix in variants {
            prefix.push_str(&alt[last..class.start]);
            for choice in &choices {
                next.push(format!("{prefix}{choice}"));
            }
        }
        variants = next;
        last = class.end;
    }
    let mut result = Vec::new();
    let parents = alt.split('/').take_while(|p| *p == "..").count();
    for mut variant in variants {
        variant.push_str(&alt[last..]);
        // Only authored parent components may leave the current directory. A slash
        // matched by a class must separate two real names, never a dot or empty component.
        if components(&variant)
            .into_iter()
            .skip(parents)
            .all(|p| !matches!(p, "" | "." | ".."))
        {
            result.push(variant);
        }
    }
    Some(result)
}

/// Split path components without treating a slash inside a glob character class as a separator.
pub(super) fn components(pattern: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut chars = pattern.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        match ch {
            '[' => {
                end_of_class(&mut chars);
            }
            '/' => {
                parts.push(&pattern[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&pattern[start..]);
    parts
}
