//! Teams: teams/channels tree | channel posts (cards with the latest replies, "New post"
//! composer with a subject) | the selected post's thread with a reply box.
//!
//! Selection: the nav key is `team/channel`, the item is a post id. `j`/`k` move between posts.

use std::collections::HashSet;

use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Deserialize;
use serde_json::{Value, json};

use super::chat::run_cmd;
use super::chat_icons::{CIcon, ChatIcon};
use super::chat_view::{Action, MessageRow, render_messages, scroll_to_bottom};
use super::common::nothing_selected;
use crate::components::{EmptyState, Spinner};
use crate::compose_chat::{Composer, DraftSpec, Editing, Outgoing};
use crate::icons::IconName;
use crate::state::{Area, ListNav, QueryState, app_state, use_query};

/// Store tables the Teams queries depend on.
pub const TEAMS_TABLES: &[&str] = &["team", "channel", "channel_message", "draft"];

/// A channel (`teams.list` entry).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ChannelRow {
    /// Channel id.
    pub id: String,
    /// Team id.
    #[serde(default)]
    pub team: String,
    /// Name.
    #[serde(default)]
    pub name: String,
    /// `standard`, `private`, `shared`.
    #[serde(default)]
    pub membership: Option<String>,
    /// The General channel.
    #[serde(default)]
    pub general: bool,
}

/// A team (`teams.list`).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TeamRow {
    /// Team id.
    pub id: String,
    /// Name.
    #[serde(default)]
    pub name: String,
    /// Archived.
    #[serde(default)]
    pub archived: bool,
    /// Channels.
    #[serde(default)]
    pub channels: Vec<ChannelRow>,
}

/// A channel post with its latest replies (`channel.posts` item).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PostRow {
    /// The post itself.
    #[serde(flatten)]
    pub message: MessageRow,
    /// Total replies.
    #[serde(default)]
    pub reply_count: u32,
    /// Latest replies, oldest first.
    #[serde(default)]
    pub replies: Vec<MessageRow>,
}

/// `channel.posts`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
pub struct PostsPage {
    /// Posts, oldest first.
    #[serde(default)]
    pub items: Vec<PostRow>,
    /// Synced at least once.
    #[serde(default)]
    pub synced: bool,
}

/// Splits a nav key `team/channel`.
pub fn split_nav(nav: &str) -> Option<(String, String)> {
    let (t, c) = nav.split_once('/')?;
    (!t.is_empty() && !c.is_empty()).then(|| (t.to_owned(), c.to_owned()))
}

/// Index of the post after/before `current` for j/k (first post when nothing is selected).
pub fn step(ids: &[String], current: Option<&str>, nav: ListNav) -> Option<usize> {
    if ids.is_empty() {
        return None;
    }
    let last = ids.len() - 1;
    let Some(i) = current.and_then(|c| ids.iter().position(|x| x == c)) else {
        return Some(match nav {
            ListNav::Next => 0,
            ListNav::Previous => last,
        });
    };
    Some(match nav {
        ListNav::Next => (i + 1).min(last),
        ListNav::Previous => i.saturating_sub(1),
    })
}

/// A small coloured tile with the team's initials (drawn, no brand assets).
fn team_tile(name: &str) -> AnyView {
    let initials: String =
        name.split_whitespace().filter_map(|w| w.chars().next()).take(2).collect::<String>().to_uppercase();
    let colour = name.bytes().fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(u32::from(b))) % 8;
    view! {
        <span class="team-tile" style=format!("background: var(--avatar-{colour})") aria-hidden="true">
            {initials}
        </span>
    }
    .into_any()
}

