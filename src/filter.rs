use lopdf::{Dictionary, Object, ObjectId, Stream};

pub(crate) const FILTER_ABBREVIATIONS: [(&[u8], &[u8]); 7] = [
    (b"Fl", b"FlateDecode"),
    (b"AHx", b"ASCIIHexDecode"),
    (b"A85", b"ASCII85Decode"),
    (b"LZW", b"LZWDecode"),
    (b"RL", b"RunLengthDecode"),
    (b"CCF", b"CCITTFaxDecode"),
    (b"DCT", b"DCTDecode"),
];

pub(crate) fn canonical_filter_name(name: &[u8]) -> &[u8] {
    FILTER_ABBREVIATIONS
        .iter()
        .find_map(|(abbreviation, canonical)| (*abbreviation == name).then_some(*canonical))
        .unwrap_or(name)
}

pub(crate) fn normalize_stream_filter_names(stream: &mut Stream) -> bool {
    let Ok(filter) = stream.dict.get_mut(b"Filter") else {
        return false;
    };
    normalize_filter_object(filter)
}

pub(crate) fn normalize_object_filter_names(object: &mut Object) {
    if let Ok(stream) = object.as_stream_mut() {
        normalize_stream_filter_names(stream);
    }
}

pub(crate) fn normalize_filter_names_for_lopdf_load(
    id: ObjectId,
    object: &mut Object,
) -> Option<(ObjectId, Object)> {
    if let Ok(stream) = object.as_stream_mut() {
        if stream.dict.has_type(b"ObjStm")
            && stream.dict.has(b"Filter")
            && decode_stream_in_place(stream).is_ok()
        {
            return Some((id, object.clone()));
        }
        normalize_stream_filter_names(stream);
        return Some((id, object.clone()));
    }
    normalize_object_filter_names(object);
    Some((id, object.clone()))
}

pub(crate) fn decode_stream_content(stream: &Stream) -> lopdf::Result<Vec<u8>> {
    let Some(filters) = stream_filters(stream) else {
        return Ok(stream.content.clone());
    };
    let decode_params = stream.dict.get(b"DecodeParms").ok();
    let mut output = stream.content.clone();
    for (index, filter) in filters.iter().enumerate() {
        output = decode_filter(filter, &output, decode_params_at(decode_params, index))?;
    }
    Ok(output)
}

pub(crate) fn decode_stream_in_place(stream: &mut Stream) -> lopdf::Result<()> {
    let content = decode_stream_content(stream)?;
    stream.set_plain_content(content);
    Ok(())
}

fn normalize_filter_object(filter: &mut Object) -> bool {
    match filter {
        Object::Name(name) => normalize_filter_name(name),
        Object::Array(filters) => {
            let mut changed = false;
            for filter in filters {
                changed |= normalize_filter_object(filter);
            }
            changed
        }
        _ => false,
    }
}

fn normalize_filter_name(name: &mut Vec<u8>) -> bool {
    let canonical = canonical_filter_name(name);
    if canonical == name.as_slice() {
        return false;
    }
    *name = canonical.to_vec();
    true
}

fn stream_filters(stream: &Stream) -> Option<Vec<Vec<u8>>> {
    match stream.dict.get(b"Filter").ok()? {
        Object::Name(name) => Some(vec![canonical_filter_name(name).to_vec()]),
        Object::Array(filters) => {
            let mut names = Vec::with_capacity(filters.len());
            for filter in filters {
                let Object::Name(name) = filter else {
                    return None;
                };
                names.push(canonical_filter_name(name).to_vec());
            }
            Some(names)
        }
        _ => None,
    }
}

fn decode_params_at(params: Option<&Object>, index: usize) -> Option<&Dictionary> {
    match params? {
        Object::Dictionary(params) => Some(params),
        Object::Array(params) => params.get(index).and_then(|params| match params {
            Object::Dictionary(params) => Some(params),
            _ => None,
        }),
        _ => None,
    }
}

fn decode_filter(
    filter: &[u8],
    input: &[u8],
    params: Option<&Dictionary>,
) -> lopdf::Result<Vec<u8>> {
    match filter {
        b"ASCIIHexDecode" => decode_ascii_hex(input),
        b"RunLengthDecode" => decode_run_length(input),
        b"FlateDecode" | b"LZWDecode" | b"ASCII85Decode" => {
            decode_with_lopdf(filter, input, params)
        }
        _ => Err(lopdf::Error::Unimplemented("decompression algorithms")),
    }
}

