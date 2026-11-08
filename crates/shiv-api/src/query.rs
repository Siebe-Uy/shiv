//! Client-side search, filter, sort and paging over the full mod catalog
//! (`GET /v2/mods` takes no parameters).

use crate::dto::ModDto;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
    /// Most recently added first.
    #[default]
    Newest,
    /// Most recently updated first.
    Updated,
    MostDownloaded,
    MostLiked,
    /// A to Z, case-insensitive.
    Name,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModQuery {
    pub search: Option<String>,
    pub sort: SortKey,
    /// Case-insensitive exact category match.
    pub category: Option<String>,
    /// Zero-based page index.
    pub page: usize,
    /// Items per page; 0 is treated as 1.
    pub page_size: usize,
}

impl Default for ModQuery {
    fn default() -> Self {
        Self {
            search: None,
            sort: SortKey::default(),
            category: None,
            page: 0,
            page_size: 50,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModPage {
    pub items: Vec<ModDto>,
    /// Matches across all pages.
    pub total: usize,
    /// The requested page index, echoed back.
    pub page: usize,
    pub page_count: usize,
}

fn matches_search(m: &ModDto, needle: &str) -> bool {
    let hit = |s: &str| s.to_lowercase().contains(needle);
    hit(&m.name)
        || m.author.as_deref().is_some_and(hit)
        || m.description.as_deref().is_some_and(hit)
}

pub fn apply_query(mods: &[ModDto], q: &ModQuery) -> ModPage {
    let needle = q
        .search
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase);
    let category = q.category.as_deref().map(str::to_lowercase);

    let mut hits: Vec<&ModDto> = mods
        .iter()
        .filter(|m| needle.as_deref().is_none_or(|n| matches_search(m, n)))
        .filter(|m| {
            category.as_deref().is_none_or(|c| {
                m.category
                    .as_deref()
                    .is_some_and(|mc| mc.to_lowercase() == c)
            })
        })
        .collect();

    // Stable sorts keep catalog order for ties. ISO dates compare as text;
    // a missing date sorts last.
    match q.sort {
        SortKey::Newest => hits.sort_by(|a, b| b.remote_added_at.cmp(&a.remote_added_at)),
        SortKey::Updated => hits.sort_by(|a, b| b.remote_updated_at.cmp(&a.remote_updated_at)),
        SortKey::MostDownloaded => hits.sort_by_key(|m| std::cmp::Reverse(m.download_count)),
        SortKey::MostLiked => hits.sort_by_key(|m| std::cmp::Reverse(m.likes)),
        SortKey::Name => hits.sort_by_key(|m| m.name.to_lowercase()),
    }

    let total = hits.len();
    let page_size = q.page_size.max(1);
    let page_count = total.div_ceil(page_size);
    let items = hits
        .into_iter()
        .skip(q.page.saturating_mul(page_size))
        .take(page_size)
        .cloned()
        .collect();
    ModPage {
        items,
        total,
        page: q.page,
        page_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(name: &str, author: &str, cat: &str, dl: u64, likes: u64, added: &str) -> ModDto {
        ModDto {
            name: name.into(),
            author: Some(author.into()),
            category: Some(cat.into()),
            download_count: dl,
            likes,
            remote_added_at: Some(added.into()),
            remote_updated_at: Some(added.into()),
            ..ModDto::default()
        }
    }

    fn sample() -> Vec<ModDto> {
        vec![
            m("Bravo Skin", "Zed", "Skins", 10, 5, "2025-01-02T00:00:00Z"),
            m("alpha hud", "Amy", "HUD", 30, 1, "2025-03-01T00:00:00Z"),
            m(
                "Charlie Sound",
                "Bob",
                "Audio",
                20,
                9,
                "2025-02-01T00:00:00Z",
            ),
        ]
    }

    fn names(p: &ModPage) -> Vec<&str> {
        p.items.iter().map(|m| m.name.as_str()).collect()
    }

    #[test]
    fn search_is_case_insensitive_over_name_author_description() {
        let mut mods = sample();
        mods[2].description = Some("Has a ROBOT voice".into());
        let q = |s: &str| ModQuery {
            search: Some(s.into()),
            ..ModQuery::default()
        };
        assert_eq!(names(&apply_query(&mods, &q("BRAVO"))), ["Bravo Skin"]);
        assert_eq!(names(&apply_query(&mods, &q("amy"))), ["alpha hud"]);
        assert_eq!(names(&apply_query(&mods, &q("robot"))), ["Charlie Sound"]);
        assert_eq!(apply_query(&mods, &q("nomatch")).total, 0);
        assert_eq!(apply_query(&mods, &q("  ")).total, 3);
    }

    #[test]
    fn category_filter() {
        let q = ModQuery {
            category: Some("hud".into()),
            ..ModQuery::default()
        };
        assert_eq!(names(&apply_query(&sample(), &q)), ["alpha hud"]);
    }

    #[test]
    fn sort_keys() {
        let run = |sort| {
            let p = apply_query(
                &sample(),
                &ModQuery {
                    sort,
                    ..ModQuery::default()
                },
            );
            p.items.into_iter().map(|m| m.name).collect::<Vec<_>>()
        };
        assert_eq!(
            run(SortKey::Newest),
            ["alpha hud", "Charlie Sound", "Bravo Skin"]
        );
        assert_eq!(
            run(SortKey::Updated),
            ["alpha hud", "Charlie Sound", "Bravo Skin"]
        );
        assert_eq!(
            run(SortKey::MostDownloaded),
            ["alpha hud", "Charlie Sound", "Bravo Skin"]
        );
        assert_eq!(
            run(SortKey::MostLiked),
            ["Charlie Sound", "Bravo Skin", "alpha hud"]
        );
        assert_eq!(
            run(SortKey::Name),
            ["alpha hud", "Bravo Skin", "Charlie Sound"]
        );
    }

    #[test]
    fn paging_and_bounds() {
        let q = |page| ModQuery {
            page,
            page_size: 2,
            sort: SortKey::Name,
            ..ModQuery::default()
        };
        let p0 = apply_query(&sample(), &q(0));
        assert_eq!((p0.items.len(), p0.total, p0.page_count), (2, 3, 2));
        let p1 = apply_query(&sample(), &q(1));
        assert_eq!(names(&p1), ["Charlie Sound"]);
        let p9 = apply_query(&sample(), &q(9));
        assert!(p9.items.is_empty());
        assert_eq!((p9.total, p9.page, p9.page_count), (3, 9, 2));
        let huge = apply_query(&sample(), &q(usize::MAX));
        assert!(huge.items.is_empty());
    }

    #[test]
    fn empty_input() {
        let p = apply_query(&[], &ModQuery::default());
        assert_eq!((p.items.len(), p.total, p.page_count), (0, 0, 0));
        let zero = apply_query(
            &sample(),
            &ModQuery {
                page_size: 0,
                ..ModQuery::default()
            },
        );
        assert_eq!((zero.items.len(), zero.page_count), (1, 3));
    }
}
