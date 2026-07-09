use wasm_bindgen::prelude::*;

fn map_error(err: pdq::PdfOpsError) -> JsValue {
    JsValue::from_str(&err.to_string())
}

fn parse_pages(raw: Option<String>) -> pdq::Result<Option<pdq::PageRangeGroup>> {
    raw.map(pdq::PageRangeGroup::parse)
        .transpose()
        .map_err(pdq::PdfOpsError::from)
}

fn uint8_array(bytes: &[u8]) -> js_sys::Uint8Array {
    js_sys::Uint8Array::from(bytes)
}

fn set_prop(object: &js_sys::Object, key: &str, value: &JsValue) -> Result<(), JsValue> {
    js_sys::Reflect::set(object, &JsValue::from_str(key), value).map(|_| ())
}

fn parse_ranges(raw: &[String]) -> pdq::Result<Vec<pdq::PageRangeGroup>> {
    raw.iter()
        .map(|range| pdq::PageRangeGroup::parse(range.as_str()))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(pdq::PdfOpsError::from)
}

fn js_string_array(array: &js_sys::Array) -> Result<Vec<String>, JsValue> {
    array
        .iter()
        .map(|value| {
            value
                .as_string()
                .ok_or_else(|| JsValue::from_str("expected an array of strings"))
        })
        .collect()
}

fn rendered_page_to_object(page: pdq::RenderedPage) -> Result<js_sys::Object, JsValue> {
    let object = js_sys::Object::new();
    set_prop(&object, "page", &JsValue::from_f64(page.page as f64))?;
    set_prop(&object, "width", &JsValue::from_f64(page.width as f64))?;
    set_prop(&object, "height", &JsValue::from_f64(page.height as f64))?;
    set_prop(&object, "png", &uint8_array(&page.png))?;
    Ok(object)
}

fn pdf_bytes_output_to_object(output: pdq::PdfBytesOutput) -> Result<js_sys::Object, JsValue> {
    let object = js_sys::Object::new();
    set_prop(&object, "index", &JsValue::from_f64(output.index as f64))?;
    set_prop(&object, "pdf", &uint8_array(&output.pdf))?;
    Ok(object)
}

#[wasm_bindgen(js_name = version)]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[wasm_bindgen(js_name = pageCount)]
pub fn page_count(input: &[u8], strict: bool, password: Option<String>) -> Result<usize, JsValue> {
    let password = password.as_deref();
    if strict {
        pdq::page_count_from_bytes_with_password(input, password).map_err(map_error)
    } else {
        pdq::page_count_fast_from_bytes_with_password(input, password).map_err(map_error)
    }
}

#[wasm_bindgen(js_name = extractTextJson)]
pub fn extract_text_json(
    input: &[u8],
    pages: Option<String>,
    password: Option<String>,
) -> Result<String, JsValue> {
    let options = pdq::ExtractTextOptions {
        pages: parse_pages(pages).map_err(map_error)?,
        password,
    };
    let pages = pdq::extract_text_from_bytes(input, &options).map_err(map_error)?;
    Ok(pdq::text::pages_to_json(&pages))
}

#[wasm_bindgen(js_name = renderPages)]
pub fn render_pages(
    input: &[u8],
    pages: Option<String>,
    dpi: Option<f32>,
) -> Result<js_sys::Array, JsValue> {
    let options = pdq::RenderOptions {
        dpi: dpi.unwrap_or(pdq::RenderOptions::default().dpi),
        pages: parse_pages(pages).map_err(map_error)?,
    };
    let rendered = pdq::render_pages_from_bytes(input, &options).map_err(map_error)?;

    let out = js_sys::Array::new();
    for page in rendered {
        out.push(&rendered_page_to_object(page)?.into());
    }
    Ok(out)
}

