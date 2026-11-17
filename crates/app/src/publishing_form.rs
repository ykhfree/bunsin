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
use bunsin_core::inbound::{ArdListing, InboundConfig, InboundScope};
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
    listed: bool,
    title: Entity<InputState>,
    description: Entity<InputState>,
    publisher: Entity<InputState>,
    namespace: Entity<InputState>,
    agent: Entity<InputState>,
    queries: Entity<TextareaState>,
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
            listed: false,
            title: input("Optional, for example Kim's agent", window, cx),
            description: input("For example: questions about the deploy schedule", window, cx),
            publisher: input("For example acme.com", window, cx),
            namespace: input("Optional, for example team", window, cx),
            agent: input("For example kim", window, cx),
            queries: cx.new(|cx| TextareaState::new(window, cx).auto_grow(2, 5)),
        }
    }

    /// The settings that the fields describe.
    fn value(&self, cx: &App) -> InboundConfig {
        let queries = self.queries.read(cx).value().to_string();
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
            }),
        }
    }

    fn fill(&mut self, config: &InboundConfig, window: &mut Window, cx: &mut App) {
        self.enabled = config.enabled;
        self.public = config.scope == InboundScope::Public;
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
        let inbound = form.value(cx);
        let Some((profile, expected)) = form.track.save_target(anyway) else {
            return;
        };
        self.send(UiCommand::SaveInbound {
            profile,
            update: InboundUpdate { inbound, expected },
        });
    }

    fn load_inbound(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(server) = self.state.inbound.clone() {
            self.publishing.inbound.track.load(&server);
            self.publishing.inbound.fill(&server, window, cx);
        }
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
        let skip_authors = text(&form.skip, cx)
            .split(',')
            .map(str::trim)
            .filter(|author| !author.is_empty())
            .map(str::to_owned)
            .collect();
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
            let lines = format!(
                "{} listener\nMCP: {}\nServer card: {}\nARD catalog: {}",
                scope_text(endpoint.listener),
                endpoint.mcp_url,
                endpoint.card_url,
                endpoint.catalog_url
            );
            code_block(cx).child(labelled(format!("inbound-endpoint-{i}"), lines))
        });
        let ard = status.ard.as_ref().map(|ard| {
            let listed = match ard.listed_on.as_slice() {
                [] => "ARD listing: not in any catalog".to_owned(),
                on => {
                    let words: Vec<&str> = on.iter().map(|scope| scope_text(*scope)).collect();
                    format!("ARD listing: in the catalog of the {} listener", words.join(" and "))
                }
            };
            let identifier = ard.identifier.clone().unwrap_or_else(|| "no identifier".to_owned());
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(labelled("ard-listed", listed))
                .child(labelled_as("ard-identifier", identifier.clone(), quiet(identifier, cx)))
                .children(ard.refused.iter().enumerate().map(|(i, why)| {
                    div()
                        .text_sm()
                        .text_color(palette(cx).stopped)
                        .child(labelled(format!("ard-refused-{i}"), format!("Left out: {why}")))
                }))
                .children(ard.warnings.iter().enumerate().map(|(i, warning)| {
                    div().text_sm().text_color(palette(cx).waiting).child(labelled(
                        format!("ard-warning-{i}"),
                        format!("Listed with a warning: {warning}"),
                    ))
                }))
        });
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(labelled("inbound-state", state))
            .children(addresses)
            .children(ard)
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
                    .child(field(
                        "Questions it answers well, one per line (2 to 5)",
                        div()
                            .id("ard-queries")
                            .child(Textarea::new(&form.queries).aria_label("Representative queries")),
                        cx,
                    ))
            }))
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
