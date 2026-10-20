//! The calendar views: the time grid (day, work week, week) with an all-day row, the
//! current-time line, side-by-side overlaps and hatched free/busy layers; the month grid; and the
//! agenda list. Layout math lives in [`super::calendar_layout`].

use leptos::html;
use leptos::prelude::*;
use serde_json::json;

use super::calendar::{Busy, Ev, ViewModel, open_form};
use super::calendar_date::{Ymd, hour_label};
use super::calendar_icons::{CalIcon, KIcon};
use super::calendar_layout::{Lane, Timed, lanes, minutes_to_px, overflow, px_to_minutes, timed_segments};
use crate::state::{AppState, Area, app_state};

/// Fallback height of one hour in the time grid (the `--cal-hour` token wins).
const HOUR_PX: f64 = 48.0;
/// All-day lanes shown before "+N".
const ALL_DAY_LANES: usize = 3;
/// Month cell lanes shown before "+N more".
const MONTH_LANES: usize = 3;
/// Snap for drag-to-create, minutes.
const SNAP: u32 = 15;

fn hour_px() -> f64 {
    crate::dom::document()
        .and_then(|d| d.document_element())
        .and_then(|el| crate::dom::css_var(&el, "--cal-hour"))
        .and_then(|v| crate::dom::parse_px(&v))
        .filter(|v| v.is_finite() && *v >= 16.0 && *v <= 200.0)
        .unwrap_or(HOUR_PX)
}

fn select(state: AppState, id: &str) {
    super::calendar::ui().form.set(None);
    state.selection(Area::Calendar).item.set(Some(id.to_owned()));
}

fn day_date(m: &ViewModel, day: usize) -> Option<Ymd> {
    m.days.get(day).and_then(|d| Ymd::parse(&d.date))
}

fn chip_icons(e: &Ev) -> impl IntoView + use<> {
    let (recurring, online, private) = (e.is_recurring, e.is_online, e.is_private);
    view! {
        {recurring.then(|| view! { <span class="cal-ev-flag" title="Recurring"><KIcon name=CalIcon::Repeat /></span> })}
        {online.then(|| view! { <span class="cal-ev-flag" title="Online meeting"><KIcon name=CalIcon::Video /></span> })}
        {private.then(|| view! { <span class="cal-ev-flag" title="Private"><KIcon name=CalIcon::Lock /></span> })}
    }
}

