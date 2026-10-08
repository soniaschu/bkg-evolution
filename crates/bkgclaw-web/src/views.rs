//! The cockpit views: one signal, four panels, zero hidden state.

use crate::net;
use crate::{Cockpit, Panel, TranscriptItem, WireClient};
use dioxus::prelude::*;
use serde_json::json;

#[component]
pub fn App() -> Element {
    let mut cockpit = use_signal(|| Cockpit {
        base: net::resolve_base(),
        policy: "allow-read-only".into(),
        ..Default::default()
    });

    // Boot: socket, sessions, a fresh session to talk to. One shot, not a
    // render effect — the cockpit owns exactly one of each.
    use_hook(|| {
        let base = cockpit.read().base.clone();
        let mut cockpit_for_socket = cockpit;
        spawn(async move {
            if net::ensure_socket(&base, cockpit_for_socket).is_none() {
                cockpit_for_socket.write().notice = "websocket nicht erreichbar".into();
            }
        });
    });

    let mut first_boot = use_signal(|| false);
    if !first_boot() {
        first_boot.set(true);
        let base = cockpit.read().base.clone();
        spawn(async move {
            // Fetch everything first, write once at the end: a signal's
            // write guard must never live across an await, or the UI
            // stops rendering mid-flight.
            let models = net::fetch_models(&base).await;
            let sessions = net::fetch_sessions(&base).await.unwrap_or_default();
            let session = net::create_session(&base, None, "allow-read-only".to_string()).await.ok();
            let mut state = cockpit.write();
            state.models = models;
            state.sessions = sessions;
            if let Some(session) = session {
                net::send(&WireClient::Subscribe { session: session.clone() });
                state.session = Some(session);
            } else {
                state.notice = "sitzung konnte nicht erstellt werden".into();
            }
        });
    }

    rsx! {
        // The stylesheet is bundled through manganis (`asset!`): dx gives it
        // a hashed name and this link is what actually loads it.
        document::Link { rel: "stylesheet", href: asset!("/assets/style.css") }

        div { class: "cockpit",
            header { class: "top",
                h1 { "bkgclaw" }
                select {
                    class: "model",
                    onchange: move |event| {
                        let model = event.value();
                        cockpit.write().notice = format!("modell {model} gilt ab nächster neuer sitzung");
                        cockpit.write().chosen_model = Some(model);
                    },
                    option { value: "", "failover-kette" }
                    for model in cockpit.read().models.clone().into_iter() {
                        option { value: "{model}" }
                    }
                }
                span { class: "notice", "{cockpit.read().notice}" }
                nav {
                    button {
                        class: if cockpit.read().panel == Panel::Chat { "on" },
                        onclick: move |_| cockpit.write().panel = Panel::Chat,
                        "chat"
                    }
                    button {
                        class: if cockpit.read().panel == Panel::Tasks { "on" },
                        onclick: move |_| {
                            cockpit.write().panel = Panel::Tasks;
                            let base = cockpit.read().base.clone();
                            spawn(async move {
                                let tasks = net::fetch_tasks(&base).await;
                                cockpit.write().tasks = tasks;
                            });
                        },
                        "aufgaben"
                    }
                    button {
                        class: if cockpit.read().panel == Panel::Memory { "on" },
                        onclick: move |_| {
                            cockpit.write().panel = Panel::Memory;
                            let base = cockpit.read().base.clone();
                            spawn(async move {
                                let memory = net::fetch_memory(&base).await;
                                cockpit.write().memory = memory;
                            });
                        },
                        "gedächtnis"
                    }
                    button {
                        class: if cockpit.read().panel == Panel::Skills { "on" },
                        onclick: move |_| {
                            cockpit.write().panel = Panel::Skills;
                            let base = cockpit.read().base.clone();
                            spawn(async move {
                                let skills = net::fetch_skills(&base).await;
                                cockpit.write().skills = skills;
                            });
                        },
                        "skills"
                    }
                }
            }

            div { class: "body",
                aside { class: "side",
                    button {
                        class: "new",
                        onclick: move |_| {
                            let mut state = cockpit.write();
                            state.pending_new = true;
                            net::send(&WireClient::CreateSession {
                                model: state.chosen_model.clone(),
                                policy: Some(state.policy.clone()),
                                fork_from: None,
                                fork_at: None,
                            });
                        },
                        "+ neue sitzung"
                    }
                    for row in cockpit.read().sessions.clone().into_iter() {
                        button {
                            class: "session",
                            onclick: move |_| {
                                let session = row.id.clone();
                                let base = cockpit.read().base.clone();
                                spawn(async move {
                                    let transcript = net::fetch_transcript(&base, &session).await;
                                    let old = cockpit.read().session.clone();
                                    if let Some(old) = old {
                                        net::send(&WireClient::StopReceiving { session: old });
                                    }
                                    net::send(&WireClient::Subscribe { session: session.clone() });
                                    let mut state = cockpit.write();
                                    state.session = Some(session);
                                    state.transcript = transcript;
                                    state.live.clear();
                                    state.live_reasoning.clear();
                                    state.panel = Panel::Chat;
                                });
                            },
                            "{row.id}"
                            small { "{row.preview}" }
                        }
                    }
                }

                main { class: "main",
                    match cockpit.read().panel {
                        Panel::Chat => rsx! { Chat { cockpit } },
                        Panel::Tasks => rsx! { TaskPanel { cockpit } },
                        Panel::Memory => rsx! { MemoryPanel { cockpit } },
                        Panel::Skills => rsx! { SkillPanel { cockpit } },
                    }
                }
            }
        }
    }
}