/// The Teams area.
#[component]
pub fn TeamsArea() -> impl IntoView {
    let state = app_state();
    let sel = state.selection(Area::Teams);
    let teams = use_query::<Vec<TeamRow>>("teams.list", || Value::Null, TEAMS_TABLES);
    let collapsed = RwSignal::new(HashSet::<String>::new());

    // Default to the first channel; opening a channel tells the engine (fast polling).
    Effect::new(move |_| {
        if sel.nav.with(|n| n.as_deref().and_then(split_nav).is_none())
            && let Some(first) = teams
                .data()
                .and_then(|t| t.iter().find_map(|t| t.channels.first().map(|c| format!("{}/{}", t.id, c.id))))
        {
            sel.nav.set(Some(first));
        }
    });
    let opened = StoredValue::new(None::<String>);
    Effect::new(move |_| {
        let nav = sel.nav.get();
        if nav == opened.get_value() {
            return;
        }
        let had = opened.get_value().is_some();
        opened.set_value(nav.clone());
        let Some((team, channel)) = nav.as_deref().and_then(split_nav) else { return };
        // A different channel: drop the post selection unless a request named one.
        let requested =
            state.open_request.get_untracked().and_then(|r| r.get("post").and_then(Value::as_str).map(str::to_owned));
        if had && requested.is_none() {
            sel.item.set(None);
        }
        spawn_local(async move {
            let _ = crate::ipc::run("channel.open", json!({ "team": team, "channel": channel })).await;
        });
    });
    Effect::new(move |_| {
        let nav = sel.nav.get();
        super::people_ui::report("teams", nav.into_iter().chain(sel.item.get()).collect(), "channel");
    });

    let nav = move || {
        move || match teams.state.with(|s| match s {
            QueryState::Loading => 0u8,
            QueryState::Failed { .. } => 1,
            QueryState::Ready(_) => 2,
        }) {
            0 => view! { <div class="list-loading"><Spinner /></div> }.into_any(),
            1 => view! { <EmptyState title="Couldn't load your teams" error=true /> }.into_any(),
            _ => {
                let list = teams.data().map(|t| t.as_ref().clone()).unwrap_or_default();
                if list.is_empty() {
                    return view! { <EmptyState title="No teams yet" icon=IconName::Teams /> }.into_any();
                }
                view! {
                    <div class="list-pane">
                        <header class="chat-list-header"><h2 class="list-title">"Teams"</h2></header>
                        <nav class="teams-tree" aria-label="Teams and channels">
                            {list
                                .into_iter()
                                .filter(|t| !t.archived)
                                .map(|t| {
                                    let tid = t.id.clone();
                                    let tid2 = t.id.clone();
                                    let is_open = move || !collapsed.with(|c| c.contains(&tid));
                                    let open2 = is_open.clone();
                                    view! {
                                        <div class="team-group" class:collapsed=move || !is_open()>
                                            <button
                                                class="team-head"
                                                type="button"
                                                aria-expanded=move || if open2() { "true" } else { "false" }
                                                on:click=move |_| {
                                                    collapsed.update(|c| {
                                                        if !c.remove(&tid2) {
                                                            c.insert(tid2.clone());
                                                        }
                                                    })
                                                }
                                            >
                                                <CIcon name=ChatIcon::ChevronDown />
                                                {team_tile(&t.name)}
                                                <span>{t.name.clone()}</span>
                                            </button>
                                            <Show when=is_open.clone()>
                                                {t
                                                    .channels
                                                    .iter()
                                                    .map(|c| {
                                                        let key = format!("{}/{}", t.id, c.id);
                                                        let k2 = key.clone();
                                                        let private = c.membership.as_deref() == Some("private");
                                                        view! {
                                                            <button
                                                                class="channel-row"
                                                                type="button"
                                                                class:selected=move || sel.nav.with(|n| n.as_deref() == Some(key.as_str()))
                                                                aria-current=move || sel.nav.with(|n| n.as_deref() == Some(k2.as_str())).then_some("page")
                                                                on:click={
                                                                    let key = format!("{}/{}", t.id, c.id);
                                                                    move |_| sel.nav.set(Some(key.clone()))
                                                                }
                                                            >
                                                                <span>{c.name.clone()}</span>
                                                                {private.then(|| view! { <span class="item-meta" title="Private channel">"🔒"</span> })}
                                                            </button>
                                                        }
                                                    })
                                                    .collect_view()}
                                            </Show>
                                        </div>
                                    }
                                })
                                .collect_view()}
                        </nav>
                    </div>
                }
                .into_any()
            }
        }
    };

    let channel_name = Signal::derive(move || {
        let (team, channel) = sel.nav.get().as_deref().and_then(split_nav)?;
        let data = teams.data()?;
        let t = data.iter().find(|t| t.id == team)?;
        let c = t.channels.iter().find(|c| c.id == channel)?;
        Some((t.name.clone(), c.name.clone()))
    });

    let list = move || {
        move || match sel.nav.get().as_deref().and_then(split_nav) {
            None => nothing_selected("Select a channel", IconName::Teams),
            Some((team, channel)) => {
                view! { <ChannelFeed team=team channel=channel title=channel_name selected=sel.item /> }.into_any()
            }
        }
    };

    let detail = move || {
        move || match (sel.nav.get().as_deref().and_then(split_nav), sel.item.get()) {
            (Some((team, channel)), Some(post)) => {
                view! { <Thread team=team channel=channel post=post selected=sel.item /> }.into_any()
            }
            _ => nothing_selected("Select a post to see its replies", IconName::Reply),
        }
    };

    view! {
        <super::common::AreaLayout
            id="teams"
            nav_label="Teams"
            nav=nav
            list_label="Posts"
            list=list
            detail_label="Thread"
            detail=detail
        />
    }
}

