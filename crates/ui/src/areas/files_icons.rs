//! File-type icons for the Files area, drawn in code for this project: a folded page with a
//! simple glyph per kind (lines, grid, slide, chart...), coloured by a `--file-<kind>` token in
//! `style/files.css`. No product icons or third-party artwork (CLAUDE.md, assets).

use leptos::prelude::*;

/// Page outline with a folded corner (24×24 grid).
const PAGE: &str = "M6.5 2.75h7.25l4.5 4.5v13.5a.5.5 0 0 1-.5.5h-11.25a.5.5 0 0 1-.5-.5v-17.5a.5.5 0 0 1 .5-.5z";
const FOLD: &str = "M13.75 2.75v4.5h4.5";

/// Glyph strokes drawn on the page for `kind`.
pub fn glyph(kind: &str) -> &'static [&'static str] {
    match kind {
        "word" | "text" => &["M8.5 11h7", "M8.5 13.75h7", "M8.5 16.5h4.5"],
        "excel" => &["M8.25 10.5h7.5v7h-7.5z", "M8.25 14h7.5", "M12 10.5v7"],
        "powerpoint" => &["M8 10.75h8v5.5h-8z", "M12 16.25v2"],
        "pdf" => &["M8.5 15.5c2-1 4.5-4.5 3.5-6s-2 3.5 3.5 5.5"],
        "image" => &["M8 17.5l2.75-3.25 2 2.25 1.5-1.5 2.25 2.5z", "M14.5 11.25a1 1 0 1 0 0-.01"],
        "video" => &["M10.25 11v6l5-3z"],
        "audio" => &["M11 17.25a1.25 1.25 0 1 1-1.25-1.25H11v-5.5l4-1v5.5", "M15 15.25a1.25 1.25 0 1 1-1.25-1.25H15"],
        "archive" => &["M11.5 3v1.5h1v1.5h-1v1.5h1v1.5h-1", "M11 12h2v3h-2z"],
        "code" => &["M10.25 11.5l-2 2.25 2 2.25", "M13.75 11.5l2 2.25-2 2.25"],
        "onenote" => &["M8.5 10.5h7v7.5h-7z", "M8.5 12.5h-1", "M8.5 14.5h-1", "M8.5 16.5h-1"],
        "visio" => &["M8 10.5h3v2.5h-3z", "M13 15h3v2.5h-3z", "M9.5 13v3.25h3.5"],
        "project" => &["M8.25 11h4", "M10.25 13.75h4.5", "M12.25 16.5h3.5"],
        _ => &[],
    }
}

/// A file or folder icon. `kind` is the engine's `kind` (`folder`, `word`, `excel`, ...).
#[component]
pub fn FileIcon(
    /// The engine's file kind.
    #[prop(into)]
    kind: String,
    /// Extra class (`large` for grid tiles and the details pane).
    #[prop(optional)]
    class: &'static str,
) -> impl IntoView {
    let cls = format!("file-icon file-{kind} {class}");
    if kind == "folder" {
        return view! {
            <svg class=cls viewBox="0 0 24 24" aria-hidden="true" focusable="false">
                <path class="file-folder-back" d="M2.75 6.25a1.5 1.5 0 0 1 1.5-1.5h5l2 2.25h8.5a1.5 1.5 0 0 1 1.5 1.5v9.75a1.5 1.5 0 0 1-1.5 1.5h-15.5a1.5 1.5 0 0 1-1.5-1.5z" />
                <path class="file-folder-front" d="M2.75 9.5h18.5v8.75a1.5 1.5 0 0 1-1.5 1.5h-15.5a1.5 1.5 0 0 1-1.5-1.5z" />
            </svg>
        }
        .into_any();
    }
    let strokes = glyph(&kind);
    view! {
        <svg class=cls viewBox="0 0 24 24" aria-hidden="true" focusable="false">
            <path class="file-page" d=PAGE />
            <path class="file-fold" d=FOLD />
            {strokes.iter().map(|d| view! { <path class="file-glyph" d=*d /> }).collect_view()}
        </svg>
    }
    .into_any()
}

/// A SharePoint site icon: a rounded tile with the site's initial.
#[component]
pub fn SiteIcon(
    /// Site name.
    #[prop(into)]
    name: String,
) -> impl IntoView {
    let initial = name.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().to_string()).unwrap_or_default();
    let hue = crate::components::avatar::colour_index(&name);
    view! { <span class=format!("site-icon avatar-c{hue}") aria-hidden="true">{initial}</span> }
}

#[cfg(test)]
mod tests {
    use super::glyph;

    #[test]
    fn every_kind_draws_within_the_grid() {
        for k in [
            "word",
            "excel",
            "powerpoint",
            "pdf",
            "image",
            "video",
            "audio",
            "archive",
            "code",
            "text",
            "onenote",
            "visio",
            "project",
            "file",
        ] {
            for d in glyph(k) {
                // Only path commands and numbers inside the 24 px grid.
                for n in d.split(|c: char| !(c.is_ascii_digit() || c == '.')).filter(|s| !s.is_empty()) {
                    let v: f64 = n.parse().unwrap_or(0.0);
                    assert!((0.0..=24.0).contains(&v), "{k}: {d}");
                }
            }
        }
        assert!(glyph("unknown").is_empty());
    }
}