#[component]
fn ApprovalBar(mut cockpit: Signal<Cockpit>, pending: crate::ApprovalRequest) -> Element {
    // Each button needs its own id: a move-closure owns what it captures,
    // and three closures cannot share one.
    let allow_id = pending.call_id.clone();
    let deny_id = pending.call_id.clone();
    let always_id = pending.call_id.clone();
    rsx! {
        div { class: "approval",
            strong { "freigabe nötig: " }
            span { "{pending.name} ({pending.risk})" }
            code { "{pending.arguments}" }
            div { class: "choices",
                button {
                    onclick: move |_| {
                        net::send(&WireClient::Approval { call_id: allow_id.clone(), decision: "allow".into() });
                        cockpit.write().pending_approval = None;
                    },
                    "erlauben"
                }
                button {
                    onclick: move |_| {
                        net::send(&WireClient::Approval { call_id: deny_id.clone(), decision: "deny".into() });
                        cockpit.write().pending_approval = None;
                    },
                    "ablehnen"
                }
                button {
                    onclick: move |_| {
                        net::send(&WireClient::Approval { call_id: always_id.clone(), decision: "always".into() });
                        cockpit.write().pending_approval = None;
                    },
                    "immer erlauben"
                }
            }
        }
    }
}

/// Send the current input to the active session. Shared by the form and
/// the Enter key so both do exactly the same thing.
fn submit_message(cockpit: &mut Signal<Cockpit>) {
    let trimmed = cockpit.read().input.trim().to_string();
    if trimmed.is_empty() {
        return;
    }
    let session = cockpit.read().session.clone();
    let Some(session) = session else { return };
    cockpit.write().input.clear();
    cockpit.write().transcript.push(TranscriptItem::User(trimmed.clone()));
    net::send(&WireClient::Send { session, message: trimmed });
}

