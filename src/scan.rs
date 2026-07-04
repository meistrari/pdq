use std::{
    borrow::Cow,
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
    let stripped = strip_content_comments(data);
    let content = Content::decode_strict(stripped.as_ref()).ok()?;
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

fn strip_content_comments(data: &[u8]) -> Cow<'_, [u8]> {
    let mut stripped: Option<Vec<u8>> = None;
    let mut i = 0;
    let mut literal_depth = 0usize;
    let mut in_hex_string = false;

    while i < data.len() {
        let byte = data[i];

        if literal_depth > 0 {
            if let Some(output) = stripped.as_mut() {
                output.push(byte);
            }
            match byte {
                b'\\' => {
                    i += 1;
                    if i < data.len() {
                        if let Some(output) = stripped.as_mut() {
                            output.push(data[i]);
                        }
                        i += 1;
                    }
                }
                b'(' => {
                    literal_depth += 1;
                    i += 1;
                }
                b')' => {
                    literal_depth -= 1;
                    i += 1;
                }
                _ => i += 1,
            }
            continue;
        }

        if in_hex_string {
            if let Some(output) = stripped.as_mut() {
                output.push(byte);
            }
            if byte == b'>' {
                in_hex_string = false;
            }
            i += 1;
            continue;
        }

        match byte {
            b'%' => {
                let output = stripped.get_or_insert_with(|| data[..i].to_vec());
                output.push(b' ');
                i += 1;
                while i < data.len() && data[i] != b'\r' && data[i] != b'\n' {
                    i += 1;
                }
                if i < data.len() {
                    output.push(data[i]);
                    if data[i] == b'\r' && i + 1 < data.len() && data[i + 1] == b'\n' {
                        i += 1;
                        output.push(data[i]);
                    }
                    i += 1;
                }
            }
            b'(' => {
                if let Some(output) = stripped.as_mut() {
                    output.push(byte);
                }
                literal_depth = 1;
                i += 1;
            }
            b'<' if data.get(i + 1) == Some(&b'<') => {
                if let Some(output) = stripped.as_mut() {
                    output.extend_from_slice(b"<<");
                }
                i += 2;
            }
            b'<' if data.get(i + 1) != Some(&b'<') => {
                if let Some(output) = stripped.as_mut() {
                    output.push(byte);
                }
                in_hex_string = true;
                i += 1;
            }
            b'>' if data.get(i + 1) == Some(&b'>') => {
                if let Some(output) = stripped.as_mut() {
                    output.extend_from_slice(b">>");
                }
                i += 2;
            }
            _ => {
                if let Some(output) = stripped.as_mut() {
                    output.push(byte);
                }
                i += 1;
            }
        }
    }

    match stripped {
        Some(stripped) => Cow::Owned(stripped),
        None => Cow::Borrowed(data),
    }
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
    use super::scan_names;

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
    fn scan_tolerates_content_stream_comments() {
        let used = scan_names(b"q /TPL0 Do\n% producer comment\nBT /F1 12 Tf ET Q").unwrap();

        assert!(used.contains(b"TPL0"));
        assert!(used.contains(b"F1"));
    }

    #[test]
    fn scan_preserves_percent_inside_literal_strings() {
        let used = scan_names(b"(% not a comment) Tj /TPL0 Do").unwrap();

        assert!(used.contains(b"TPL0"));
    }

    #[test]
    fn scan_preserves_percent_inside_hex_strings() {
        assert!(scan_names(b"<% not a comment\n> /TPL0 Do").is_none());
    }

    #[test]
    fn scan_strips_comments_after_inline_dictionaries() {
        let used = scan_names(b"<< /MCID 0 >> BDC % producer comment\n/TPL0 Do EMC").unwrap();

        assert!(used.contains(b"TPL0"));
    }
}