/// Day, work week and week.
#[component]
pub fn TimeGrid(
    /// The view model.
    data: Memo<ViewModel>,
) -> impl IntoView {
    let state = app_state();
    let sel = state.selection(Area::Calendar);
    let hour = hour_px();
    let scroller = NodeRef::<html::Div>::new();
    let expanded = RwSignal::new(false);
    // Scroll to the start of the working day when the view or its range changes.
    Effect::new(move |prev: Option<(String, String)>| {
        let key = data.with(|m| (m.view.clone(), m.days.first().map(|d| d.date.clone()).unwrap_or_default()));
        let work = data.with(|m| m.work_start);
        if prev.as_ref() != Some(&key) && !key.1.is_empty() {
            request_animation_frame(move || {
                if let Some(el) = scroller.get_untracked() {
                    let top = minutes_to_px(work.saturating_sub(30), hour);
                    el.set_scroll_top(i32::try_from(top as i64).unwrap_or(0));
                }
            });
        }
        key
    });
    let n = move || data.with(|m| m.days.len().max(1));
    let cols = move || format!("--days: {}", n());

    // Drag to create: (day, anchor minute, current minute).
    let drag = RwSignal::new(None::<(usize, u32, u32)>);

    view! {
        <div class="cal-grid" style=cols>
            <div class="cal-grid-head" role="row">
                <div class="cal-gutter" aria-hidden="true"></div>
                {move || {
                    data.with(|m| {
                        m.days
                            .iter()
                            .map(|d| {
                                let date = d.date.clone();
                                view! {
                                    <button
                                        class="cal-day-head"
                                        class:today=d.is_today
                                        class:weekend=d.is_weekend
                                        role="columnheader"
                                        title="Show this day"
                                        on:click=move |_| crate::commands::dispatch(state, "calendar.go", json!({ "to": date, "view": "day" }))
                                    >
                                        <span class="cal-day-wd">{d.weekday.clone()}</span>
                                        <span class="cal-day-num">{d.day}</span>
                                    </button>
                                }
                            })
                            .collect_view()
                    })
                }}
            </div>
            <AllDayRow data=data expanded=expanded />
            <div class="cal-scroll" node_ref=scroller tabindex="0" data-primary-list="" aria-label="Time grid">
                <div class="cal-hours" style=format!("height: {}px", minutes_to_px(1440, hour))>
                    <div class="cal-gutter-col" aria-hidden="true">
                        {(1..24).map(|h| view! { <span class="cal-hour-label" style=format!("top: {}px", minutes_to_px(h * 60, hour))>{hour_label(h)}</span> }).collect_view()}
                    </div>
                    <div class="cal-columns">
                        <div class="cal-workband" aria-hidden="true" style=move || {
                            let (s, e) = data.with(|m| (m.work_start, m.work_end));
                            format!("top: {}px; height: {}px", minutes_to_px(s, hour), minutes_to_px(e.saturating_sub(s), hour))
                        }></div>
                        {(1..24).map(|h| view! { <div class="cal-hour-line" aria-hidden="true" style=format!("top: {}px", minutes_to_px(h * 60, hour))></div> }).collect_view()}
                        {move || {
                            let m = data.get();
                            let days = m.days.len();
                            let timed: Vec<&Ev> = m.events.iter().filter(|e| !e.all_day).collect();
                            let segs = timed_segments(
                                &timed.iter().map(|e| Timed { start_day: e.start_day, end_day: e.end_day, start_min: e.start_min, end_min: e.end_min }).collect::<Vec<_>>(),
                                days,
                                20,
                            );
                            let busy: Vec<Busy> = m.busy.clone();
                            (0..days)
                                .map(|day| {
                                    let d = m.days.get(day).cloned().unwrap_or_default();
                                    let date = day_date(&m, day);
                                    let now = d.is_today.then_some(m.now_min);
                                    let chips: Vec<_> = segs
                                        .iter()
                                        .filter(|s| s.day == day)
                                        .filter_map(|s| timed.get(s.idx).map(|e| (*s, (*e).clone())))
                                        .collect();
                                    let blocks: Vec<Busy> = busy
                                        .iter()
                                        .filter(|b| b.start_day <= day as i64 && b.end_day >= day as i64)
                                        .cloned()
                                        .collect();
                                    view! {
                                        <div
                                            class="cal-col"
                                            class:today=d.is_today
                                            class:weekend=d.is_weekend
                                            role="gridcell"
                                            aria-label=d.label.clone()
                                            on:pointerdown=move |e: web_sys::PointerEvent| {
                                                if e.button() != 0 {
                                                    return;
                                                }
                                                let m = px_to_minutes(f64::from(e.offset_y()), hour, SNAP);
                                                if let Some(t) = e.current_target().and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok()) {
                                                    let _ = t.set_pointer_capture(e.pointer_id());
                                                }
                                                drag.set(Some((day, m, m)));
                                            }
                                            on:pointermove=move |e: web_sys::PointerEvent| {
                                                if let Some((dd, a, _)) = drag.get_untracked() && dd == day {
                                                    let m = px_to_minutes(f64::from(e.offset_y()), hour, SNAP);
                                                    drag.set(Some((dd, a, m)));
                                                }
                                            }
                                            on:pointerup=move |_| {
                                                if let Some((dd, a, b)) = drag.get_untracked() && dd == day {
                                                    drag.set(None);
                                                    let (s, e) = if b >= a { (a, b + SNAP) } else { (b, a + SNAP) };
                                                    if e.saturating_sub(s) > SNAP && let Some(date) = date {
                                                        open_form(state, super::calendar::Draft::at(date, s, e - s));
                                                    } else {
                                                        sel.item.set(None);
                                                    }
                                                }
                                            }
                                            on:pointercancel=move |_| drag.set(None)
                                            on:dblclick=move |e: web_sys::MouseEvent| {
                                                let m = px_to_minutes(f64::from(e.offset_y()), hour, 30);
                                                if let Some(date) = date {
                                                    open_form(state, super::calendar::Draft::at(date, m, 30));
                                                }
                                            }
                                        >
                                            {blocks
                                                .into_iter()
                                                .map(|b| {
                                                    let top = if b.start_day == day as i64 { b.start_min } else { 0 };
                                                    let bottom = if b.end_day == day as i64 { b.end_min } else { 1440 };
                                                    view! {
                                                        <div
                                                            class=format!("cal-busy is-{}", b.status)
                                                            style=format!("top: {}px; height: {}px; --ev: {}", minutes_to_px(top, hour), minutes_to_px(bottom.saturating_sub(top).max(15), hour), b.color)
                                                            title=format!("{} · {} · {}", b.name, b.status, b.time_label)
                                                        >
                                                            <span class="cal-busy-label">{b.name.clone()}</span>
                                                        </div>
                                                    }
                                                })
                                                .collect_view()}
                                            {chips
                                                .into_iter()
                                                .map(|(s, e)| {
                                                    let id = e.id.clone();
                                                    let id2 = e.id.clone();
                                                    let height = minutes_to_px(s.bottom.saturating_sub(s.top), hour);
                                                    let gap = 2.0;
                                                    let style = format!(
                                                        "top: {}px; height: {}px; left: calc({}% + 1px); width: calc({}% - {}px); --ev: {}",
                                                        minutes_to_px(s.top, hour),
                                                        (height - 1.0).max(14.0),
                                                        s.col as f64 * 100.0 / s.cols as f64,
                                                        100.0 / s.cols as f64,
                                                        gap,
                                                        e.color
                                                    );
                                                    let compact = height < 34.0;
                                                    let selected = move || sel.item.with(|x| x.as_deref() == Some(id2.as_str()));
                                                    let label = e.aria();
                                                    view! {
                                                        <button
                                                            class=e.status_class()
                                                            class:compact=compact
                                                            class:selected=selected
                                                            class:continued=!s.starts
                                                            style=style
                                                            data-event-id=id.clone()
                                                            aria-label=label
                                                            on:pointerdown=|e: web_sys::PointerEvent| e.stop_propagation()
                                                            on:dblclick=|e: web_sys::MouseEvent| e.stop_propagation()
                                                            on:click=move |_| select(state, &id)
                                                        >
                                                            <span class="cal-ev-title">{chip_icons(&e)}{e.subject.clone()}</span>
                                                            {(!compact).then(|| view! { <span class="cal-ev-meta">{e.time_label.clone()}</span> })}
                                                            {(!compact && !e.location.is_empty()).then(|| view! { <span class="cal-ev-meta">{e.location.clone()}</span> })}
                                                        </button>
                                                    }
                                                })
                                                .collect_view()}
                                            {move || {
                                                drag.get().filter(|(dd, _, _)| *dd == day).map(|(_, a, b)| {
                                                    let (s, e) = if b >= a { (a, b + SNAP) } else { (b, a + SNAP) };
                                                    view! {
                                                        <div class="cal-ghost" style=format!("top: {}px; height: {}px", minutes_to_px(s, hour), minutes_to_px(e - s, hour))>
                                                            {format!("{} – {}", super::calendar_date::hhmm(s), super::calendar_date::hhmm(e))}
                                                        </div>
                                                    }
                                                })
                                            }}
                                            {now.map(|now| {
                                                let top = minutes_to_px(u32::try_from(now.clamp(0, 1439)).unwrap_or(0), hour);
                                                view! { <div class="cal-now" style=format!("top: {top}px") aria-hidden="true"></div> }
                                            })}
                                        </div>
                                    }
                                })
                                .collect_view()
                        }}
                    </div>
                </div>
            </div>
        </div>
    }
}

