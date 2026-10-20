//! The command palette (⌘K): fuzzy search over every command (engine registry + UI commands),
//! arrow keys / Enter / Escape, shortcut hints and a marker on destructive commands.

use leptos::html;
use leptos::prelude::*;
use quorum_ipc::CommandInfo;
use wasm_bindgen::JsCast;

use crate::components::icon::Icon;
use crate::icons::IconName;
use crate::state::app_state;

/// Longest text and query the matcher looks at (bounds the work per keystroke).
const MAX_TEXT: usize = 160;
const MAX_QUERY: usize = 48;
/// Most results shown.
const MAX_RESULTS: usize = 60;

/// A fuzzy match of a query in a text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyMatch {
    /// Higher is better.
    pub score: i32,
    /// Char indices of `text` that matched, ascending.
    pub positions: Vec<usize>,
}

const NONE: i32 = i32::MIN / 4;
const BONUS_MATCH: i32 = 1;
const BONUS_WORD_START: i32 = 8;
const BONUS_CONSECUTIVE: i32 = 6;
const BONUS_FIRST_CHAR: i32 = 4;
const PENALTY_GAP: i32 = 2;

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn is_word_start(prev: Option<char>, cur: char) -> bool {
    match prev {
        None => true,
        Some(p) => !p.is_alphanumeric() || (p.is_lowercase() && cur.is_uppercase()),
    }
}

/// Matches `query` as a subsequence of `text`, case-insensitively, preferring word starts and
/// runs of consecutive characters. Whitespace in the query is ignored. `None` if no match.
///
/// Dynamic programming over (query char, text position), O(query × text) with both capped.
pub fn fuzzy_match(query: &str, text: &str) -> Option<FuzzyMatch> {
    let q: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).take(MAX_QUERY).map(fold).collect();
    if q.is_empty() {
        return Some(FuzzyMatch { score: 0, positions: Vec::new() });
    }
    let orig: Vec<char> = text.chars().take(MAX_TEXT).collect();
    let t: Vec<char> = orig.iter().copied().map(fold).collect();
    let (m, n) = (q.len(), t.len());
    if m > n {
        return None;
    }
    let bonus: Vec<i32> = (0..n)
        .map(|i| {
            let prev = i.checked_sub(1).and_then(|p| orig.get(p)).copied();
            let cur = orig.get(i).copied().unwrap_or(' ');
            let mut b = BONUS_MATCH;
            if is_word_start(prev, cur) {
                b += BONUS_WORD_START;
            }
            if i == 0 {
                b += BONUS_FIRST_CHAR;
            }
            b
        })
        .collect();
    // score[j * n + i]: best score with q[j] matched at t[i]; from[..]: previous match index.
    let mut score = vec![NONE; m * n];
    let mut from = vec![usize::MAX; m * n];
    for (j, qc) in q.iter().enumerate() {
        let mut best_gap = NONE; // best score[j-1][k] for k < i-1
        let mut best_gap_at = usize::MAX;
        for i in 0..n {
            if j > 0 && i >= 2 {
                let k = i - 2;
                let s = score.get((j - 1) * n + k).copied().unwrap_or(NONE);
                if s > best_gap {
                    best_gap = s;
                    best_gap_at = k;
                }
            }
            if t.get(i) != Some(qc) {
                continue;
            }
            let b = bonus.get(i).copied().unwrap_or(0);
            let (s, f) = if j == 0 {
                // Leading unmatched characters cost a little.
                (b - i32::try_from(i.min(6)).unwrap_or(6), usize::MAX)
            } else {
                let consecutive = i
                    .checked_sub(1)
                    .and_then(|k| score.get((j - 1) * n + k).copied())
                    .filter(|s| *s > NONE)
                    .map(|s| (s + BONUS_CONSECUTIVE, i - 1));
                let gapped = (best_gap > NONE).then_some((best_gap - PENALTY_GAP, best_gap_at));
                match (consecutive, gapped) {
                    (Some(c), Some(g)) => {
                        if c.0 >= g.0 {
                            (c.0 + b, c.1)
                        } else {
                            (g.0 + b, g.1)
                        }
                    }
                    (Some(c), None) => (c.0 + b, c.1),
                    (None, Some(g)) => (g.0 + b, g.1),
                    (None, None) => continue,
                }
            };
            if let (Some(sc), Some(fr)) = (score.get_mut(j * n + i), from.get_mut(j * n + i)) {
                *sc = s;
                *fr = f;
            }
        }
    }
    let last = (m - 1) * n;
    let (end, best) = (0..n)
        .filter_map(|i| score.get(last + i).map(|s| (i, *s)))
        .filter(|(_, s)| *s > NONE)
        .max_by_key(|(i, s)| (*s, std::cmp::Reverse(*i)))?;
    let mut positions = vec![0; m];
    let mut at = end;
    for j in (0..m).rev() {
        if let Some(p) = positions.get_mut(j) {
            *p = at;
        }
        at = from.get(j * n + at).copied().unwrap_or(usize::MAX);
        if at == usize::MAX && j > 0 {
            return None; // inconsistent table; treat as no match rather than guess
        }
    }
    // Slightly prefer shorter texts.
    let length_penalty = i32::try_from(n / 12).unwrap_or(0);
    Some(FuzzyMatch { score: best - length_penalty, positions })
}

