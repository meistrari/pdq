use wasm_bindgen::prelude::*;

#[allow(dead_code)]
fn map_error(err: pdq::PdfOpsError) -> JsValue {
    JsValue::from_str(&err.to_string())
}

#[wasm_bindgen(js_name = version)]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