#[component]
fn AllDayRow(data: Memo<ViewModel>, expanded: RwSignal<bool>) -> impl IntoView {
    let state = app_state();
    let sel = state.selection(Area::Calendar);
    view! {
        <div class="cal-allday" role="row" aria-label="All-day events">
            <div class="cal-gutter cal-allday-label">"All day"</div>
            {move || {
                let m = data.get();
                let days = m.days.len().max(1);
                let all: Vec<&Ev> = m.events.iter().filter(|e| e.all_day).collect();
                let spans: Vec<(i64, i64)> = all.iter().map(|e| (e.start_day, e.end_day)).collect();
                let (placed, count) = lanes(&spans, days);
                let limit = if expanded.get() { usize::MAX } else { ALL_DAY_LANES };
                let extra = overflow(&placed, days, limit);
                let hidden = extra.iter().any(|n| *n > 0);
                view! {
                    <div class="cal-allday-grid" style=format!("grid-template-rows: repeat({}, var(--cal-lane))", count.min(limit).max(1) + usize::from(hidden))>
                        {placed
                            .iter()
                            .filter(|l| l.lane < limit)
                            .filter_map(|l| all.get(l.idx).map(|e| (*l, (*e).clone())))
                            .map(|(l, e)| lane_chip(state, sel.item, l, e, false))
                            .collect_view()}
                        {extra
                            .iter()
                            .enumerate()
                            .filter(|(_, n)| **n > 0)
                            .map(|(d, n)| {
                                view! {
                                    <button
                                        class="cal-more"
                                        style=format!("grid-column: {}; grid-row: {}", d + 1, limit.min(count) + 1)
                                        on:click=move |_| expanded.set(true)
                                    >
                                        {format!("+{n}")}
                                    </button>
                                }
                            })
                            .collect_view()}
                        {(expanded.get() && count > ALL_DAY_LANES).then(|| view! {
                            <button class="cal-more cal-less" style=format!("grid-column: {days}; grid-row: {}", count + 1) on:click=move |_| expanded.set(false)>"Less"</button>
                        })}
                    </div>
                }
            }}
        </div>
    }
}