/// One palette row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ranked {
    /// The command.
    pub info: CommandInfo,
    /// Match score.
    pub score: i32,
    /// Matched char positions in the label (empty when the id matched instead).
    pub label_positions: Vec<usize>,
}

/// Filters and orders `commands` for `query`: best label or id match first, ties by label.
pub fn rank(query: &str, commands: &[CommandInfo]) -> Vec<Ranked> {
    let mut out: Vec<Ranked> = commands
        .iter()
        .filter(|c| c.id != "palette.open" && c.id != "view.open")
        .filter_map(|c| {
            let by_label = fuzzy_match(query, &c.label);
            // Ids match too (e.g. "mail.rep"), slightly below an equal label match.
            let by_id = fuzzy_match(query, &c.id).map(|m| m.score - 3);
            match (by_label, by_id) {
                (Some(l), Some(i)) if i > l.score => {
                    Some(Ranked { info: c.clone(), score: i, label_positions: Vec::new() })
                }
                (Some(l), _) => Some(Ranked { info: c.clone(), score: l.score, label_positions: l.positions }),
                (None, Some(i)) => Some(Ranked { info: c.clone(), score: i, label_positions: Vec::new() }),
                (None, None) => None,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.info.enabled.cmp(&a.info.enabled).then(b.score.cmp(&a.score)).then_with(|| a.info.label.cmp(&b.info.label))
    });
    out.truncate(MAX_RESULTS);
    out
}

/// Splits `label` into (text, highlighted) runs for the matched `positions`.
pub fn highlight_runs(label: &str, positions: &[usize]) -> Vec<(String, bool)> {
    let mut runs: Vec<(String, bool)> = Vec::new();
    for (i, c) in label.chars().enumerate() {
        let hit = positions.binary_search(&i).is_ok();
        match runs.last_mut() {
            Some((text, h)) if *h == hit => text.push(c),
            _ => runs.push((c.to_string(), hit)),
        }
    }
    runs
}

/// The palette overlay. Mounted once by the shell; shown while `palette_open` is set.
#[component]
pub fn CommandPalette() -> impl IntoView {
    let state = app_state();
    let active = RwSignal::new(0usize);
    let input_ref = NodeRef::<html::Input>::new();
    let restore_focus = StoredValue::new_local(None::<web_sys::Element>);

    let results = Memo::new(move |_| {
        let q = state.palette_query.get();
        rank(&q, &state.all_commands_tracked())
    });

    // Focus the input on open; remember and restore the previously focused element.
    Effect::new(move |was_open: Option<bool>| {
        let open = state.palette_open.get();
        if open && was_open != Some(true) {
            restore_focus.set_value(crate::dom::active_element());
            active.set(0);
            request_animation_frame(move || {
                if let Some(input) = input_ref.get_untracked() {
                    let _ = input.focus();
                    input.select();
                }
            });
        } else if !open && was_open == Some(true) {
            let prev = restore_focus.try_with_value(Clone::clone).flatten();
            if let Some(el) = prev {
                let _ = crate::dom::focus_element(&el);
            }
        }
        open
    });

    // Keep the active option scrolled into view.
    Effect::new(move |_| {
        let i = active.get();
        if !state.palette_open.get() {
            return;
        }
        request_animation_frame(move || {
            if let Some(el) = crate::dom::document().and_then(|d| d.get_element_by_id(&option_id(i))) {
                el.scroll_into_view_with_bool(false);
            }
        });
    });

    let close = move || {
        state.palette_open.set(false);
    };
    let run_at = move |i: usize| {
        let Some(item) = results.with_untracked(|r| r.get(i).cloned()) else { return };
        if !item.info.enabled {
            state.toast(crate::state::ToastLevel::Warning, format!("{} isn't available right now.", item.info.label));
            return;
        }
        state.palette_open.set(false);
        let id = item.info.id;
        // Run after focus is restored so context (area, selection) is the user's, not the palette's.
        request_animation_frame(move || {
            crate::commands::dispatch(state, &id, crate::commands::context_params(state, &id));
        });
    };

    let on_keydown = move |e: web_sys::KeyboardEvent| {
        let len = results.with_untracked(Vec::len);
        let mac = crate::dom::is_mac();
        let toggle = crate::keys::Shortcut::from_event_parts(
            &e.key(),
            &e.code(),
            e.meta_key(),
            e.ctrl_key(),
            e.alt_key(),
            e.shift_key(),
            mac,
        ) == crate::keys::Shortcut::parse("Cmd+K", mac);
        match e.key().as_str() {
            "ArrowDown" => {
                e.prevent_default();
                if len > 0 {
                    active.update(|a| *a = (*a + 1) % len);
                }
            }
            "ArrowUp" => {
                e.prevent_default();
                if len > 0 {
                    active.update(|a| *a = if *a == 0 { len - 1 } else { *a - 1 });
                }
            }
            "PageDown" => {
                e.prevent_default();
                active.set(len.saturating_sub(1));
            }
            "PageUp" => {
                e.prevent_default();
                active.set(0);
            }
            "Enter" => {
                e.prevent_default();
                run_at(active.get_untracked());
            }
            "Escape" => {
                e.prevent_default();
                e.stop_propagation();
                close();
            }
            "Tab" => e.prevent_default(), // focus stays in the dialog
            _ if toggle => {
                e.prevent_default();
                close();
            }
            _ => {}
        }
    };

    view! {
        <Show when=move || state.palette_open.get()>
            <div class="palette-backdrop" on:mousedown=move |_| close()>
                <div
                    class="palette"
                    role="dialog"
                    aria-modal="true"
                    aria-label="Command palette"
                    on:mousedown=|e| e.stop_propagation()
                >
                    <div class="palette-input-row">
                        <Icon name=IconName::Command />
                        <input
                            node_ref=input_ref
                            class="palette-input"
                            type="text"
                            role="combobox"
                            aria-expanded="true"
                            aria-controls="palette-listbox"
                            aria-autocomplete="list"
                            aria-label="Command"
                            placeholder="Type a command"
                            spellcheck="false"
                            autocomplete="off"
                            aria-activedescendant=move || {
                                let i = active.get();
                                results.with(|r| r.get(i).map(|_| option_id(i)))
                            }
                            prop:value=move || state.palette_query.get()
                            on:input=move |e| {
                                let v = e
                                    .target()
                                    .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
                                    .map(|i| i.value())
                                    .unwrap_or_default();
                                state.palette_query.set(v);
                                active.set(0);
                            }
                            on:keydown=on_keydown
                        />
                    </div>
                    <ul class="palette-list" id="palette-listbox" role="listbox" aria-label="Commands">
                        {move || {
                            let items = results.get();
                            if items.is_empty() {
                                return view! { <li class="palette-empty" role="presentation">"No matching commands"</li> }
                                    .into_any();
                            }
                            items
                                .into_iter()
                                .enumerate()
                                .map(|(i, item)| palette_row(i, item, active, run_at))
                                .collect_view()
                                .into_any()
                        }}
                    </ul>
                </div>
            </div>
        </Show>
    }
}

fn option_id(i: usize) -> String {
    format!("palette-opt-{i}")
}

fn palette_row(
    i: usize,
    item: Ranked,
    active: RwSignal<usize>,
    run_at: impl Fn(usize) + Copy + 'static,
) -> impl IntoView {
    let mac = crate::dom::is_mac();
    let shortcut = item.info.shortcut.as_deref().map(|s| crate::keys::display_shortcut(s, mac));
    let runs = highlight_runs(&item.info.label, &item.label_positions);
    let destructive = item.info.destructive;
    let enabled = item.info.enabled;
    let id = item.info.id.clone();
    view! {
        <li
            id=option_id(i)
            class="palette-option"
            class:active=move || active.get() == i
            class:disabled=!enabled
            role="option"
            aria-selected=move || if active.get() == i { "true" } else { "false" }
            aria-disabled=if enabled { "false" } else { "true" }
            on:mousemove=move |_| {
                if active.get_untracked() != i {
                    active.set(i);
                }
            }
            on:click=move |_| run_at(i)
        >
            <span class="palette-label">
                {runs
                    .into_iter()
                    .map(|(text, hit)| {
                        if hit { view! { <mark>{text}</mark> }.into_any() } else { view! { <span>{text}</span> }.into_any() }
                    })
                    .collect_view()}
            </span>
            <span class="palette-id">{id}</span>
            {destructive
                .then(|| {
                    view! {
                        <span class="palette-destructive" title="Sends, posts, deletes or shares">
                            <span aria-hidden="true">"●"</span>
                            <span class="sr-only">"(sends, posts, deletes or shares)"</span>
                        </span>
                    }
                })}
            {shortcut.map(|s| view! { <kbd class="palette-shortcut">{s}</kbd> })}
        </li>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(id: &str, label: &str) -> CommandInfo {
        CommandInfo {
            id: id.into(),
            label: label.into(),
            menu: None,
            shortcut: None,
            destructive: false,
            enabled: true,
        }
    }

    #[test]
    fn empty_query_matches_everything() {
        assert_eq!(fuzzy_match("", "Reply"), Some(FuzzyMatch { score: 0, positions: vec![] }));
        assert_eq!(fuzzy_match("   ", "Reply").map(|m| m.score), Some(0));
    }

    #[test]
    fn subsequence_is_required() {
        assert!(fuzzy_match("rpl", "Reply").is_some());
        assert!(fuzzy_match("xyz", "Reply").is_none());
        assert!(fuzzy_match("replyall", "Reply").is_none());
        assert!(fuzzy_match("lr", "Reply").is_none()); // order matters
    }

    #[test]
    fn case_insensitive_with_positions() {
        let m = fuzzy_match("GTM", "Go to Mail");
        assert_eq!(m.map(|m| m.positions), Some(vec![0, 3, 6]));
    }

    #[test]
    fn prefers_word_starts_and_runs() {
        let word = fuzzy_match("ra", "Reply All").map(|m| m.score).unwrap_or(NONE);
        let inner = fuzzy_match("ra", "Prepare").map(|m| m.score).unwrap_or(NONE);
        assert!(word > inner, "{word} vs {inner}");
        let run = fuzzy_match("rep", "Reply").map(|m| m.score).unwrap_or(NONE);
        let scattered = fuzzy_match("rep", "Rename the paper").map(|m| m.score).unwrap_or(NONE);
        assert!(run > scattered, "{run} vs {scattered}");
        // Picks the word-start occurrence over an earlier inner one.
        assert_eq!(fuzzy_match("m", "Format Mail").map(|m| m.positions), Some(vec![7]));
    }

    #[test]
    fn hostile_text_never_panics() {
        let long = "ä".repeat(10_000);
        assert!(fuzzy_match("ää", &long).is_some());
        assert!(fuzzy_match(&"a".repeat(1000), "a").is_none());
        assert!(fuzzy_match("İ", "İstanbul").is_some());
        assert!(fuzzy_match("\u{0}", "\u{0}x").is_some());
    }

    #[test]
    fn rank_orders_by_score_and_hides_palette_itself() {
        let cmds = vec![
            cmd("palette.open", "Command Palette"),
            cmd("mail.reply_all", "Reply All"),
            cmd("mail.reply", "Reply"),
            cmd("drive.prepare", "Prepare"),
        ];
        let r = rank("rep", &cmds);
        let ids: Vec<&str> = r.iter().map(|r| r.info.id.as_str()).collect();
        assert_eq!(ids.first(), Some(&"mail.reply"));
        assert!(!ids.contains(&"palette.open"));
        // Ids match too.
        let by_id = rank("drive.p", &cmds);
        assert_eq!(by_id.first().map(|r| r.info.id.as_str()), Some("drive.prepare"));
    }

    #[test]
    fn disabled_commands_sort_last() {
        let mut off = cmd("a.reply", "Reply");
        off.enabled = false;
        let cmds = vec![off, cmd("b.reply", "Reply later")];
        let r = rank("reply", &cmds);
        assert_eq!(r.first().map(|r| r.info.id.as_str()), Some("b.reply"));
    }

    #[test]
    fn highlight_runs_group_matches() {
        assert_eq!(highlight_runs("Reply", &[0, 1]), vec![("Re".to_owned(), true), ("ply".to_owned(), false)]);
        assert_eq!(highlight_runs("", &[]), Vec::<(String, bool)>::new());
    }
}
