use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ops::Deref,
    rc::Rc,
};

use lopdf::{content::Content, Dictionary, Object, ObjectId};

use crate::{copy::ObjectSource, Result};

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
    collect_used_names_from_bytes_with_options(source, None, &content, resources, &mut state, false)
}

pub(crate) fn collect_used_names_from_stream(
    source: &impl ObjectSource,
    stream_id: Option<ObjectId>,
    content: &[u8],
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
    content: &[u8],
    resources: &Dictionary,
    state: &mut ScanState<'_>,
    strict_own_form_failures: bool,
) -> Result<Option<UsedNames>> {
    let Some(mut used) = scan_names_cached(content, stream_id, state.used_names_cache) else {
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

        let Ok(content) = stream.decompressed_content() else {
            if strict_own_form_failures {
                return Ok(false);
            }
            continue;
        };
        let Some(form_used) = scan_names_cached(&content, id, state.used_names_cache) else {
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
    let content = Content::decode_strict(data).ok()?;
    let mut used = UsedNames::default();
    let mut last_name: Option<Vec<u8>> = None;

    for operation in content.operations {
        for operand in &operation.operands {
            if let Object::Name(name) = operand {
                last_name = Some(name.clone());
            }
        }
        let Some(resource_type) = resource_type_for_operator(&operation.operator) else {
            continue;
        };
        let Some(name) = last_name.as_deref() else {
            continue;
        };
        used.insert(resource_type, name);
    }

    Some(used)
}

fn scan_names_cached(
    data: &[u8],
    stream_id: Option<ObjectId>,
    used_names_cache: &mut BTreeMap<ObjectId, Option<UsedNames>>,
) -> Option<UsedNames> {
    let Some(stream_id) = stream_id else {
        return scan_names(data);
    };
    if let Some(cached) = used_names_cache.get(&stream_id) {
        return cached.clone();
    }
    let used = scan_names(data);
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

fn content_bytes(source: &impl ObjectSource, page: &Dictionary) -> Result<Option<Vec<u8>>> {
    let Ok(contents) = page.get(b"Contents") else {
        return Ok(Some(Vec::new()));
    };
    let mut data = Vec::new();
    match contents {
        Object::Reference(id) => {
            if append_content_stream(source, *id, &mut data)?.is_none() {
                return Ok(None);
            }
        }
        Object::Array(items) => {
            for item in items {
                let Object::Reference(id) = item else {
                    return Ok(None);
                };
                if append_content_stream(source, *id, &mut data)?.is_none() {
                    return Ok(None);
                }
                data.push(b'\n');
            }
        }
        Object::Stream(stream) => {
            let Ok(decoded) = stream.decompressed_content() else {
                return Ok(None);
            };
            data.extend(decoded);
        }
        _ => return Ok(None),
    }
    Ok(Some(data))
}

fn append_content_stream(
    source: &impl ObjectSource,
    id: ObjectId,
    data: &mut Vec<u8>,
) -> Result<Option<()>> {
    let object = match source.get_object_value(id) {
        Ok(object) => object,
        Err(lopdf::Error::ObjectNotFound(_)) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let Ok(stream) = object.as_stream() else {
        return Ok(None);
    };
    let Ok(decoded) = stream.decompressed_content() else {
        return Ok(None);
    };
    data.extend(decoded);
    Ok(Some(()))
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
}