#[wasm_bindgen(js_name = split)]
pub fn split(
    input: &[u8],
    ranges: js_sys::Array,
    password: Option<String>,
) -> Result<js_sys::Array, JsValue> {
    let ranges = js_string_array(&ranges)?;
    let ranges = parse_ranges(&ranges).map_err(map_error)?;
    let outputs: Vec<pdq::SplitBytesOutput> = ranges
        .into_iter()
        .map(|range| pdq::SplitBytesOutput { range })
        .collect();

    let results = pdq::split_from_bytes_with_password(input, &outputs, password.as_deref())
        .map_err(map_error)?;

    let out = js_sys::Array::new();
    for result in results {
        out.push(&pdf_bytes_output_to_object(result)?.into());
    }
    Ok(out)
}

#[wasm_bindgen(js_name = splitPages)]
pub fn split_pages(
    input: &[u8],
    pages_per_file: usize,
    password: Option<String>,
) -> Result<js_sys::Array, JsValue> {
    let options = pdq::SplitPagesOptions {
        pages_per_file,
        password,
    };
    let results = pdq::split_pages_from_bytes(input, &options).map_err(map_error)?;

    let out = js_sys::Array::new();
    for result in results {
        out.push(&pdf_bytes_output_to_object(result)?.into());
    }
    Ok(out)
}

#[wasm_bindgen(js_name = merge)]
pub fn merge(
    inputs: js_sys::Array,
    password: Option<String>,
) -> Result<js_sys::Uint8Array, JsValue> {
    let mut merge_inputs = Vec::with_capacity(inputs.length() as usize);
    for value in inputs.iter() {
        let object: js_sys::Object = value
            .dyn_into()
            .map_err(|_| JsValue::from_str("merge input must be an object"))?;

        let pdf_value = js_sys::Reflect::get(&object, &JsValue::from_str("pdf"))?;
        let pdf: js_sys::Uint8Array = pdf_value
            .dyn_into()
            .map_err(|_| JsValue::from_str("merge input.pdf must be a Uint8Array"))?;

        let ranges_value = js_sys::Reflect::get(&object, &JsValue::from_str("ranges"))?;
        let ranges = if ranges_value.is_undefined() || ranges_value.is_null() {
            Vec::new()
        } else {
            let ranges_array: js_sys::Array = ranges_value
                .dyn_into()
                .map_err(|_| JsValue::from_str("merge input.ranges must be an array of strings"))?;
            parse_ranges(&js_string_array(&ranges_array)?).map_err(map_error)?
        };

        merge_inputs.push(pdq::MergeBytesInput {
            bytes: pdf.to_vec(),
            ranges,
        });
    }

    let options = pdq::MergeBytesOptions { password };
    let merged = pdq::merge_from_bytes_with_options(&merge_inputs, options).map_err(map_error)?;
    Ok(uint8_array(&merged))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pages_none_stays_none() {
        assert_eq!(parse_pages(None).unwrap(), None);
    }

    #[test]
    fn parse_pages_parses_a_valid_range() {
        let parsed = parse_pages(Some("1-3,5".to_string())).unwrap().unwrap();
        assert_eq!(parsed.raw(), "1-3,5");
    }

    #[test]
    fn parse_pages_rejects_an_invalid_range() {
        assert!(parse_pages(Some("".to_string())).is_err());
    }

    #[test]
    fn parse_ranges_empty_list_is_empty() {
        assert_eq!(parse_ranges(&[]).unwrap(), Vec::new());
    }

    #[test]
    fn parse_ranges_parses_every_entry_in_order() {
        let ranges = parse_ranges(&["1-2".to_string(), "r1".to_string()]).unwrap();
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].raw(), "1-2");
        assert_eq!(ranges[1].raw(), "r1");
    }

    #[test]
    fn parse_ranges_rejects_an_invalid_entry() {
        assert!(parse_ranges(&["1-2".to_string(), "".to_string()]).is_err());
    }

    #[test]
    fn page_count_accepts_no_password_fast_call_shape() {
        let bytes = include_bytes!("../../../tests/fixtures/11-pages.pdf");
        assert_eq!(page_count(bytes, false, None).unwrap(), 11);
    }
}
