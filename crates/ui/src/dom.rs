//! Small, failure-tolerant wrappers over browser APIs (web-sys). Every call that can throw is
//! turned into `Option`/`bool`; callers degrade gracefully instead of crashing.

use std::cell::Cell;

use wasm_bindgen::JsCast;

thread_local! {
    static IS_MAC: Cell<Option<bool>> = const { Cell::new(None) };
    static NEXT_ID: Cell<u64> = const { Cell::new(0) };
}

/// The document, if there is one.
pub fn document() -> Option<web_sys::Document> {
    web_sys::window().and_then(|w| w.document())
}

/// Whether the platform is macOS (Cmd is the primary modifier there, Ctrl elsewhere).
pub fn is_mac() -> bool {
    IS_MAC.with(|c| {
        if let Some(v) = c.get() {
            return v;
        }
        let v = web_sys::window()
            .and_then(|w| w.navigator().platform().ok())
            .is_some_and(|p| p.to_ascii_lowercase().contains("mac"));
        c.set(Some(v));
        v
    })
}

/// A document-unique element id with the given prefix.
pub fn unique_id(prefix: &str) -> String {
    let n = NEXT_ID.with(|c| {
        let n = c.get().wrapping_add(1);
        c.set(n);
        n
    });
    format!("{prefix}-{n}")
}

/// Reads a value from `localStorage`; `None` when storage is unavailable or throws.
pub fn storage_get(key: &str) -> Option<String> {
    web_sys::window()?.local_storage().ok()??.get_item(key).ok()?
}

/// Writes a value to `localStorage`; failures (private mode, quota) are ignored.
pub fn storage_set(key: &str, value: &str) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item(key, value);
    }
}

/// The value of a CSS custom property on `el`, e.g. `--list-row-height`.
pub fn css_var(el: &web_sys::Element, name: &str) -> Option<String> {
    let style = web_sys::window()?.get_computed_style(el).ok()??;
    let v = style.get_property_value(name).ok()?;
    let v = v.trim();
    if v.is_empty() { None } else { Some(v.to_owned()) }
}

/// Parses a CSS pixel length such as `32px` or `32`; rejects non-finite and non-positive values.
pub fn parse_px(value: &str) -> Option<f64> {
    let v = value.trim();
    let v = v.strip_suffix("px").unwrap_or(v).trim();
    let n: f64 = v.parse().ok()?;
    (n.is_finite() && n > 0.0).then_some(n)
}

/// Focuses the element matching `selector`; returns whether one was found.
pub fn focus_selector(selector: &str) -> bool {
    let Some(el) = document().and_then(|d| d.query_selector(selector).ok().flatten()) else { return false };
    focus_element(&el)
}

/// Focuses `el` if it is an HTML element.
pub fn focus_element(el: &web_sys::Element) -> bool {
    match el.dyn_ref::<web_sys::HtmlElement>() {
        Some(h) => h.focus().is_ok(),
        None => false,
    }
}

/// The element that has focus.
pub fn active_element() -> Option<web_sys::Element> {
    document()?.active_element()
}

/// Whether `target` is a text field or editable region (plain-key shortcuts must not fire there).
pub fn is_editable(target: Option<&web_sys::EventTarget>) -> bool {
    let Some(el) = target.and_then(|t| t.dyn_ref::<web_sys::HtmlElement>()) else { return false };
    let tag = el.tag_name().to_ascii_uppercase();
    matches!(tag.as_str(), "INPUT" | "TEXTAREA" | "SELECT") || el.is_content_editable()
}

/// Whether the app is showing its dark appearance: `data-theme` on `<html>` when settings force
/// one, else the system's.
pub fn is_dark() -> bool {
    let theme = document().and_then(|d| d.document_element()).and_then(|e| e.get_attribute("data-theme"));
    match theme.as_deref() {
        Some("dark") => true,
        Some("light") => false,
        _ => web_sys::window()
            .and_then(|w| w.match_media("(prefers-color-scheme: dark)").ok().flatten())
            .is_some_and(|m| m.matches()),
    }
}

/// All elements matching `selector`, in document order.
pub fn query_all(selector: &str) -> Vec<web_sys::Element> {
    let Some(list) = document().and_then(|d| d.query_selector_all(selector).ok()) else { return Vec::new() };
    (0..list.length()).filter_map(|i| list.item(i)).filter_map(|n| n.dyn_into::<web_sys::Element>().ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_px_accepts_lengths_and_rejects_garbage() {
        assert_eq!(parse_px("32px"), Some(32.0));
        assert_eq!(parse_px(" 44.5px "), Some(44.5));
        assert_eq!(parse_px("28"), Some(28.0));
        assert_eq!(parse_px(""), None);
        assert_eq!(parse_px("0px"), None);
        assert_eq!(parse_px("-4px"), None);
        assert_eq!(parse_px("NaNpx"), None);
        assert_eq!(parse_px("inf"), None);
        assert_eq!(parse_px("2rem"), None);
    }
}
