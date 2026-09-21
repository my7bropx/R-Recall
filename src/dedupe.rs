//! Finding and resolving duplicate entries.
//!
//! Duplicates arise when the same source is imported twice by tools that
//! serialise it slightly differently — an older binary without import
//! de-duplication, or a different importer. Because those copies are not
//! always byte-identical, the match key is selectable.

use crate::models::Entry;

/// What makes two entries "the same".
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DupKey {
    /// Identical title AND identical content. Safest.
    Exact,
    /// Identical title, ignoring case and surrounding space. Content may differ,
    /// so pair this with `--keep longest` to retain the fullest copy.
    Title,
    /// Title and content compared with case and all whitespace runs normalised.
    /// Catches copies that differ only in indentation or blank lines.
    Normalized,
}

impl DupKey {
    pub fn parse(s: &str) -> Option<DupKey> {
        match s.trim().to_lowercase().as_str() {
            "exact" | "both" => Some(DupKey::Exact),
            "title" | "name" => Some(DupKey::Title),
            "normalized" | "normalised" | "norm" => Some(DupKey::Normalized),
            _ => None,
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            DupKey::Exact => "exact",
            DupKey::Title => "title",
            DupKey::Normalized => "normalized",
        }
    }
}

/// Which copy of a duplicate set to keep.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum KeepRule {
    /// Oldest row (lowest id).
    First,
    /// Newest row (highest id).
    Last,
    /// Longest content; ties broken by lowest id.
    Longest,
}

impl KeepRule {
    pub fn parse(s: &str) -> Option<KeepRule> {
        match s.trim().to_lowercase().as_str() {
            "first" | "oldest" => Some(KeepRule::First),
            "last" | "newest"  => Some(KeepRule::Last),
            "longest" | "long" => Some(KeepRule::Longest),
            _ => None,
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            KeepRule::First => "first",
            KeepRule::Last => "last",
            KeepRule::Longest => "longest",
        }
    }
}

fn squeeze(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The grouping key for one entry.
pub fn key_of(e: &Entry, key: DupKey) -> String {
    match key {
        DupKey::Exact => format!("{}\u{1}{}", e.title, e.content),
        DupKey::Title => e.title.trim().to_lowercase(),
        DupKey::Normalized => format!(
            "{}\u{1}{}",
            squeeze(&e.title.to_lowercase()),
            squeeze(&e.content.to_lowercase())
        ),
    }
}

/// One resolved duplicate set: the row to keep, the rows to remove, and the
/// tag/favorite state the survivor should end up with.
#[derive(Debug, Clone)]
pub struct Group {
    pub title:        String,
    pub survivor:     i64,
    pub victims:      Vec<i64>,
    /// Union of every copy's tags (only when merging).
    pub merged_tags:  Vec<String>,
    /// True if ANY copy was favorited (only when merging).
    pub favorite:     bool,
    /// Whether the survivor actually needs updating.
    pub survivor_changed: bool,
}

/// Build the resolution plan. Entries are grouped by `key`; every group with
/// more than one member yields a [`Group`]. Nothing is written here.
pub fn plan(entries: &[Entry], key: DupKey, keep: KeepRule, merge: bool) -> Vec<Group> {
    use std::collections::HashMap;

    // Preserve first-seen order so the report is stable between runs.
    let mut order: Vec<String> = Vec::new();
    let mut buckets: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, e) in entries.iter().enumerate() {
        let k = key_of(e, key);
        if !buckets.contains_key(&k) {
            order.push(k.clone());
        }
        buckets.entry(k).or_default().push(i);
    }

    let mut out = Vec::new();
    for k in order {
        let idxs = &buckets[&k];
        if idxs.len() < 2 {
            continue;
        }

        // choose the survivor
        let survivor_idx = match keep {
            KeepRule::First => *idxs.iter().min_by_key(|&&i| entries[i].id).unwrap(),
            KeepRule::Last  => *idxs.iter().max_by_key(|&&i| entries[i].id).unwrap(),
            KeepRule::Longest => *idxs
                .iter()
                .min_by_key(|&&i| (std::cmp::Reverse(entries[i].content.len()), entries[i].id))
                .unwrap(),
        };
        let survivor = &entries[survivor_idx];

        let victims: Vec<i64> = idxs
            .iter()
            .filter(|&&i| i != survivor_idx)
            .map(|&i| entries[i].id)
            .collect();

        // merge tags / favorite so nothing is silently lost
        let mut merged_tags = survivor.tags.clone();
        let mut favorite = survivor.favorite;
        if merge {
            for &i in idxs {
                let e = &entries[i];
                favorite |= e.favorite;
                for t in &e.tags {
                    if !merged_tags.iter().any(|x| x.eq_ignore_ascii_case(t)) {
                        merged_tags.push(t.clone());
                    }
                }
            }
        }
        let survivor_changed =
            merge && (merged_tags != survivor.tags || favorite != survivor.favorite);

        out.push(Group {
            title: survivor.title.clone(),
            survivor: survivor.id,
            victims,
            merged_tags,
            favorite,
            survivor_changed,
        });
    }
    out
}

/// Total rows the plan would delete.
pub fn removable(plan: &[Group]) -> usize {
    plan.iter().map(|g| g.victims.len()).sum()
}
