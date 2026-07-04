use std::{collections::BTreeSet, num::ParseIntError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRangeGroup {
    raw: String,
}

impl PageRangeGroup {
    pub fn parse(raw: impl Into<String>) -> Result<Self, PageRangeError> {
        let raw = raw.into();
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(PageRangeError::EmptyGroup);
        }
        if trimmed.contains(';') {
            return Err(PageRangeError::GroupSeparator);
        }
        for part in trimmed.split(',') {
            parse_part(part.trim())?;
        }
        Ok(Self {
            raw: trimmed.to_string(),
        })
    }

    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// Highest page number this group can reference, or `None` when any
    /// endpoint depends on the document's total page count (`z` or a reverse
    /// `rN` endpoint). Callers can use a `Some` bound to stop resolving pages
    /// early; `None` means the whole document must be enumerated.
    pub fn bounded_max_page(&self) -> Option<usize> {
        let mut max = 0usize;
        for part in self.raw.split(',') {
            let part = part.trim();
            let endpoints: [&str; 2] = match part.split_once('-') {
                Some((start, end)) => [start, end],
                None => [part, part],
            };
            for endpoint in endpoints {
                let endpoint = endpoint.trim();
                if endpoint == "z" || endpoint.starts_with('r') {
                    return None;
                }
                max = max.max(parse_positive(endpoint).ok()?);
            }
        }
        Some(max)
    }

    /// True when the group is literally `1-z`: every page, in document
    /// order, regardless of page count. This is the one range shape a caller
    /// can prove keeps the whole document without resolving it first.
    pub(crate) fn is_full_document(&self) -> bool {
        let mut parts = self.raw.split(',');
        let only = parts.next().map(str::trim);
        parts.next().is_none()
            && only
                .and_then(|part| part.split_once('-'))
                .is_some_and(|(start, end)| start.trim() == "1" && end.trim() == "z")
    }

    pub fn resolve(&self, page_count: usize) -> Result<Vec<usize>, PageRangeError> {
        if page_count == 0 {
            return Err(PageRangeError::NoPages);
        }

        let mut pages = Vec::new();
        for part in self.raw.split(',') {
            append_part(part.trim(), page_count, &mut pages)?;
        }
        Ok(pages)
    }
}

pub fn parse_groups(groups: &[String]) -> Result<Vec<PageRangeGroup>, PageRangeError> {
    if groups.is_empty() {
        return Err(PageRangeError::EmptyGroup);
    }
    groups
        .iter()
        .map(|group| PageRangeGroup::parse(group.clone()))
        .collect()
}

pub fn parse_group_string(raw: &str) -> Result<Vec<PageRangeGroup>, PageRangeError> {
    raw.split(';')
        .map(|group| PageRangeGroup::parse(group.to_string()))
        .collect()
}

fn parse_part(part: &str) -> Result<(), PageRangeError> {
    if part.is_empty() {
        return Err(PageRangeError::EmptyRange);
    }
    if let Some((start, end)) = part.split_once('-') {
        parse_endpoint(start)?;
        parse_endpoint(end)?;
        return Ok(());
    }
    parse_endpoint(part)
}

fn append_part(
    part: &str,
    page_count: usize,
    pages: &mut Vec<usize>,
) -> Result<(), PageRangeError> {
    if let Some((start, end)) = part.split_once('-') {
        let start = resolve_endpoint(start, page_count)?;
        let end = resolve_endpoint(end, page_count)?;
        if start <= end {
            pages.extend(start..=end);
        } else {
            pages.extend((end..=start).rev());
        }
        return Ok(());
    }
    pages.push(resolve_endpoint(part, page_count)?);
    Ok(())
}

fn parse_endpoint(endpoint: &str) -> Result<(), PageRangeError> {
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        return Err(PageRangeError::EmptyRange);
    }
    if endpoint == "z" {
        return Ok(());
    }
    if let Some(rest) = endpoint.strip_prefix('r') {
        parse_positive(rest)?;
        return Ok(());
    }
    parse_positive(endpoint)?;
    Ok(())
}

fn resolve_endpoint(endpoint: &str, page_count: usize) -> Result<usize, PageRangeError> {
    let endpoint = endpoint.trim();
    if endpoint == "z" {
        return Ok(page_count);
    }
    let resolved = if let Some(rest) = endpoint.strip_prefix('r') {
        let reverse = parse_positive(rest)?;
        if reverse > page_count {
            return Err(PageRangeError::OutOfBounds {
                page: reverse,
                page_count,
            });
        }
        page_count + 1 - reverse
    } else {
        parse_positive(endpoint)?
    };

    if resolved > page_count {
        return Err(PageRangeError::OutOfBounds {
            page: resolved,
            page_count,
        });
    }
    Ok(resolved)
}

fn parse_positive(raw: &str) -> Result<usize, PageRangeError> {
    let value = raw
        .parse::<usize>()
        .map_err(PageRangeError::InvalidNumber)?;
    if value == 0 {
        return Err(PageRangeError::ZeroPage);
    }
    Ok(value)
}

pub fn dedupe_preserving_order(pages: &[usize]) -> Vec<usize> {
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for page in pages {
        if seen.insert(*page) {
            result.push(*page);
        }
    }
    result
}

#[derive(Debug, thiserror::Error)]
pub enum PageRangeError {
    #[error("page range group must not be empty")]
    EmptyGroup,

    #[error("page range must not be empty")]
    EmptyRange,