fn reaction_action(team: &str, channel: &str) -> impl Fn(Action) + Clone + 'static {
    let (team, channel) = (team.to_owned(), channel.to_owned());
    move |a: Action| match a {
        Action::React { id, emoji, remove } => {
            run_cmd(
                "channel.react",
                json!({ "team": team, "channel": channel, "message": id, "reaction": emoji, "remove": remove }),
            );
        }
        Action::Delete(id) => run_cmd("channel.delete", json!({ "team": team, "channel": channel, "message": id })),
        Action::OpenFile(url) => super::chat_view::open_url(url),
        Action::Reply(_) | Action::Edit(_) => {}
    }
}

/// The posts of one channel.
#[component]
fn ChannelFeed(
    team: String,
    channel: String,
    title: Signal<Option<(String, String)>>,
    selected: RwSignal<Option<String>>,
) -> impl IntoView {
    let state = app_state();
    let (qt, qc) = (team.clone(), channel.clone());
    let posts = use_query::<PostsPage>(
        "channel.posts",
        move || json!({ "team": qt, "channel": qc, "limit": 50 }),
        TEAMS_TABLES,
    );
    let scroller = NodeRef::<html::Div>::new();
    let first = StoredValue::new(true);
    let count = StoredValue::new(0usize);
    let composing = RwSignal::new(false);
    super::chat::area_keys(Area::Teams, move |e| {
        if super::chat::is_new_shortcut(e) {
            composing.set(true);
            request_animation_frame(|| {
                let _ = crate::dom::focus_selector(".channel-feed .composer-subject");
            });
            return true;
        }
        false
    });

    Effect::new(move |_| {
        let Some(p) = posts.data() else { return };
        let n = p.items.len();
        let grew = n > count.get_value();
        count.set_value(n);
        if first.get_value() || grew {
            let smooth = !first.get_value();
            first.set_value(false);
            request_animation_frame(move || scroll_to_bottom(scroller, smooth));
        }
    });

    // j/k move between posts and keep the selected one in view.
    Effect::new(move |prev: Option<u64>| {
        let (n, req) = state.list_nav.get();
        if prev.is_some_and(|p| p != n)
            && let Some(req) = req
            && state.area.get_untracked() == Area::Teams
        {
            let ids: Vec<String> =
                posts.data().map(|p| p.items.iter().map(|x| x.message.id.clone()).collect()).unwrap_or_default();
            let cur = selected.get_untracked();
            if let Some(id) = step(&ids, cur.as_deref(), req).and_then(|i| ids.get(i).cloned()) {
                selected.set(Some(id.clone()));
                request_animation_frame(move || {
                    if let Some(el) = crate::dom::document().and_then(|d| {
                        d.query_selector(&format!(".posts [data-post=\"{}\"]", id.replace('"', ""))).ok().flatten()
                    }) {
                        el.scroll_into_view_with_bool(false);
                    }
                });
            }
        }
        n
    });

    let (rt, rc) = (team.clone(), channel.clone());
    let on_action = Callback::new(reaction_action(&rt, &rc));
    let (st, sc) = (team.clone(), channel.clone());
    let on_send = Callback::new(move |out: Outgoing| {
        run_cmd("channel.post", out.into_params(json!({ "team": st.clone(), "channel": sc.clone() })));
        composing.set(false);
    });
    let editing = RwSignal::new(None::<Editing>);
    let draft = DraftSpec {
        query: "channel.draft",
        save: "channel.save_draft",
        params: json!({ "team": team.clone(), "channel": channel.clone() }),
    };

    view! {
        <section class="conversation channel-feed" aria-label="Channel posts">
            <header class="conversation-header">
                {move || title.get().map(|(t, _)| team_tile(&t))}
                <div class="conversation-title">
                    <h1>{move || title.get().map(|(_, c)| c).unwrap_or_default()}</h1>
                    <span class="conversation-subtitle">{move || title.get().map(|(t, _)| t).unwrap_or_default()}</span>
                </div>
            </header>
            <div class="posts" node_ref=scroller role="feed" aria-label="Posts" tabindex="-1">
                {move || match posts.data() {
                    None => view! { <div class="list-loading"><Spinner /></div> }.into_any(),
                    Some(p) if p.items.is_empty() => {
                        let t = if p.synced { "No posts yet. Start a conversation." } else { "Loading posts…" };
                        view! { <EmptyState title=t icon=IconName::Teams /> }.into_any()
                    }
                    Some(p) => p
                        .items
                        .iter()
                        .map(|post| {
                            let pid = post.message.id.clone();
                            let pid2 = pid.clone();
                            let pid3 = pid.clone();
                            let more = post.reply_count.saturating_sub(u32::try_from(post.replies.len()).unwrap_or(0));
                            let count = post.reply_count;
                            view! {
                                <article
                                    class="post-card"
                                    data-post=pid.clone()
                                    class:selected=move || selected.with(|s| s.as_deref() == Some(pid.as_str()))
                                    aria-label=format!("Post by {}", post.message.from_name)
                                >
                                    {render_messages(std::slice::from_ref(&post.message), on_action, None)}
                                    <div class="post-replies">
                                        {(more > 0)
                                            .then(|| view! {
                                                <button class="post-more" type="button" on:click=move |_| selected.set(Some(pid2.clone()))>
                                                    {format!("{more} earlier {}", if more == 1 { "reply" } else { "replies" })}
                                                </button>
                                            })}
                                        {(!post.replies.is_empty()).then(|| render_messages(&post.replies, on_action, None))}
                                        <button class="post-more" type="button" on:click=move |_| {
                                            selected.set(Some(pid3.clone()));
                                            request_animation_frame(|| {
                                                let _ = crate::dom::focus_selector(".thread-pane .composer-editor");
                                            });
                                        }>
                                            <CIcon name=ChatIcon::Thread />
                                            {if count == 0 { " Reply".to_owned() } else { format!(" Reply · {count}") }}
                                        </button>
                                    </div>
                                </article>
                            }
                        })
                        .collect_view()
                        .into_any(),
                }}
            </div>
            {move || {
                if composing.get() {
                    view! {
                        <Composer
                            draft=Signal::stored(Some(draft.clone()))
                            placeholder=Signal::stored("Start a post".to_owned())
                            people=Signal::stored(Vec::new())
                            on_send=on_send
                            editing=editing
                            subject=true
                            label="New post"
                        />
                    }
                    .into_any()
                } else {
                    view! {
                        <div class="composer">
                            <button
                                class="chip"
                                type="button"
                                title="New post (⌘N)"
                                on:click=move |_| {
                                    composing.set(true);
                                    request_animation_frame(|| {
                                        let _ = crate::dom::focus_selector(".channel-feed .composer-subject");
                                    });
                                }
                            >
                                <CIcon name=ChatIcon::Compose />
                                " New post"
                            </button>
                        </div>
                    }
                    .into_any()
                }
            }}
        </section>
    }
    .into_any()
}