fn lane_chip(state: AppState, selected: RwSignal<Option<String>>, l: Lane, e: Ev, month: bool) -> AnyView {
    let id = e.id.clone();
    let id2 = e.id.clone();
    let style = format!(
        "grid-column: {} / span {}; grid-row: {}; --ev: {}",
        l.first + 1,
        l.last - l.first + 1,
        l.lane + if month { 2 } else { 1 },
        e.color
    );
    let timed = month && !e.all_day;
    let class =
        if timed { format!("{} cal-ev-dot", e.status_class()) } else { format!("{} cal-ev-bar", e.status_class()) };
    let is_sel = move || selected.with(|x| x.as_deref() == Some(id2.as_str()));
    let time = if timed { super::calendar_date::hhmm(e.start_min) } else { String::new() };
    view! {
        <button
            class=class
            class:selected=is_sel
            class:cont-left=l.cont_left
            class:cont-right=l.cont_right
            style=style
            data-event-id=id.clone()
            aria-label=e.aria()
            on:click=move |ev: web_sys::MouseEvent| {
                ev.stop_propagation();
                select(state, &id);
            }
        >
            {timed.then(|| view! { <span class="cal-ev-time">{time}</span> })}
            <span class="cal-ev-title">{chip_icons(&e)}{e.subject.clone()}</span>
        </button>
    }
    .into_any()
}