    #[error("page range groups must be passed separately; found ';' inside a group")]
    GroupSeparator,

    #[error("page numbers start at 1")]
    ZeroPage,

    #[error("invalid page number: {0}")]
    InvalidNumber(ParseIntError),

    #[error("page {page} is out of bounds for a document with {page_count} pages")]
    OutOfBounds { page: usize, page_count: usize },

    #[error("document has no pages")]
    NoPages,
}

#[cfg(test)]
mod tests {
    use super::{
        dedupe_preserving_order, parse_group_string, parse_groups, PageRangeError, PageRangeGroup,
    };

    #[test]
    fn resolves_forward_ranges_and_lists() {
        let group = PageRangeGroup::parse("1-3,5").unwrap();

        assert_eq!(group.resolve(6).unwrap(), vec![1, 2, 3, 5]);
    }

    #[test]
    fn full_document_is_exactly_one_to_z() {
        assert!(PageRangeGroup::parse("1-z").unwrap().is_full_document());
        assert!(PageRangeGroup::parse(" 1 - z ").unwrap().is_full_document());

        for subset in ["2-z", "1-5", "z", "r1-z", "1-z,1", "1,2-z"] {
            assert!(
                !PageRangeGroup::parse(subset).unwrap().is_full_document(),
                "{subset} must not be treated as a whole-document rewrite"
            );
        }
    }

    #[test]
    fn bounded_max_page_reports_numeric_endpoints() {
        assert_eq!(
            PageRangeGroup::parse("5000-5100")
                .unwrap()
                .bounded_max_page(),
            Some(5100)
        );
        assert_eq!(
            PageRangeGroup::parse("1,3,9-7").unwrap().bounded_max_page(),
            Some(9)
        );
    }

    #[test]
    fn bounded_max_page_is_none_for_document_relative_endpoints() {
        assert_eq!(
            PageRangeGroup::parse("1-z").unwrap().bounded_max_page(),
            None
        );
        assert_eq!(
            PageRangeGroup::parse("r2").unwrap().bounded_max_page(),
            None
        );
        assert_eq!(
            PageRangeGroup::parse("1-3,r1-z")
                .unwrap()
                .bounded_max_page(),
            None
        );
    }

    #[test]
    fn resolves_reverse_pages_and_z() {
        let group = PageRangeGroup::parse("r1,r3-r1,1-z").unwrap();

        assert_eq!(group.resolve(4).unwrap(), vec![4, 2, 3, 4, 1, 2, 3, 4]);
    }

    #[test]
    fn rejects_empty_groups() {
        assert!(PageRangeGroup::parse("").is_err());
    }

    #[test]
    fn rejects_group_separator_inside_group() {
        assert!(PageRangeGroup::parse("1-2;3-4").is_err());
    }

    #[test]
    fn rejects_out_of_bounds_pages() {
        let group = PageRangeGroup::parse("1-5").unwrap();

        assert!(group.resolve(3).is_err());
    }

    #[test]
    fn dedupes_without_sorting() {
        assert_eq!(dedupe_preserving_order(&[3, 1, 3, 2, 1]), vec![3, 1, 2]);
    }

    #[test]
    fn resolves_descending_ranges_in_reverse_order() {
        let group = PageRangeGroup::parse("5-2").unwrap();

        assert_eq!(group.resolve(6).unwrap(), vec![5, 4, 3, 2]);
    }

    #[test]
    fn trims_whitespace_around_parts_and_endpoints() {
        let group = PageRangeGroup::parse("  1 - 2 , 4  ").unwrap();

        assert_eq!(group.raw(), "1 - 2 , 4");
        assert_eq!(group.resolve(4).unwrap(), vec![1, 2, 4]);
    }

    #[test]
    fn rejects_page_zero() {
        assert!(matches!(
            PageRangeGroup::parse("0"),
            Err(PageRangeError::ZeroPage)
        ));
        assert!(matches!(
            PageRangeGroup::parse("r0"),
            Err(PageRangeError::ZeroPage)
        ));
    }

    #[test]
    fn rejects_trailing_comma_as_empty_range() {
        assert!(matches!(
            PageRangeGroup::parse("1,"),
            Err(PageRangeError::EmptyRange)
        ));
    }

    #[test]
    fn rejects_reverse_endpoint_beyond_page_count() {
        let group = PageRangeGroup::parse("r5").unwrap();

        assert!(matches!(
            group.resolve(3),
            Err(PageRangeError::OutOfBounds {
                page: 5,
                page_count: 3
            })
        ));
    }

    #[test]
    fn resolve_rejects_empty_documents() {
        let group = PageRangeGroup::parse("1").unwrap();

        assert!(matches!(group.resolve(0), Err(PageRangeError::NoPages)));
    }

    #[test]
    fn parse_groups_rejects_empty_list_and_keeps_order() {
        assert!(matches!(parse_groups(&[]), Err(PageRangeError::EmptyGroup)));

        let groups = parse_groups(&["1-2".to_string(), "3".to_string()]).unwrap();
        assert_eq!(groups[0].raw(), "1-2");
        assert_eq!(groups[1].raw(), "3");
    }

    #[test]
    fn parse_group_string_splits_on_semicolons() {
        let groups = parse_group_string("1-2;r1").unwrap();

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].raw(), "1-2");
        assert_eq!(groups[1].raw(), "r1");
        assert!(parse_group_string("1-2;;3").is_err());
    }
}