#[component]
fn Chat(mut cockpit: Signal<Cockpit>) -> Element {
    rsx! {
        div { class: "chat",
            if let Some(pending) = cockpit.read().pending_approval.clone() {
                ApprovalBar { cockpit, pending }
            }

            div { class: "transcript",
                for item in cockpit.read().transcript.clone().into_iter() {
                    match item {
                        TranscriptItem::User(text) => rsx! { div { class: "msg user", "{text}" } },
                        TranscriptItem::Assistant(text) => rsx! { div { class: "msg agent", "{text}" } },
                        TranscriptItem::Reasoning(text) => rsx! { div { class: "msg think", "{text}" } },
                        TranscriptItem::Tool { name, outcome, output } => rsx! {
                            div { class: "msg tool {outcome}",
                                "⚙ {name}: "
                                code { "{output}" }
                            }
                        },
                        TranscriptItem::Error(text) => rsx! { div { class: "msg error", "{text}" } },
                    }
                }
                if !cockpit.read().live_reasoning.is_empty() {
                    div { class: "msg think", "{cockpit.read().live_reasoning}" }
                }
                if !cockpit.read().live.is_empty() {
                    div { class: "msg agent live", "{cockpit.read().live}" }
                }
            }

            form {
                class: "input",
                // The native default would navigate away from the app;
                // the SPA stays.
                onsubmit: move |event| {
                    event.prevent_default();
                    submit_message(&mut cockpit);
                },
                textarea {
                    name: "message",
                    rows: "2",
                    placeholder: "nachricht an den agenten — enter sendet, shift+enter neue zeile",
                    value: "{cockpit.read().input}",
                    oninput: move |event| {
                        cockpit.write().input = event.value();
                    },
                    onkeydown: move |event| {
                        // Enter sends; Shift+Enter is the newline, because a
                        // textarea's native Enter would insert a line break
                        // and never submit the form.
                        if event.data().code() == dioxus::prelude::Code::Enter
                            && !event.data().modifiers().shift()
                        {
                            event.prevent_default();
                            submit_message(&mut cockpit);
                        }
                    }
                }
                div { class: "actions",
                    span { class: "cost", "${cockpit.read().spent:.4}" }
                    if cockpit.read().busy {
                        button {
                            class: "cancel",
                            onclick: move |_| {
                                let session = cockpit.read().session.clone().unwrap_or_default();
                                net::send(&WireClient::Cancel { session });
                            },
                            "turn abbrechen"
                        }
                    }
                    button { r#type: "submit", "senden" }
                }
            }
        }
    }
}

#[component]
fn TaskPanel(mut cockpit: Signal<Cockpit>) -> Element {
    rsx! {
        div { class: "panel",
            if cockpit.read().tasks.is_empty() {
                p { "keine aufgaben im workspace" }
            }
            for task in cockpit.read().tasks.clone().into_iter() {
                div { class: "row",
                    span { class: "status {task.status}", "[{task.status}]" }
                    strong { "{task.title}" }
                    small { "({task.slug})" }
                }
            }
            p { class: "hint", "aufgaben sind dateien in .bkgclaw/tasks — der agent pflegt sie mit task_add/task_update" }
        }
    }
}

#[component]
fn MemoryPanel(mut cockpit: Signal<Cockpit>) -> Element {
    rsx! {
        div { class: "panel",
            if cockpit.read().memory.is_empty() {
                p { "keine langzeit-einträge" }
            }
            for entry in cockpit.read().memory.clone().into_iter() {
                div { class: "row",
                    strong { "{entry.key}" }
                    span { "{entry.value}" }
                }
            }
            p { class: "hint", "heiße fakten: .bkgclaw/MEMORY.md — langzeit: memory_set / memory_search" }
        }
    }
}

#[component]
fn SkillPanel(mut cockpit: Signal<Cockpit>) -> Element {
    rsx! {
        div { class: "panel",
            if cockpit.read().skills.is_empty() {
                p { "keine skills installiert" }
            }
            for skill in cockpit.read().skills.clone().into_iter() {
                div { class: "row",
                    strong { "{skill.name}" }
                    span { "{skill.description}" }
                }
            }
            p { class: "hint", "skills liegen in .bkgclaw/skills/<name>/SKILL.md — der agent lädt sie mit skill_read" }
        }
    }
}