/// The month grid: six weeks, events packed into lanes per week.
#[component]
pub fn MonthView(
    /// The view model.
    data: Memo<ViewModel>,
) -> impl IntoView {
    let state = app_state();
    let sel = state.selection(Area::Calendar);
    view! {
        <div class="cal-month" role="grid" aria-label="Month">
            <div class="cal-month-head" role="row">
                {["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].into_iter().map(|d| view! { <span role="columnheader">{d}</span> }).collect_view()}
            </div>
            <div class="cal-month-body" tabindex="0" data-primary-list="">
                {move || {
                    let m = data.get();
                    let weeks = m.days.len() / 7;
                    (0..weeks)
                        .map(|w| {
                            let base = i64::try_from(w * 7).unwrap_or(0);
                            let week: Vec<Ev> = m
                                .events
                                .iter()
                                .filter(|e| e.end_day >= base && e.start_day < base + 7)
                                .map(|e| Ev { start_day: e.start_day - base, end_day: e.end_day - base, ..e.clone() })
                                .collect();
                            let spans: Vec<(i64, i64)> = week.iter().map(|e| (e.start_day, e.end_day)).collect();
                            let (placed, _) = lanes(&spans, 7);
                            let extra = overflow(&placed, 7, MONTH_LANES);
                            let days: Vec<_> = (0..7).filter_map(|i| m.days.get(w * 7 + i).cloned()).collect();
                            view! {
                                <div class="cal-week" role="row">
                                    {days
                                        .iter()
                                        .enumerate()
                                        .map(|(i, d)| {
                                            let date = d.date.clone();
                                            let date2 = d.date.clone();
                                            let label = Ymd::parse(&d.date).map(Ymd::short).unwrap_or_default();
                                            view! {
                                                <div
                                                    class="cal-cell"
                                                    class:out=!d.in_month
                                                    class:today=d.is_today
                                                    class:weekend=d.is_weekend
                                                    style=format!("grid-column: {}; grid-row: 1 / -1", i + 1)
                                                    role="gridcell"
                                                    aria-label=label
                                                    on:click=move |_| sel.item.set(None)
                                                    on:dblclick=move |_| {
                                                        if let Some(date) = Ymd::parse(&date2) {
                                                            open_form(state, super::calendar::Draft::at(date, 9 * 60, 30));
                                                        }
                                                    }
                                                ></div>
                                                <button
                                                    class="cal-cell-num"
                                                    class:today=d.is_today
                                                    style=format!("grid-column: {}; grid-row: 1", i + 1)
                                                    title="Show this day"
                                                    on:click=move |_| crate::commands::dispatch(state, "calendar.go", json!({ "to": date, "view": "day" }))
                                                >
                                                    {d.day}
                                                </button>
                                            }
                                        })
                                        .collect_view()}
                                    {placed
                                        .iter()
                                        .filter(|l| l.lane < MONTH_LANES)
                                        .filter_map(|l| week.get(l.idx).map(|e| (*l, e.clone())))
                                        .map(|(l, e)| lane_chip(state, sel.item, l, e, true))
                                        .collect_view()}
                                    {extra
                                        .iter()
                                        .enumerate()
                                        .filter(|(_, n)| **n > 0)
                                        .map(|(i, n)| {
                                            let date = days.get(i).map(|d| d.date.clone()).unwrap_or_default();
                                            view! {
                                                <button
                                                    class="cal-more"
                                                    style=format!("grid-column: {}; grid-row: {}", i + 1, MONTH_LANES + 2)
                                                    on:click=move |_| crate::commands::dispatch(state, "calendar.go", json!({ "to": date, "view": "day" }))
                                                >
                                                    {format!("+{n} more")}
                                                </button>
                                            }
                                        })
                                        .collect_view()}
                                </div>
                            }
                        })
                        .collect_view()
                }}
            </div>
        </div>
    }
}

/// The agenda: the coming days as a list.
#[component]
pub fn AgendaView(
    /// The view model.
    data: Memo<ViewModel>,
) -> impl IntoView {
    let state = app_state();
    let sel = state.selection(Area::Calendar);
    view! {
        <div class="cal-agenda" tabindex="0" data-primary-list="" role="list" aria-label="Agenda">
            {move || {
                let m = data.get();
                let mut any = false;
                let rows = m
                    .days
                    .iter()
                    .enumerate()
                    .filter_map(|(i, d)| {
                        let day = i as i64;
                        let events: Vec<Ev> = m.events.iter().filter(|e| e.start_day <= day && e.end_day >= day).cloned().collect();
                        if events.is_empty() && !d.is_today {
                            return None;
                        }
                        any = any || !events.is_empty();
                        let title = Ymd::parse(&d.date).map(Ymd::short).unwrap_or_default();
                        Some(view! {
                            <section class="cal-agenda-day" class:today=d.is_today>
                                <h3 class="cal-agenda-date">
                                    {title}
                                    {d.is_today.then(|| view! { <span class="cal-badge">"Today"</span> })}
                                </h3>
                                {events.is_empty().then(|| view! { <p class="placeholder-note">"Nothing scheduled"</p> })}
                                {events
                                    .into_iter()
                                    .map(|e| {
                                        let id = e.id.clone();
                                        let id2 = e.id.clone();
                                        let join = e.id.clone();
                                        let is_sel = move || sel.item.with(|x| x.as_deref() == Some(id2.as_str()));
                                        view! {
                                            <div class="cal-agenda-row" role="listitem" style=format!("--ev: {}", e.color)>
                                                <button
                                                    class=format!("{} cal-agenda-ev", e.status_class())
                                                    class:selected=is_sel
                                                    data-event-id=id.clone()
                                                    aria-label=e.aria()
                                                    on:click=move |_| select(state, &id)
                                                >
                                                    <span class="cal-agenda-time">{e.time_label.clone()}</span>
                                                    <span class="cal-agenda-bar" aria-hidden="true"></span>
                                                    <span class="cal-agenda-text">
                                                        <span class="cal-ev-title">{chip_icons(&e)}{e.subject.clone()}</span>
                                                        <span class="cal-ev-meta">
                                                            {[e.location.clone(), e.source_name.clone()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ")}
                                                        </span>
                                                    </span>
                                                </button>
                                                {e.can_join.then(|| view! {
                                                    <button class="cal-join" on:click=move |_| crate::commands::dispatch(state, "calendar.join", json!({ "id": join }))>
                                                        <KIcon name=CalIcon::Video />
                                                        "Join"
                                                    </button>
                                                })}
                                            </div>
                                        }
                                    })
                                    .collect_view()}
                            </section>
                        })
                    })
                    .collect_view();
                view! {
                    {rows}
                    {(!any).then(|| view! {
                        <crate::components::EmptyState icon=crate::icons::IconName::Calendar title="Nothing coming up" detail="Events in the next two weeks show here." />
                    })}
                }
            }}
        </div>
    }
}
