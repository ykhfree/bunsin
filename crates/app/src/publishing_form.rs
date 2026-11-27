//! The publishing cards of the agent view: the inbound endpoint with its ARD listing, the ARD
//! registries that the agent searches, and the webhooks that Slack or Jira Cloud send events to.
//!
//! Each card saves its own part of the settings with the conflict check. A typed signing secret
//! goes from its masked field into one command and nowhere else; no label holds it.

use crate::bridge::{Part, SaveError, Typed, UiCommand, UiState};
use crate::part_form::PartTrack;
use crate::shell::{CONFLICT, Shell, labelled, labelled_as};
use crate::theme::{card, code_block, field, palette, quiet};
use bunsin_api::{ArdRegistriesUpdate, InboundStatus, InboundUpdate, WebhooksUpdate};
use bunsin_core::ard::ArdRegistry;
use bunsin_core::inbound::{ArdListing, InboundConfig, InboundScope, InboundSkill};
use bunsin_core::webhook::WebhookConfig;
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::{
    App, AppContext, Context, Div, Entity, FontWeight, ParentElement, SharedString, Styled, Window, div, prelude::*, px,
};
use std::collections::BTreeMap;

const INBOUND_HELP: &str = "Other people's agents ask your agent questions here, with a caller token. \
    Every answer waits for your approval. The ARD listing puts the endpoint into the catalog of each listener \
    that serves it, under a publisher domain that the server operator allows.";
const REGISTRIES_HELP: &str = "The agent may search these ARD registries for agents and tools. \
    The results are data written by others. The agent cannot call what it finds; you add it first.";
const WEBHOOKS_HELP: &str = "Slack or Jira Cloud send events to a webhook of a REST API connector. \
    Give the service the address below and store its signing secret here. Anyone who can write in the \
    service can steer such an event, so its changes always wait for you.";
const DNS_HELP: &str = "Optional: publish this DNS text in your zone, so that crawlers find the catalog. \
    A line that starts with ; says why a listener has no record.";
const SKILLS_HELP: &str = "The Agent Card lists these skills, in this order, so that callers know what to ask for. \
    They only describe the endpoint: every request takes the same path, and every answer and every write \
    waits for your approval.";
const SECRET_HELP: &str = "The server keeps the signing secret encrypted and never shows it again.";

/// The scope word that the window shows.
fn scope_text(scope: InboundScope) -> &'static str {
    match scope {
        InboundScope::Internal => "internal",
        InboundScope::Public => "public",
    }
}

/// The text of a field.
fn text(input: &Entity<InputState>, cx: &App) -> String {
    input.read(cx).value().to_string()
}

/// A text that may be empty, as an optional setting.
fn optional(text: String) -> Option<String> {
    (!text.is_empty()).then_some(text)
}

fn set(input: &Entity<InputState>, value: &str, window: &mut Window, cx: &mut App) {
    input.update(cx, |input, cx| input.set_value(value.to_owned(), window, cx));
}

/// The fields of the inbound card. The fields keep the owner's text as it is, so that settings
/// written by the window load back unchanged.
pub struct InboundForm {
    track: PartTrack<InboundConfig>,
    enabled: bool,
    public: bool,
    a2a: bool,
    listed: bool,
    title: Entity<InputState>,
    description: Entity<InputState>,
    publisher: Entity<InputState>,
    namespace: Entity<InputState>,
    agent: Entity<InputState>,
    queries: Entity<TextareaState>,
    /// The tags, separated by commas.
    tags: Entity<InputState>,
    /// The skills of the Agent Card, edited with Add, Edit and Remove.
    skills: Vec<InboundSkill>,
    /// The skill fields: `None` while closed, `Some(None)` for a new skill, `Some(Some(i))` to change skill `i`.
    skill_edit: Option<Option<usize>>,
    skill_id: Entity<InputState>,
    skill_name: Entity<InputState>,
    skill_description: Entity<TextareaState>,
    /// The tags of the skill, separated by commas.
    skill_tags: Entity<InputState>,
    /// The examples of the skill, one per line.
    skill_examples: Entity<TextareaState>,
}