/// The thread of one post with a reply box.
#[component]
fn Thread(team: String, channel: String, post: String, selected: RwSignal<Option<String>>) -> impl IntoView {
    let (qt, qc, qp) = (team.clone(), channel.clone(), post.clone());
    let posts = use_query::<PostsPage>(
        "channel.posts",
        {
            let (t, c) = (team.clone(), channel.clone());
            move || json!({ "team": t, "channel": c, "limit": 50 })
        },
        TEAMS_TABLES,
    );
    let replies = use_query::<Vec<MessageRow>>(
        "channel.replies",
        move || json!({ "team": qt, "channel": qc, "post": qp }),
        TEAMS_TABLES,
    );
    let scroller = NodeRef::<html::Div>::new();
    let pid = post.clone();
    let root = Signal::derive(move || {
        posts.data().and_then(|p| p.items.iter().find(|x| x.message.id == pid).map(|x| x.message.clone()))
    });
    Effect::new(move |_| {
        if replies.data().is_some() {
            request_animation_frame(move || scroll_to_bottom(scroller, true));
        }
    });
    let editing = RwSignal::new(None::<Editing>);
    let on_action = Callback::new(reaction_action(&team, &channel));
    let (st, sc, sp) = (team.clone(), channel.clone(), post.clone());
    let on_send = Callback::new(move |out: Outgoing| {
        run_cmd(
            "channel.reply",
            out.into_params(json!({ "team": st.clone(), "channel": sc.clone(), "post": sp.clone() })),
        );
    });
    let draft = DraftSpec {
        query: "channel.draft",
        save: "channel.save_draft",
        params: json!({ "team": team.clone(), "channel": channel.clone(), "post": post.clone() }),
    };
    view! {
        <section class="conversation thread-pane" aria-label="Thread">
            <header class="conversation-header">
                <div class="conversation-title">
                    <h1>{move || root.get().and_then(|r| r.subject).unwrap_or_else(|| "Thread".to_owned())}</h1>
                    <span class="conversation-subtitle">{move || root.get().map(|r| r.from_name).unwrap_or_default()}</span>
                </div>
                <span class="composer-spacer"></span>
                <button class="icon-button" type="button" aria-label="Close thread" title="Close (Esc)" on:click=move |_| selected.set(None)>
                    "×"
                </button>
            </header>
            <div class="messages" node_ref=scroller role="log" aria-label="Replies" tabindex="-1">
                {move || root.get().map(|r| render_messages(std::slice::from_ref(&r), on_action, None))}
                <div class="day-separator">
                    {move || match replies.data() {
                        Some(r) if r.is_empty() => "No replies yet".to_owned(),
                        Some(r) => format!("{} {}", r.len(), if r.len() == 1 { "reply" } else { "replies" }),
                        None => "Loading replies…".to_owned(),
                    }}
                </div>
                {move || replies.data().map(|r| render_messages(&r, on_action, None))}
            </div>
            <Composer
                draft=Signal::stored(Some(draft))
                placeholder=Signal::stored("Reply".to_owned())
                people=Signal::stored(Vec::new())
                on_send=on_send
                editing=editing
                label="Reply"
            />
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nav_keys_split() {
        assert_eq!(split_nav("t/c"), Some(("t".into(), "c".into())));
        assert_eq!(split_nav("t/"), None);
        assert_eq!(split_nav("nothing"), None);
    }

    #[test]
    fn j_and_k_step_through_posts() {
        let ids: Vec<String> = ["a", "b", "c"].iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(step(&ids, None, ListNav::Next), Some(0));
        assert_eq!(step(&ids, None, ListNav::Previous), Some(2));
        assert_eq!(step(&ids, Some("b"), ListNav::Next), Some(2));
        assert_eq!(step(&ids, Some("c"), ListNav::Next), Some(2));
        assert_eq!(step(&ids, Some("a"), ListNav::Previous), Some(0));
        assert_eq!(step(&ids, Some("zz"), ListNav::Next), Some(0));
        assert_eq!(step(&[], Some("a"), ListNav::Next), None);
    }

    #[test]
    fn posts_parse_with_flattened_message() {
        let v = json!({ "items": [{ "id": "1", "from_name": "Morgan", "html": "<p>x</p>", "reply_count": 4, "replies": [{ "id": "2" }] }], "synced": true });
        let p: Result<PostsPage, _> = serde_json::from_value(v);
        let p = p.unwrap_or_default();
        assert_eq!(p.items.len(), 1);
        assert_eq!(p.items.first().map(|x| x.reply_count), Some(4));
    }
}
