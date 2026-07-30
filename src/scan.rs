use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ops::Deref,
    rc::Rc,
};

use lopdf::{content::Content, Dictionary, Object, ObjectId};

use crate::{copy::ObjectSource, filter::decode_stream_content, Result};

const MAX_FORM_RESOURCE_DEPTH: usize = 32;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct UsedNames {
    names: BTreeSet<Vec<u8>>,
    fonts: BTreeSet<Vec<u8>>,
    xobjects: BTreeSet<Vec<u8>>,
}

impl UsedNames {
    pub(crate) fn contains(&self, name: &[u8]) -> bool {
        self.names.contains(name)
    }

    fn extend(&mut self, other: UsedNames) {
        self.names.extend(other.names);
        self.fonts.extend(other.fonts);
        self.xobjects.extend(other.xobjects);
    }

    fn insert(&mut self, resource_type: ResourceType, name: &[u8]) {
        self.names.insert(name.to_vec());
        match resource_type {
            ResourceType::Font => {
                self.fonts.insert(name.to_vec());
            }
            ResourceType::XObject => {
                self.xobjects.insert(name.to_vec());
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceType {
    ColorSpace,
    ExtGState,
    Font,
    Pattern,
    Properties,
    Shading,
    XObject,
}

struct ScanState<'a> {
    dictionary_cache: &'a mut BTreeMap<ObjectId, Rc<Dictionary>>,
    used_names_cache: &'a mut BTreeMap<ObjectId, Option<UsedNames>>,
}

pub(crate) fn collect_used_names(
    source: &impl ObjectSource,
    page: &Dictionary,
    resources: &Dictionary,
    dictionary_cache: &mut BTreeMap<ObjectId, Rc<Dictionary>>,
    used_names_cache: &mut BTreeMap<ObjectId, Option<UsedNames>>,
) -> Result<Option<UsedNames>> {
    let Some(content) = content_bytes(source, page)? else {
        return Ok(None);
    };
    let mut state = ScanState {
        dictionary_cache,
        used_names_cache,
    };
    collect_used_names_from_bytes_with_options(
        source,
        content.stream_id,
        || Some(content.data),
        resources,
        &mut state,
        false,
    )
}

pub(crate) fn collect_used_names_from_stream(
    source: &impl ObjectSource,
    stream_id: Option<ObjectId>,
    content: impl FnOnce() -> Option<Vec<u8>>,
    resources: &Dictionary,
    dictionary_cache: &mut BTreeMap<ObjectId, Rc<Dictionary>>,
    used_names_cache: &mut BTreeMap<ObjectId, Option<UsedNames>>,
) -> Result<Option<UsedNames>> {
    let mut state = ScanState {
        dictionary_cache,
        used_names_cache,
    };
    collect_used_names_from_bytes_with_options(
        source, stream_id, content, resources, &mut state, true,
    )
}

fn collect_used_names_from_bytes_with_options(
    source: &impl ObjectSource,
    stream_id: Option<ObjectId>,
    content: impl FnOnce() -> Option<Vec<u8>>,
    resources: &Dictionary,
    state: &mut ScanState<'_>,
    strict_own_form_failures: bool,
) -> Result<Option<UsedNames>> {
    let Some(mut used) = scan_names_cached(stream_id, state.used_names_cache, content) else {
        return Ok(None);
    };
    if !all_named_resources_resolve(source, resources, b"Font", &used.fonts, state)? {
        return Ok(None);
    }
    if !all_named_resources_resolve(source, resources, b"XObject", &used.xobjects, state)? {
        return Ok(None);
    }
    let mut visited = BTreeSet::new();
    if !collect_form_names(
        source,
        resources,
        &mut used,
        &mut visited,
        state,
        0,
        strict_own_form_failures,
    )? {
        return Ok(None);
    }
    Ok(Some(used))
}

fn collect_form_names(
    source: &impl ObjectSource,
    resources: &Dictionary,
    used: &mut UsedNames,
    visited: &mut BTreeSet<ObjectId>,
    state: &mut ScanState<'_>,
    depth: usize,
    strict_own_form_failures: bool,
) -> Result<bool> {
    if depth > MAX_FORM_RESOURCE_DEPTH {
        return Ok(false);
    }

    let mut queue: VecDeque<Vec<u8>> = used.xobjects.iter().cloned().collect();
    let mut seen_names = BTreeSet::new();
    while let Some(name) = queue.pop_front() {
        if !seen_names.insert(name.clone()) {
            continue;
        }
        let Some(xobject) = named_resource_object(source, resources, b"XObject", &name, state)?
        else {
            return Ok(false);
        };
        let Some((id, stream)) = stream_object(source, &xobject)? else {
            continue;
        };
        if stream.dict.get(b"Subtype").and_then(Object::as_name).ok() != Some(b"Form") {
            continue;
        }
        if let Some(id) = id {
            if !visited.insert(id) {
                continue;
            }
        }

        let Some(form_used) = scan_names_cached(id, state.used_names_cache, || {
            decode_stream_content(&stream).ok()
        }) else {
            if strict_own_form_failures {
                return Ok(false);
            }
            continue;
        };

        if let Ok(form_resources_obj) = stream.dict.get(b"Resources") {
            let Some(form_resources) = dictionary_object(source, form_resources_obj, state)? else {
                if strict_own_form_failures {
                    return Ok(false);
                }
                continue;
            };
            if !all_named_resources_resolve(
                source,
                &form_resources,
                b"Font",
                &form_used.fonts,
                state,
            )? || !all_named_resources_resolve(
                source,
                &form_resources,
                b"XObject",
                &form_used.xobjects,
                state,
            )? {
                return Ok(false);
            }
            let mut nested = form_used;
            let nested_ok = collect_form_names(
                source,
                &form_resources,
                &mut nested,
                visited,
                state,
                depth + 1,
                strict_own_form_failures,
            )?;
            if strict_own_form_failures && !nested_ok {
                return Ok(false);
            }
        } else {
            let before = used.xobjects.clone();
            used.extend(form_used);
            for xobject_name in used.xobjects.difference(&before) {
                queue.push_back(xobject_name.clone());
            }
        }
    }

    Ok(true)
}

fn scan_names(data: &[u8]) -> Option<UsedNames> {
    let data = strip_comments(data);
    let content = Content::decode_strict(&data).ok()?;
    let mut used = UsedNames::default();
    let mut last_name: Option<&[u8]> = None;

    for operation in &content.operations {
        for operand in &operation.operands {
            if let Object::Name(name) = operand {
                last_name = Some(name);
            }
        }
        let Some(resource_type) = resource_type_for_operator(&operation.operator) else {
            continue;
        };
        let Some(name) = last_name else {
            continue;
        };
        used.insert(resource_type, name);
    }

    Some(used)
}

/// Replace `%` comments with spaces so `Content::decode_strict` sees them as
/// the white-space ISO 32000-1 §7.2.4 says they are. lopdf only tolerates a
/// comment immediately before an operation; producers such as Canon scanner
/// firmware emit them mid-stream (`% CANON_PFINF_TYPE0_TEXTON` followed by a
/// blank line), which would otherwise fail the strict parse and disable
/// resource pruning for the whole page or form.
///
/// A `%` inside a literal or hex string is data and is left alone. Content
/// containing an inline image is returned unchanged: its binary payload may
/// contain `%` bytes that must not be treated as comment starts, so those
/// streams keep the parse-or-fallback behavior they have today.
fn strip_comments(data: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    use std::borrow::Cow;

    if memchr::memchr(b'%', data).is_none() {
        return Cow::Borrowed(data);
    }

    enum State {
        Normal,
        LiteralString { depth: usize },
        HexString,
        Comment,
    }
    fn is_boundary(byte: Option<u8>) -> bool {
        match byte {
            None => true,
            Some(byte) => byte.is_ascii_whitespace() || b"()<>[]{}/%".contains(&byte),
        }
    }

    let mut out = data.to_vec();
    let mut state = State::Normal;
    let mut index = 0;
    while index < out.len() {
        let byte = out[index];
        match state {
            State::Normal => match byte {
                b'%' => {
                    state = State::Comment;
                    continue;
                }
                b'(' => state = State::LiteralString { depth: 1 },
                b'<' => {
                    if out.get(index + 1) == Some(&b'<') {
                        index += 1;
                    } else {
                        state = State::HexString;
                    }
                }
                b'B' if out.get(index + 1) == Some(&b'I')
                    && is_boundary(index.checked_sub(1).map(|i| out[i]))
                    && is_boundary(out.get(index + 2).copied()) =>
                {
                    return Cow::Borrowed(data);
                }
                _ => {}
            },
            State::LiteralString { depth } => match byte {
                b'\\' => index += 1,
                b'(' => state = State::LiteralString { depth: depth + 1 },
                b')' => {
                    state = if depth > 1 {
                        State::LiteralString { depth: depth - 1 }
                    } else {
                        State::Normal
                    };
                }
                _ => {}
            },
            State::HexString => {
                if byte == b'>' {
                    state = State::Normal;
                }
            }
            State::Comment => {
                if byte == b'\r' || byte == b'\n' {
                    state = State::Normal;
                    continue;
                }
                out[index] = b' ';
            }
        }
        index += 1;
    }
    Cow::Owned(out)
}

fn scan_names_cached(
    stream_id: Option<ObjectId>,
    used_names_cache: &mut BTreeMap<ObjectId, Option<UsedNames>>,
    content: impl FnOnce() -> Option<Vec<u8>>,
) -> Option<UsedNames> {
    let Some(stream_id) = stream_id else {
        return scan_names(&content()?);
    };
    if let Some(cached) = used_names_cache.get(&stream_id) {
        return cached.clone();
    }
    let data = content()?;
    let used = scan_names(&data);
    used_names_cache.insert(stream_id, used.clone());
    used
}

fn resource_type_for_operator(operator: &str) -> Option<ResourceType> {
    match operator {
        "CS" | "cs" => Some(ResourceType::ColorSpace),
        "gs" => Some(ResourceType::ExtGState),
        "Tf" => Some(ResourceType::Font),
        "SCN" | "scn" => Some(ResourceType::Pattern),
        "BDC" | "DP" => Some(ResourceType::Properties),
        "sh" => Some(ResourceType::Shading),
        "Do" => Some(ResourceType::XObject),
        _ => None,
    }
}

struct ContentBytes {
    stream_id: Option<ObjectId>,
    data: Vec<u8>,
}

fn content_bytes(source: &impl ObjectSource, page: &Dictionary) -> Result<Option<ContentBytes>> {
    let Ok(contents) = page.get(b"Contents") else {
        return Ok(Some(ContentBytes {
            stream_id: None,
            data: Vec::new(),
        }));
    };
    match contents {
        Object::Reference(id) => Ok(content_stream_bytes(source, *id)?.map(|data| ContentBytes {
            stream_id: Some(*id),
            data,
        })),
        Object::Array(items) => {
            let mut data = Vec::new();
            for item in items {
                let Object::Reference(id) = item else {
                    return Ok(None);
                };
                if append_content_stream(source, *id, &mut data)?.is_none() {
                    return Ok(None);
                }
                data.push(b'\n');
            }
            Ok(Some(ContentBytes {
                stream_id: None,
                data,
            }))
        }
        Object::Stream(stream) => {
            let Ok(decoded) = decode_stream_content(stream) else {
                return Ok(None);
            };
            Ok(Some(ContentBytes {
                stream_id: None,
                data: decoded,
            }))
        }
        _ => Ok(None),
    }
}

fn append_content_stream(
    source: &impl ObjectSource,
    id: ObjectId,
    data: &mut Vec<u8>,
) -> Result<Option<()>> {
    let Some(decoded) = content_stream_bytes(source, id)? else {
        return Ok(None);
    };
    data.extend(decoded);
    Ok(Some(()))
}

fn content_stream_bytes(source: &impl ObjectSource, id: ObjectId) -> Result<Option<Vec<u8>>> {
    let object = match source.get_object_value(id) {
        Ok(object) => object,
        Err(lopdf::Error::ObjectNotFound(_)) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let Ok(stream) = object.as_stream() else {
        return Ok(None);
    };
    let Ok(decoded) = decode_stream_content(stream) else {
        return Ok(None);
    };
    Ok(Some(decoded))
}

fn named_resource_object(
    source: &impl ObjectSource,
    resources: &Dictionary,
    resource_type: &[u8],
    name: &[u8],
    state: &mut ScanState<'_>,
) -> Result<Option<Object>> {
    let Some(dict) = resource_dictionary(source, resources, resource_type, state)? else {
        return Ok(None);
    };
    let Ok(value) = dict.get(name) else {
        return Ok(None);
    };
    Ok(Some(value.clone()))
}

fn resource_dictionary<'a>(
    source: &impl ObjectSource,
    resources: &'a Dictionary,
    resource_type: &[u8],
    state: &mut ScanState<'_>,
) -> Result<Option<ResolvedDictionary<'a>>> {
    let Ok(value) = resources.get(resource_type) else {
        return Ok(None);
    };
    dictionary_object(source, value, state)
}

fn dictionary_object<'a>(
    source: &impl ObjectSource,
    value: &'a Object,
    state: &mut ScanState<'_>,
) -> Result<Option<ResolvedDictionary<'a>>> {
    match value {
        Object::Dictionary(dict) => Ok(Some(ResolvedDictionary::Borrowed(dict))),
        Object::Reference(id) => {
            if let Some(cached) = state.dictionary_cache.get(id) {
                return Ok(Some(ResolvedDictionary::Shared(Rc::clone(cached))));
            }
            let object = match source.get_object_value(*id) {
                Ok(object) => object,
                Err(lopdf::Error::ObjectNotFound(_)) => return Ok(None),
                Err(err) => return Err(err.into()),
            };
            let Some(dict) = object.as_dict().ok().cloned() else {
                return Ok(None);
            };
            let dict = Rc::new(dict);
            state.dictionary_cache.insert(*id, Rc::clone(&dict));
            Ok(Some(ResolvedDictionary::Shared(dict)))
        }
        _ => Ok(None),
    }
}