/// The items of a list typed with `separator`, without the space around them and without empty items.
fn items(text: &str, separator: char) -> Vec<String> {
    text.split(separator)
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

impl InboundForm {
    fn new(window: &mut Window, cx: &mut App) -> Self {
        let input = |placeholder: &'static str, window: &mut Window, cx: &mut App| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        Self {
            track: PartTrack::new(Part::Inbound),
            enabled: false,
            public: false,
            a2a: false,
            listed: false,
            title: input("Optional, for example Kim's agent", window, cx),
            description: input("For example: questions about the deploy schedule", window, cx),
            publisher: input("For example acme.com", window, cx),
            namespace: input("Optional, for example team", window, cx),
            agent: input("For example kim", window, cx),
            queries: cx.new(|cx| TextareaState::new(window, cx).auto_grow(2, 5)),
            tags: input("Optional, for example payments, refunds", window, cx),
            skills: Vec::new(),
            skill_edit: None,
            skill_id: input("For example file-ticket", window, cx),
            skill_name: input("For example File a ticket", window, cx),
            skill_description: cx.new(|cx| TextareaState::new(window, cx).auto_grow(2, 4)),
            skill_tags: input("At least one, for example jira, tickets", window, cx),
            skill_examples: cx.new(|cx| TextareaState::new(window, cx).auto_grow(2, 4)),
        }
    }

    /// The settings that the fields describe. The card does not show the trust manifest, so the
    /// listing keeps the one that the card loaded.
    fn value(&self, cx: &App) -> InboundConfig {
        let queries = self.queries.read(cx).value().to_string();
        let tags = items(&text(&self.tags, cx), ',');
        let base = self.track.base.as_ref();
        InboundConfig {
            enabled: self.enabled,
            scope: if self.public {
                InboundScope::Public
            } else {
                InboundScope::Internal
            },
            title: optional(text(&self.title, cx)),
            description: text(&self.description, cx),
            ard: self.listed.then(|| ArdListing {
                publisher: text(&self.publisher, cx),
                namespace: optional(text(&self.namespace, cx)),
                agent_name: text(&self.agent, cx),
                representative_queries: queries.lines().filter(|q| !q.is_empty()).map(str::to_owned).collect(),
                tags,
                trust_manifest: base
                    .and_then(|base| base.ard.as_ref())
                    .and_then(|ard| ard.trust_manifest.clone()),
            }),
            a2a: self.a2a,
            skills: self.skills.clone(),
        }
    }

    /// Opens the skill fields: empty for a new skill, or with skill `index` to change it.
    fn open_skill(&mut self, index: Option<usize>, window: &mut Window, cx: &mut App) {
        let skill = index.and_then(|i| self.skills.get(i)).cloned().unwrap_or_default();
        self.skill_edit = Some(index);
        set(&self.skill_id, &skill.id, window, cx);
        set(&self.skill_name, &skill.name, window, cx);
        set(&self.skill_tags, &skill.tags.join(", "), window, cx);
        let (description, examples) = (skill.description, skill.examples.join("\n"));
        self.skill_description
            .update(cx, |area, cx| area.set_value(description, window, cx));
        self.skill_examples
            .update(cx, |area, cx| area.set_value(examples, window, cx));
    }

    fn close_skill(&mut self, window: &mut Window, cx: &mut App) {
        self.open_skill(None, window, cx);
        self.skill_edit = None;
    }

    /// The skill that the open fields describe, or why it cannot be kept. The server checks the limits on save.
    fn typed_skill(&self, cx: &App) -> Result<InboundSkill, &'static str> {
        let skill = InboundSkill {
            id: text(&self.skill_id, cx).trim().to_owned(),
            name: text(&self.skill_name, cx).trim().to_owned(),
            description: self.skill_description.read(cx).value().trim().to_owned(),
            tags: items(&text(&self.skill_tags, cx), ','),
            examples: items(&self.skill_examples.read(cx).value(), '\n'),
        };
        if skill.id.is_empty() || skill.name.is_empty() || skill.description.is_empty() || skill.tags.is_empty() {
            return Err("Enter an ID, a name, a description and at least one tag.");
        }
        let editing = self.skill_edit.flatten();
        let taken = self
            .skills
            .iter()
            .enumerate()
            .any(|(i, other)| other.id == skill.id && Some(i) != editing);
        if taken {
            return Err("A skill with this ID is in the list.");
        }
        Ok(skill)
    }

    fn fill(&mut self, config: &InboundConfig, window: &mut Window, cx: &mut App) {
        self.enabled = config.enabled;
        self.public = config.scope == InboundScope::Public;
        self.a2a = config.a2a;
        self.listed = config.ard.is_some();
        let ard = config.ard.clone().unwrap_or_default();
        set(&self.title, config.title.as_deref().unwrap_or_default(), window, cx);
        set(&self.description, &config.description, window, cx);
        set(&self.publisher, &ard.publisher, window, cx);
        set(
            &self.namespace,
            ard.namespace.as_deref().unwrap_or_default(),
            window,
            cx,
        );
        set(&self.agent, &ard.agent_name, window, cx);
        let queries = ard.representative_queries.join("\n");
        self.queries.update(cx, |area, cx| area.set_value(queries, window, cx));
        set(&self.tags, &ard.tags.join(", "), window, cx);
        self.skills = config.skills.clone();
        self.close_skill(window, cx);
    }

    fn changed(&self, cx: &App) -> bool {
        self.track.base.as_ref().is_some_and(|base| *base != self.value(cx))
    }
}

