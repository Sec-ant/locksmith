use std::collections::{BTreeMap, BTreeSet};

/// Minimum number of additional releases for one source, with the set of
/// active selectors fixed. Keep the source's version preference within a tie.
pub(super) fn minimum<'a>(domains: &[&'a [String]]) -> Vec<&'a str> {
    let Some(first) = domains.iter().min_by_key(|d| d.len()) else {
        return Vec::new();
    };
    if let Some(release) = first.iter().find(|v| domains.iter().all(|d| d.contains(v))) {
        return vec![release];
    }
    let mut universe: Vec<_> = domains
        .iter()
        .flat_map(|d| d.iter().map(String::as_str))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    universe.sort_by_cached_key(|v| v.parse::<node_semver::Version>().ok());
    let rank: BTreeMap<_, _> = universe.iter().enumerate().map(|(i, &v)| (v, i)).collect();
    let rows: Vec<Vec<_>> = domains
        .iter()
        .map(|domain| domain.iter().map(|v| rank[v.as_str()]).collect())
        .collect();
    let answer = intervals(&rows).unwrap_or_else(|| {
        let mut remaining: Vec<_> = (0..rows.len()).collect();
        remaining.sort_by_key(|&i| rows[i].len());
        let mut best = greedy(&rows, remaining.clone());
        let mut seen = BTreeMap::new();
        search(&rows, &remaining, &mut Vec::new(), &mut best, &mut seen);
        best
    });
    answer.into_iter().map(|i| universe[i]).collect()
}

/// The unhit interval with the earliest right endpoint contributes one
/// disjoint witness. Its right endpoint meets every interval assigned to it.
/// Their intersection may contain a release preferred by the project.
fn intervals(rows: &[Vec<usize>]) -> Option<Vec<usize>> {
    let mut bounds = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let left = *row.iter().min()?;
        let right = *row.iter().max()?;
        if right - left + 1 != row.len() {
            return None;
        }
        bounds.push((right, left, i));
    }
    bounds.sort_unstable();
    let mut groups: Vec<(usize, usize, usize)> = Vec::new();
    for (right, left, i) in bounds {
        if let Some((end, start, _)) = groups.last_mut()
            && left <= *end
        {
            *start = (*start).max(left);
        } else {
            groups.push((right, left, i));
        }
    }
    Some(
        groups
            .into_iter()
            .map(|(end, start, i)| {
                *rows[i]
                    .iter()
                    .find(|v| (start..=end).contains(v))
                    .expect("nonempty intersection of assigned intervals")
            })
            .collect(),
    )
}

fn greedy(rows: &[Vec<usize>], mut remaining: Vec<usize>) -> Vec<usize> {
    let mut cover = Vec::new();
    while !remaining.is_empty() {
        let mut counts = BTreeMap::<usize, usize>::new();
        for &i in &remaining {
            for &v in &rows[i] {
                *counts.entry(v).or_default() += 1;
            }
        }
        let v = *remaining
            .iter()
            .flat_map(|&i| &rows[i])
            .min_by_key(|v| std::cmp::Reverse(counts[v]))
            .expect("nonempty domains");
        remaining.retain(|&i| !rows[i].contains(&v));
        cover.push(v);
    }
    cover
}

fn search(
    rows: &[Vec<usize>],
    remaining: &[usize],
    chosen: &mut Vec<usize>,
    best: &mut Vec<usize>,
    seen: &mut BTreeMap<Vec<usize>, usize>,
) {
    let Some(&first) = remaining.first() else {
        if chosen.len() < best.len() {
            best.clone_from(chosen);
        }
        return;
    };
    let mut occupied = BTreeSet::new();
    let mut lower = chosen.len();
    for &i in remaining {
        if rows[i].iter().all(|v| !occupied.contains(v)) {
            occupied.extend(rows[i].iter().copied());
            lower += 1;
        }
    }
    if lower >= best.len() || seen.get(remaining).is_some_and(|&n| n <= chosen.len()) {
        return;
    }
    seen.insert(remaining.to_vec(), chosen.len());
    let mut alternatives = Vec::new();
    let mut signatures = BTreeSet::new();
    for &v in &rows[first] {
        let rest: Vec<_> = remaining
            .iter()
            .copied()
            .filter(|&i| !rows[i].contains(&v))
            .collect();
        if signatures.insert(rest.clone()) {
            alternatives.push((v, rest));
        }
    }
    alternatives.sort_by_key(|(_, rest)| rest.len());
    for (v, rest) in alternatives {
        chosen.push(v);
        search(rows, &rest, chosen, best, seen);
        chosen.pop();
        if best.len() == lower {
            break;
        }
    }
}