fn decode_with_lopdf(
    filter: &[u8],
    input: &[u8],
    params: Option<&Dictionary>,
) -> lopdf::Result<Vec<u8>> {
    let mut dict = Dictionary::new();
    dict.set("Filter", Object::Name(filter.to_vec()));
    if let Some(params) = params {
        dict.set("DecodeParms", Object::Dictionary(params.clone()));
    }
    Stream::new(dict, input.to_vec()).decompressed_content()
}

fn decode_ascii_hex(input: &[u8]) -> lopdf::Result<Vec<u8>> {
    let mut output = Vec::with_capacity(input.len() / 2);
    let mut high = None;
    for &byte in input {
        if byte == b'>' {
            break;
        }
        if byte.is_ascii_whitespace() {
            continue;
        }
        let Some(nibble) = hex_nibble(byte) else {
            return Err(lopdf::Error::InvalidStream(format!(
                "invalid ASCIIHexDecode byte 0x{byte:02x}"
            )));
        };
        match high.take() {
            Some(high) => output.push((high << 4) | nibble),
            None => high = Some(nibble),
        }
    }
    if let Some(high) = high {
        output.push(high << 4);
    }
    Ok(output)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_run_length(input: &[u8]) -> lopdf::Result<Vec<u8>> {
    let mut output = Vec::with_capacity(input.len());
    let mut index = 0usize;
    while index < input.len() {
        let length = input[index];
        index += 1;
        match length {
            0..=127 => {
                let count = length as usize + 1;
                let end = index.checked_add(count).ok_or_else(|| {
                    lopdf::Error::InvalidStream("RunLengthDecode literal length overflow".into())
                })?;
                let literal = input.get(index..end).ok_or_else(|| {
                    lopdf::Error::InvalidStream("truncated RunLengthDecode literal run".into())
                })?;
                output.extend_from_slice(literal);
                index = end;
            }
            128 => break,
            129..=255 => {
                let byte = *input.get(index).ok_or_else(|| {
                    lopdf::Error::InvalidStream("truncated RunLengthDecode repeat run".into())
                })?;
                index += 1;
                output.extend(std::iter::repeat_n(byte, 257usize - length as usize));
            }
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use lopdf::{dictionary, Object, Stream};

    use super::{decode_stream_content, normalize_stream_filter_names, FILTER_ABBREVIATIONS};

    #[test]
    fn normalizes_all_pdf_filter_name_abbreviations() {
        for (abbreviation, canonical) in FILTER_ABBREVIATIONS {
            let mut stream = Stream::new(
                dictionary! {
                    "Filter" => Object::Name(abbreviation.to_vec()),
                },
                Vec::new(),
            );
            assert!(normalize_stream_filter_names(&mut stream));
            assert_eq!(
                stream.dict.get(b"Filter").unwrap().as_name().unwrap(),
                canonical
            );
        }
    }

    #[test]
    fn normalizes_abbreviated_filter_arrays() {
        let mut stream = Stream::new(
            dictionary! {
                "Filter" => Object::Array(vec![
                    Object::Name(b"AHx".to_vec()),
                    Object::Name(b"Fl".to_vec()),
                    Object::Name(b"DCT".to_vec()),
                ]),
            },
            Vec::new(),
        );
        assert!(normalize_stream_filter_names(&mut stream));
        let filters = stream.dict.get(b"Filter").unwrap().as_array().unwrap();
        let names = filters
            .iter()
            .map(|filter| filter.as_name().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                b"ASCIIHexDecode".as_slice(),
                b"FlateDecode".as_slice(),
                b"DCTDecode".as_slice()
            ]
        );
    }

    #[test]
    fn decodes_abbreviated_ascii_hex_and_run_length_streams() {
        let ascii_hex = Stream::new(
            dictionary! {
                "Filter" => "AHx",
            },
            b"48 65 6c 6c 6f>".to_vec(),
        );
        assert_eq!(decode_stream_content(&ascii_hex).unwrap(), b"Hello");

        let run_length = Stream::new(
            dictionary! {
                "Filter" => "RL",
            },
            vec![4, b'H', b'e', b'l', b'l', b'o', 128],
        );
        assert_eq!(decode_stream_content(&run_length).unwrap(), b"Hello");
    }
}