/// The registries card: a draft list, edited with Add and Remove.
pub struct RegistriesForm {
    track: PartTrack<Vec<ArdRegistry>>,
    draft: Vec<ArdRegistry>,
    name: Entity<InputState>,
    url: Entity<InputState>,
}

/// The webhooks card: a draft map, edited with Add and Remove, and the field of one signing secret.
pub struct WebhooksForm {
    track: PartTrack<BTreeMap<String, WebhookConfig>>,
    draft: BTreeMap<String, WebhookConfig>,
    name: Entity<InputState>,
    connector: Entity<InputState>,
    skip: Entity<InputState>,
    public: bool,
    /// The webhook whose secret field is open.
    secret_for: Option<String>,
    secret: Entity<InputState>,
}

pub struct PublishingForm {
    inbound: InboundForm,
    registries: RegistriesForm,
    webhooks: WebhooksForm,
}

impl PublishingForm {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        let input = |placeholder: &'static str, window: &mut Window, cx: &mut App| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        Self {
            inbound: InboundForm::new(window, cx),
            registries: RegistriesForm {
                track: PartTrack::new(Part::ArdRegistries),
                draft: Vec::new(),
                name: input("For example corp", window, cx),
                url: input("For example https://registry.example.com/v1", window, cx),
            },
            webhooks: WebhooksForm {
                track: PartTrack::new(Part::Webhooks),
                draft: BTreeMap::new(),
                name: input("For example slack-events", window, cx),
                connector: input("A REST API connector of Slack or Jira", window, cx),
                skip: input("Optional: your own account IDs, with commas", window, cx),
                public: false,
                secret_for: None,
                secret: cx.new(|cx| InputState::new(window, cx).masked(true)),
            },
        }
    }

    /// Takes a new state from the server. A card keeps what the owner changed, and loads the
    /// server's part otherwise.
    pub fn receive(&mut self, state: &UiState, window: &mut Window, cx: &mut App) {
        let inbound = &mut self.inbound;
        let changed = inbound.changed(cx);
        if let Some(server) = inbound.track.receive(state, state.inbound.as_ref(), changed) {
            inbound.fill(&server, window, cx);
        }
        let registries = &mut self.registries;
        let changed = registries.track.base.as_ref() != Some(&registries.draft);
        if let Some(server) = registries.track.receive(state, state.ard_registries.as_ref(), changed) {
            registries.draft = server;
        }
        let webhooks = &mut self.webhooks;
        let changed = webhooks.track.base.as_ref() != Some(&webhooks.draft);
        if let Some(server) = webhooks.track.receive(state, state.webhooks.as_ref(), changed) {
            webhooks.draft = server;
        }
        // A secret field stays open only for a webhook that the server has.
        let known = |name: &String| state.webhooks.as_ref().is_some_and(|hooks| hooks.contains_key(name));
        if state.webhooks.is_some() && !webhooks.secret_for.as_ref().is_some_and(known) {
            webhooks.secret_for = None;
            set(&webhooks.secret, "", window, cx);
        }
        // A stored secret leaves the field.
        if matches!(state.part_result, Some((Part::WebhookSecret, Ok(())))) {
            webhooks.secret_for = None;
            set(&webhooks.secret, "", window, cx);
        }
    }
}