enum ResolvedDictionary<'a> {
    Borrowed(&'a Dictionary),
    Shared(Rc<Dictionary>),
}

impl Deref for ResolvedDictionary<'_> {
    type Target = Dictionary;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Borrowed(dict) => dict,
            Self::Shared(dict) => dict,
        }
    }
}

fn stream_object(
    source: &impl ObjectSource,
    value: &Object,
) -> Result<Option<(Option<ObjectId>, lopdf::Stream)>> {
    match value {
        Object::Reference(id) => {
            let object = match source.get_object_value(*id) {
                Ok(object) => object,
                Err(lopdf::Error::ObjectNotFound(_)) => return Ok(None),
                Err(err) => return Err(err.into()),
            };
            Ok(object
                .as_stream()
                .ok()
                .cloned()
                .map(|stream| (Some(*id), stream)))
        }
        Object::Stream(stream) => Ok(Some((None, stream.clone()))),
        _ => Ok(None),
    }
}

fn all_named_resources_resolve(
    source: &impl ObjectSource,
    resources: &Dictionary,
    resource_type: &[u8],
    names: &BTreeSet<Vec<u8>>,
    state: &mut ScanState<'_>,
) -> Result<bool> {
    if names.is_empty() {
        return Ok(true);
    }
    let Some(dict) = resource_dictionary(source, resources, resource_type, state)? else {
        return Ok(false);
    };
    Ok(names.iter().all(|name| dict.has(name)))
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::{scan_names, strip_comments};

    #[test]
    fn scans_names_used_by_resource_operators() {
        let used = scan_names(b"q /TPL0 Do /F1 12 Tf /GS1 gs Q").unwrap();

        assert!(used.contains(b"TPL0"));
        assert!(used.contains(b"F1"));
        assert!(used.contains(b"GS1"));
        assert!(!used.contains(b"Unused"));
    }

    #[test]
    fn strict_scan_rejects_trailing_invalid_content() {
        assert!(scan_names(b"/TPL0 Do @@@").is_none());
    }

    #[test]
    fn scan_survives_mid_stream_comments() {
        // Canon scanner shape: comment, blank line, then operations. lopdf
        // only accepts a comment immediately before an operation, so without
        // stripping this fails the strict parse and pruning is disabled.
        let used =
            scan_names(b"% CANON_PFINF_TYPE0_TEXTON\n\nq /TPL5 Do Q\nBT /F1 % sizes\n12 Tf ET")
                .unwrap();
        assert!(used.contains(b"TPL5"));
        assert!(used.contains(b"F1"));
    }

    #[test]
    fn strip_comments_leaves_percent_inside_strings() {
        let data = b"BT (100% juros \\(a.m.\\)) Tj ET <25AB> Tj % real comment\n/F1 12 Tf";
        let stripped = strip_comments(data);
        assert!(stripped.windows(4).any(|w| w == b"100%"));
        assert!(stripped.windows(4).any(|w| w == b"<25A"));
        assert!(!stripped.windows(4).any(|w| w == b"real"));
    }

    #[test]
    fn strip_comments_borrows_when_there_is_nothing_to_strip() {
        assert!(matches!(strip_comments(b"q /X Do Q"), Cow::Borrowed(_)));
    }

    #[test]
    fn strip_comments_declines_inline_images() {
        // '%' inside the binary inline-image payload must not be treated as a
        // comment; the stream is handed to the parser unchanged.
        let data = b"% c\nBI /W 2 /H 2 /CS /G /BPC 8 ID \x25\xF1\x00\xFF EI Q";
        assert!(matches!(strip_comments(data), Cow::Borrowed(_)));
    }
}