impl Shell {
    fn save_inbound(&mut self, anyway: bool, cx: &mut Context<Self>) {
        let form = &mut self.publishing.inbound;
        form.track.message = form.skill_edit.is_some().then_some("Keep the skill or cancel it first");
        if form.track.message.is_some() {
            return;
        }
        let inbound = form.value(cx);
        let Some((profile, expected)) = form.track.save_target(anyway) else {
            return;
        };
        self.send(UiCommand::SaveInbound {
            profile,
            update: Box::new(InboundUpdate { inbound, expected }),
        });
    }

    fn load_inbound(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(server) = self.state.inbound.clone() {
            self.publishing.inbound.track.load(&server);
            self.publishing.inbound.fill(&server, window, cx);
        }
    }

    /// Puts the typed skill into the list: at the end, or in the place of the skill it changes.
    fn keep_skill(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let form = &mut self.publishing.inbound;
        let skill = match form.typed_skill(cx) {
            Ok(skill) => skill,
            Err(why) => {
                form.track.message = Some(why);
                return;
            }
        };
        match form.skill_edit.flatten() {
            Some(i) => form.skills[i] = skill,
            None => form.skills.push(skill),
        }
        form.track.message = None;
        form.close_skill(window, cx);
    }

    /// The skills of the Agent Card: one line for each, and the fields of the one that is open.
    fn skills_section(&self, cx: &Context<Self>) -> Div {
        let form = &self.publishing.inbound;
        let count = match form.skills.len() {
            0 => "Skills of the Agent Card: none, so the card has the built-in skill ask".to_owned(),
            n => format!("Skills of the Agent Card: {n}"),
        };
        let rows = form.skills.iter().enumerate().map(|(i, skill)| {
            let line = format!("{} · {} · {}", skill.id, skill.name, skill.tags.join(", "));
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(div().flex_1().min_w(px(0.)).child(labelled(format!("skill-{i}"), line)))
                .child(
                    Button::new(SharedString::from(format!("edit-skill-{i}")))
                        .small()
                        .ghost()
                        .label("Edit")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.publishing.inbound.open_skill(Some(i), window, cx);
                            cx.notify();
                        })),
                )
                .child(
                    Button::new(SharedString::from(format!("remove-skill-{i}")))
                        .small()
                        .ghost()
                        .label("Remove")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let form = &mut this.publishing.inbound;
                            form.skills.remove(i);
                            // The open fields may belong to a skill that moved or went.
                            if form.skill_edit.is_some_and(|edit| edit.is_some()) {
                                form.close_skill(window, cx);
                            }
                            cx.notify();
                        })),
                )
        });
        let input = |name: &'static str, id: &'static str, state: &Entity<InputState>| {
            field(name, Input::new(state).id(id).aria_label(name), cx)
        };
        let area = |name: &'static str, id: &'static str, state: &Entity<TextareaState>| {
            field(name, div().id(id).child(Textarea::new(state).aria_label(name)), cx)
        };
        let fields = form.skill_edit.map(|edit| {
            div()
                .flex()
                .flex_col()
                .gap_3()
                .child(input("Skill ID", "skill-id", &form.skill_id))
                .child(input("Skill name", "skill-name", &form.skill_name))
                .child(area(
                    "What the skill does",
                    "skill-description",
                    &form.skill_description,
                ))
                .child(input("Skill tags, with commas", "skill-tags", &form.skill_tags))
                .child(area(
                    "Example requests, one per line",
                    "skill-examples",
                    &form.skill_examples,
                ))
                .child(
                    div()
                        .flex()
                        .gap_3()
                        .child(
                            Button::new("keep-skill")
                                .outline()
                                .label(if edit.is_some() {
                                    "Change the skill"
                                } else {
                                    "Add the skill"
                                })
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.keep_skill(window, cx);
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("cancel-skill")
                                .ghost()
                                .label("Cancel")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.publishing.inbound.close_skill(window, cx);
                                    cx.notify();
                                })),
                        ),
                )
        });
        let add = form.skill_edit.is_none().then(|| {
            Button::new("add-skill")
                .self_start()
                .outline()
                .label("Add a skill")
                .on_click(cx.listener(|this, _, window, cx| {
                    this.publishing.inbound.open_skill(None, window, cx);
                    cx.notify();
                }))
        });
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(labelled("skill-count", count))
            .child(labelled_as("skills-help", SKILLS_HELP, quiet(SKILLS_HELP, cx)))
            .children(rows)
            .children(add)
            .children(fields)
    }

    fn save_registries(&mut self, anyway: bool, cx: &mut Context<Self>) {
        let form = &mut self.publishing.registries;
        form.track.message = (!text(&form.name, cx).trim().is_empty() || !text(&form.url, cx).trim().is_empty())
            .then_some("Add the registry or clear its fields first");
        if form.track.message.is_some() {
            return;
        }
        let registries = form.draft.clone();
        let Some((profile, expected)) = form.track.save_target(anyway) else {
            return;
        };
        self.send(UiCommand::SaveArdRegistries {
            profile,
            update: ArdRegistriesUpdate { registries, expected },
        });
    }

    fn load_registries(&mut self, _: &mut Window, _: &mut Context<Self>) {
        if let Some(server) = self.state.ard_registries.clone() {
            self.publishing.registries.track.load(&server);
            self.publishing.registries.draft = server;
        }
    }

    fn add_registry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let form = &mut self.publishing.registries;
        let name = text(&form.name, cx).trim().to_owned();
        let url = text(&form.url, cx).trim().to_owned();
        if name.is_empty() || url.is_empty() {
            form.track.message = Some("Enter a name and an address.");
            return;
        }
        form.track.message = None;
        form.draft.push(ArdRegistry { name, url });
        set(&form.name, "", window, cx);
        set(&form.url, "", window, cx);
    }

    fn save_webhooks(&mut self, anyway: bool, cx: &mut Context<Self>) {
        let form = &mut self.publishing.webhooks;
        form.track.message = (!text(&form.name, cx).trim().is_empty() || !text(&form.connector, cx).trim().is_empty())
            .then_some("Add the webhook or clear its fields first");
        if form.track.message.is_some() {
            return;
        }
        let webhooks = form.draft.clone();
        let Some((profile, expected)) = form.track.save_target(anyway) else {
            return;
        };
        self.send(UiCommand::SaveWebhooks {
            profile,
            update: WebhooksUpdate { webhooks, expected },
        });
    }

    fn load_webhooks(&mut self, _: &mut Window, _: &mut Context<Self>) {
        if let Some(server) = self.state.webhooks.clone() {
            self.publishing.webhooks.track.load(&server);
            self.publishing.webhooks.draft = server;
        }
    }

    fn add_webhook(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let form = &mut self.publishing.webhooks;
        let name = text(&form.name, cx).trim().to_owned();
        let connector = text(&form.connector, cx).trim().to_owned();
        if name.is_empty() || connector.is_empty() {
            form.track.message = Some("Enter a name and a connector.");
            return;
        }
        if form.draft.contains_key(&name) {
            form.track.message = Some("A webhook with this name is in the list.");
            return;
        }
        let skip_authors = items(&text(&form.skip, cx), ',');
        let scope = if form.public {
            InboundScope::Public
        } else {
            InboundScope::Internal
        };
        form.track.message = None;
        form.draft.insert(
            name,
            WebhookConfig {
                connector,
                scope,
                skip_authors,
            },
        );
        for input in [&form.name, &form.connector, &form.skip] {
            set(input, "", window, cx);
        }
        form.public = false;
    }

    /// Sends the typed secret of the webhook whose field is open. The field keeps it until the
    /// server stored it.
    fn store_secret(&mut self, cx: &mut Context<Self>) {
        let form = &mut self.publishing.webhooks;
        let (Some(profile), Some(name)) = (self.state.selected.clone(), form.secret_for.clone()) else {
            return;
        };
        let secret = text(&form.secret, cx);
        if secret.trim().is_empty() {
            form.track.message = Some("Enter the signing secret.");
            return;
        }
        form.track.message = None;
        self.send(UiCommand::SetWebhookSecret {
            profile,
            name,
            secret: Typed(secret),
        });
    }

    /// The lines of the endpoint addresses and the ARD listing, from the server's status.
    fn inbound_status_lines(status: &InboundStatus, cx: &Context<Self>) -> Div {
        let state = if status.enabled {
            format!(
                "Endpoint: on, {} listener{}",
                scope_text(status.scope),
                if status.scope == InboundScope::Public {
                    "s too"
                } else {
                    " only"
                }
            )
        } else {
            "Endpoint: off".to_owned()
        };
        let addresses = status.endpoints.iter().enumerate().map(|(i, endpoint)| {
            let mut lines = format!(
                "{} listener\nMCP: {}\nServer card: {}\nARD catalog: {}",
                scope_text(endpoint.listener),
                endpoint.mcp_url,
                endpoint.card_url,
                endpoint.catalog_url
            );
            if let (Some(a2a), Some(card)) = (&endpoint.a2a_url, &endpoint.agent_card_url) {
                lines.push_str(&format!("\nA2A: {a2a}\nAgent card: {card}"));
            }
            code_block(cx).child(labelled(format!("inbound-endpoint-{i}"), lines))
        });
        // One block for each entry. The ids of the first entry have no number.
        let entries = status.ard.iter().enumerate().map(|(n, ard)| {
            let id = |name: &str| if n == 0 { name.to_owned() } else { format!("{name}-{n}") };
            let listed = match ard.listed_on.as_slice() {
                [] => format!("ARD listing ({}): not in any catalog", ard.media_type),
                on => {
                    let words: Vec<&str> = on.iter().map(|scope| scope_text(*scope)).collect();
                    format!(
                        "ARD listing ({}): in the catalog of the {} listener",
                        ard.media_type,
                        words.join(" and ")
                    )
                }
            };
            let identifier = ard.identifier.clone().unwrap_or_else(|| "no identifier".to_owned());
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(labelled(id("ard-listed"), listed))
                .child(labelled_as(
                    id("ard-identifier"),
                    identifier.clone(),
                    quiet(identifier, cx),
                ))
                .children(ard.refused.iter().enumerate().map(|(i, why)| {
                    div().text_sm().text_color(palette(cx).stopped).child(labelled(
                        format!("{}-{i}", id("ard-refused")),
                        format!("Left out: {why}"),
                    ))
                }))
                .children(ard.warnings.iter().enumerate().map(|(i, warning)| {
                    div().text_sm().text_color(palette(cx).waiting).child(labelled(
                        format!("{}-{i}", id("ard-warning")),
                        format!("Listed with a warning: {warning}"),
                    ))
                }))
        });
        let dns = (!status.dns_records.is_empty()).then(|| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(quiet(DNS_HELP, cx))
                .child(code_block(cx).child(labelled("inbound-dns", status.dns_records.join("\n"))))
        });
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(labelled("inbound-state", state))
            .children(addresses)
            .children(entries)
            .children(dns)
    }

    fn inbound_card(&self, cx: &Context<Self>) -> Div {
        let section = card(cx)
            .w(px(520.))
            .flex_none()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Inbound endpoint and ARD listing"),
            )
            .child(labelled_as("inbound-help", INBOUND_HELP, quiet(INBOUND_HELP, cx)));
        let form = &self.publishing.inbound;
        let (Some(status), Some(_)) = (&self.state.inbound_status, &form.track.base) else {
            return section;
        };
        let toggle = |id: &'static str, label: &'static str, on: bool, set: fn(&mut InboundForm, bool)| {
            Checkbox::new(id)
                .label(label)
                .checked(on)
                .on_change(cx.listener(move |this, on: &bool, _, cx| {
                    set(&mut this.publishing.inbound, *on);
                    cx.notify();
                }))
        };
        let input = |name: &'static str, id: &'static str, state: &Entity<InputState>| {
            field(name, Input::new(state).id(id).aria_label(name), cx)
        };
        let changed = form.changed(cx);
        section
            .child(Self::inbound_status_lines(status, cx))
            .child(toggle(
                "inbound-on",
                "Answer questions from other agents",
                form.enabled,
                |f, on| f.enabled = on,
            ))
            .child(toggle(
                "inbound-public",
                "Serve it on the public listener too",
                form.public,
                |f, on| f.public = on,
            ))
            .child(toggle(
                "inbound-a2a",
                "Serve it over A2A too, with an Agent Card",
                form.a2a,
                |f, on| f.a2a = on,
            ))
            .child(input(
                "What callers can ask about",
                "inbound-description",
                &form.description,
            ))
            .child(input("Title of the server card", "inbound-title", &form.title))
            .child(toggle("ard-on", "List it in the ARD catalogs", form.listed, |f, on| {
                f.listed = on
            }))
            .children(form.listed.then(|| {
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(input("Publisher domain", "ard-publisher", &form.publisher))
                    .child(input("Namespace", "ard-namespace", &form.namespace))
                    .child(input("Agent name", "ard-agent", &form.agent))
                    .child(input("Tags, with commas (up to 10)", "ard-tags", &form.tags))
                    .child(field(
                        "Questions it answers well, one per line (2 to 5)",
                        div()
                            .id("ard-queries")
                            .child(Textarea::new(&form.queries).aria_label("Representative queries")),
                        cx,
                    ))
            }))
            .child(self.skills_section(cx))
            .child(self.save_bar("inbound", &form.track, changed, Self::save_inbound, cx))
            .children(form.track.conflict.then(|| {
                Self::conflict_warning(
                    "inbound",
                    CONFLICT,
                    Self::load_inbound,
                    |this, cx| this.save_inbound(true, cx),
                    cx,
                )
            }))
    }

    fn registries_card(&self, cx: &Context<Self>) -> Div {
        let section = card(cx)
            .w(px(420.))
            .flex_none()
            .child(div().font_weight(FontWeight::SEMIBOLD).child("ARD registries"))
            .child(labelled_as(
                "registries-help",
                REGISTRIES_HELP,
                quiet(REGISTRIES_HELP, cx),
            ));
        let form = &self.publishing.registries;
        if form.track.base.is_none() {
            return section;
        }
        let rows = form.draft.iter().enumerate().map(|(i, registry)| {
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(div().flex_1().min_w(px(0.)).child(labelled(
                    format!("registry-{i}"),
                    format!("{} · {}", registry.name, registry.url),
                )))
                .child(
                    Button::new(SharedString::from(format!("remove-registry-{i}")))
                        .small()
                        .ghost()
                        .label("Remove")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.publishing.registries.draft.remove(i);
                            cx.notify();
                        })),
                )
        });
        let changed = form.track.base.as_ref() != Some(&form.draft);
        section
            .child(labelled(
                "registry-count",
                match form.draft.len() {
                    0 => "Registries: none, so the agent has no search".to_owned(),
                    count => format!("Registries: {count}"),
                },
            ))
            .children(rows)
            .child(field(
                "Name the agent uses",
                Input::new(&form.name)
                    .id("new-registry-name")
                    .aria_label("Registry name"),
                cx,
            ))
            .child(field(
                "Address",
                Input::new(&form.url)
                    .id("new-registry-url")
                    .aria_label("Registry address"),
                cx,
            ))
            .child(
                Button::new("add-registry")
                    .self_start()
                    .outline()
                    .label("Add the registry")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.add_registry(window, cx);
                        cx.notify();
                    })),
            )
            .child(self.save_bar("registries", &form.track, changed, Self::save_registries, cx))
            .children(form.track.conflict.then(|| {
                Self::conflict_warning(
                    "registries",
                    CONFLICT,
                    Self::load_registries,
                    |this, cx| this.save_registries(true, cx),
                    cx,
                )
            }))
    }

    /// One webhook of the draft, with what the server knows of it.
    fn webhook_row(&self, name: &str, hook: &WebhookConfig, cx: &Context<Self>) -> Div {
        let form = &self.publishing.webhooks;
        let mut line = format!("{name} → {} · {}", hook.connector, scope_text(hook.scope));
        if !hook.skip_authors.is_empty() {
            line.push_str(&format!(" · skips {}", hook.skip_authors.join(", ")));
        }
        let shown = self
            .state
            .inbound_status
            .as_ref()
            .and_then(|status| status.webhooks.iter().find(|h| h.name == name));
        let state = match shown {
            None => "Not saved yet".to_owned(),
            Some(h) if h.secret_stored => "Signing secret stored".to_owned(),
            Some(_) => "No signing secret: every delivery is refused".to_owned(),
        };
        let urls = shown.map(|h| h.urls.join("\n")).unwrap_or_default();
        let owned = name.to_owned();
        let remove = Button::new(SharedString::from(format!("remove-webhook-{name}")))
            .small()
            .ghost()
            .label("Remove")
            .on_click(cx.listener(move |this, _, _, cx| {
                this.publishing.webhooks.draft.remove(&owned);
                cx.notify();
            }));
        let owned = name.to_owned();
        let secret = shown.is_some().then(|| {
            Button::new(SharedString::from(format!("webhook-secret-{name}")))
                .small()
                .outline()
                .label("Store the signing secret")
                .on_click(cx.listener(move |this, _, window, cx| {
                    let form = &mut this.publishing.webhooks;
                    form.secret_for = Some(owned.clone());
                    set(&form.secret, "", window, cx);
                    cx.notify();
                }))
        });
        // The secret field is drawn, not labelled: accessibility labels must never hold a secret.
        let field_open = form.secret_for.as_deref() == Some(name);
        let secret_field = field_open.then(|| {
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(quiet(SECRET_HELP, cx))
                .child(
                    Input::new(&form.secret)
                        .id("webhook-secret")
                        .aria_label("Signing secret"),
                )
                .child(
                    Button::new("store-webhook-secret")
                        .self_start()
                        .primary()
                        .label("Store")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.store_secret(cx);
                            cx.notify();
                        })),
                )
        });
        div()
            .flex()
            .flex_col()
            .gap_1()
            .py_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .child(labelled(format!("webhook-{name}"), line)),
                    )
                    .children(secret)
                    .child(remove),
            )
            .child(labelled_as(
                format!("webhook-state-{name}"),
                state.clone(),
                quiet(state, cx),
            ))
            .children((!urls.is_empty()).then(|| code_block(cx).child(labelled(format!("webhook-urls-{name}"), urls))))
            .children(secret_field)
    }

    fn webhooks_card(&self, cx: &Context<Self>) -> Div {
        let section = card(cx)
            .w(px(520.))
            .flex_none()
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Webhooks"))
            .child(labelled_as("webhooks-help", WEBHOOKS_HELP, quiet(WEBHOOKS_HELP, cx)));
        let form = &self.publishing.webhooks;
        if form.track.base.is_none() {
            return section;
        }
        let changed = form.track.base.as_ref() != Some(&form.draft);
        let secret_result = match &self.state.part_result {
            Some((Part::WebhookSecret, Ok(()))) => "Signing secret stored".to_owned(),
            Some((Part::WebhookSecret, Err(SaveError::Failed(error)))) => error.clone(),
            _ => String::new(),
        };
        section
            .child(labelled(
                "webhook-count",
                match form.draft.len() {
                    0 => "Webhooks: none".to_owned(),
                    count => format!("Webhooks: {count}"),
                },
            ))
            .children(form.draft.iter().map(|(name, hook)| self.webhook_row(name, hook, cx)))
            .child(div().text_sm().child(labelled("webhook-secret-result", secret_result)))
            .child(field(
                "Name",
                Input::new(&form.name).id("new-webhook-name").aria_label("Webhook name"),
                cx,
            ))
            .child(field(
                "Connector",
                Input::new(&form.connector)
                    .id("new-webhook-connector")
                    .aria_label("Webhook connector"),
                cx,
            ))
            .child(field(
                "Authors to skip",
                Input::new(&form.skip)
                    .id("new-webhook-skip")
                    .aria_label("Authors to skip"),
                cx,
            ))
            .child(
                Checkbox::new("new-webhook-public")
                    .label("Take deliveries on the public listener too (Slack and Jira Cloud need it)")
                    .checked(form.public)
                    .on_change(cx.listener(|this, on: &bool, _, cx| {
                        this.publishing.webhooks.public = *on;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("add-webhook")
                    .self_start()
                    .outline()
                    .label("Add the webhook")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.add_webhook(window, cx);
                        cx.notify();
                    })),
            )
            .child(self.save_bar("webhooks", &form.track, changed, Self::save_webhooks, cx))
            .children(form.track.conflict.then(|| {
                Self::conflict_warning(
                    "webhooks",
                    CONFLICT,
                    Self::load_webhooks,
                    |this, cx| this.save_webhooks(true, cx),
                    cx,
                )
            }))
    }

    /// The publishing cards of the agent view.
    pub(crate) fn publishing_cards(&self, cx: &Context<Self>) -> [Div; 3] {
        [self.inbound_card(cx), self.webhooks_card(cx), self.registries_card(cx)]
    }
}
