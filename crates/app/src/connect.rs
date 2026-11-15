//! The window root: the connect screen, and the shell after a connection.

use crate::bridge::{REFRESH_EVERY, spawn_bridge};
use crate::connection::{Connection, KeychainLookup, NoKeychain, TokenStore, client, load_connection, save_connection};
use crate::shell::{Shell, labelled, labelled_as};
use crate::theme::{card, field, palette, quiet};
use bunsin_api::{ApiError, ApiResult, EngineApi};
use bunsin_client::RemoteEngine;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme, Disableable, Sizable};
use gpui_kit::{AppContext, Context, Entity, FontWeight, IntoElement, ParentElement, Render, Styled, Window, div, px};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::oneshot;

/// Where the connection comes from.
pub enum Source {
    /// The saved connection. Connect saves to it and Disconnect deletes the token.
    Saved { dir: PathBuf, tokens: Arc<dyn TokenStore> },
    /// Given by the environment for development. Nothing is read or saved.
    Fixed { connection: Connection, token: String },
}

pub struct AppView {
    runtime: tokio::runtime::Handle,
    source: Source,
    server: Entity<InputState>,
    token: Entity<InputState>,
    ca_cert: Entity<InputState>,
    error: Option<String>,
    connecting: bool,
    shell: Option<Entity<Shell>>,
    /// Given to the shell, for reading an existing Keychain item. Inert unless the app sets one.
    keychain: Arc<dyn KeychainLookup>,
}

impl AppView {
    /// Fills in the known address. If a token is known too, connects with it.
    pub fn new(runtime: tokio::runtime::Handle, source: Source, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut error = None;
        let (known, token) = match &source {
            Source::Saved { dir, tokens } => match load_connection(dir) {
                // The token store is read only for a saved address.
                Some(connection) => match tokens.get() {
                    Ok(token) => (Some(connection), token),
                    Err(e) => {
                        error = Some(e);
                        (Some(connection), None)
                    }
                },
                None => (None, None),
            },
            Source::Fixed { connection, token } => (Some(connection.clone()), Some(token.clone())),
        };
        let server = known.as_ref().map(|c| c.server.clone()).unwrap_or_default();
        let ca_cert = known
            .as_ref()
            .and_then(|c| c.ca_cert.as_deref())
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let mut view = Self {
            runtime,
            source,
            server: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("https://bunsin.example.com")
                    .default_value(server)
            }),
            token: cx.new(|cx| InputState::new(window, cx).masked(true)),
            ca_cert: cx.new(|cx| InputState::new(window, cx).default_value(ca_cert)),
            error,
            connecting: false,
            shell: None,
            keychain: Arc::new(NoKeychain),
        };
        if let (Some(connection), Some(token)) = (known, token) {
            view.connect(connection, token, window, cx);
        }
        view
    }

    /// Lets the main window read items of the Keychain. Only the real app calls this.
    /// A main window that exists already gets the lookup too, so the order of the calls does not matter.
    pub fn set_keychain(&mut self, keychain: Arc<dyn KeychainLookup>, cx: &mut Context<Self>) {
        if let Some(shell) = &self.shell {
            shell.update(cx, |shell, _| shell.set_keychain(keychain.clone()));
        }
        self.keychain = keychain;
    }

    /// The shell, after a connection.
    pub fn shell(&self) -> Option<&Entity<Shell>> {
        self.shell.as_ref()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn connecting(&self) -> bool {
        self.connecting
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = |input: &Entity<InputState>| input.read(cx).value().trim().to_owned();
        let ca_cert = text(&self.ca_cert);
        let connection = Connection {
            server: text(&self.server),
            ca_cert: (!ca_cert.is_empty()).then(|| PathBuf::from(ca_cert)),
        };
        let mut token = text(&self.token);
        // An empty field means "use the token that is already stored". A token is shown
        // only once, so the user cannot type it again after a start-up that failed.
        if token.is_empty()
            && let Source::Saved { tokens, .. } = &self.source
            && let Ok(Some(stored)) = tokens.get()
        {
            token = stored;
        }
        self.connect(connection, token, window, cx);
    }

    /// Checks the address at once. Then asks the server for the profile list on tokio.
    fn connect(&mut self, connection: Connection, token: String, window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        cx.notify();
        let engine = {
            // The HTTP client is built inside the runtime it will run on.
            let _runtime = self.runtime.enter();
            client(&connection, &token)
        };
        let engine = match engine {
            Ok(engine) => Arc::new(engine),
            Err(error) => {
                self.error = Some(error.to_string());
                return;
            }
        };
        self.connecting = true;
        let (done_tx, done_rx) = oneshot::channel();
        let probe = engine.clone();
        self.runtime.spawn(async move {
            let _ = done_tx.send(probe.list_profiles().await.map(|_| ()));
        });
        // tokio channels work on the GPUI executor. tokio timers do not, so none is used here.
        cx.spawn_in(window, async move |this, cx| {
            let result = done_rx
                .await
                .unwrap_or_else(|_| Err(ApiError::Internal("the connection check stopped".into())));
            let _ = this.update_in(cx, |view, window, cx| {
                view.finish(connection, token, engine, result, window, cx)
            });
        })
        .detach();
    }

    fn finish(
        &mut self,
        connection: Connection,
        token: String,
        engine: Arc<RemoteEngine>,
        result: ApiResult<()>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.connecting = false;
        cx.notify();
        if let Err(error) = result {
            self.error = Some(error.to_string());
            return;
        }
        if let Source::Saved { dir, tokens } = &self.source
            && let Err(error) = save(dir, tokens.as_ref(), &connection, &token)
        {
            self.error = Some(error);
            return;
        }
        // The token now lives in the client and the token store, not in the form.
        self.token.update(cx, |input, cx| input.set_value("", window, cx));
        let (commands, states) = spawn_bridge(&self.runtime, engine, REFRESH_EVERY);
        let keychain = self.keychain.clone();
        self.shell = Some(cx.new(|cx| {
            let mut shell = Shell::new(states, commands, window, cx);
            shell.set_keychain(keychain);
            shell
        }));
    }

    /// Ends the session. The shell holds the bridge's command sender.
    /// The bridge task stops when GPUI releases the shell and the channel closes.
    fn disconnect(&mut self, cx: &mut Context<Self>) {
        self.shell = None;
        self.error = None;
        if let Source::Saved { tokens, .. } = &self.source
            && let Err(error) = tokens.delete()
        {
            self.error = Some(error);
        }
        cx.notify();
    }
}

fn save(dir: &Path, tokens: &dyn TokenStore, connection: &Connection, token: &str) -> Result<(), String> {
    tokens.set(token)?;
    save_connection(dir, connection).map_err(|e| format!("connected, but the connection cannot be saved: {e}"))
}

impl Render for AppView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        if let Some(shell) = &self.shell {
            // Disconnect sits at the foot of the shell's left column, under the state of the connection.
            return div()
                .size_full()
                .relative()
                .child(shell.clone())
                .child(
                    div().absolute().bottom_3().left_3().child(
                        Button::new("disconnect")
                            .small()
                            .ghost()
                            .label("Disconnect")
                            .on_click(cx.listener(|this, _, _, cx| this.disconnect(cx))),
                    ),
                )
                .into_any_element();
        }

        let error = self.error.clone().unwrap_or_default();
        let connecting = if self.connecting { "Connecting…" } else { "" };
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme.background)
            .text_color(theme.foreground)
            .text_size(theme.font_size)
            .child(
                card(cx)
                    .w(px(440.))
                    .p_6()
                    .gap_4()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child("Bunsin"))
                            .child(labelled_as(
                                "connect-title",
                                "Connect to a Bunsin server",
                                quiet("Connect to the server that runs your agent.", cx),
                            )),
                    )
                    .child(field(
                        "Server address",
                        Input::new(&self.server).id("server").aria_label("Server address"),
                        cx,
                    ))
                    .child(field(
                        "Access token, from the operator of the server",
                        // Masked, with no toggle that would show the token.
                        Input::new(&self.token).id("token").aria_label("Access token"),
                        cx,
                    ))
                    .child(field(
                        "CA certificate file, only for a private CA",
                        Input::new(&self.ca_cert)
                            .id("ca-cert")
                            .aria_label("CA certificate file"),
                        cx,
                    ))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                Button::new("connect")
                                    .primary()
                                    .label("Connect")
                                    .disabled(self.connecting)
                                    .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                            )
                            .child(labelled_as("connect-status", connecting, quiet(connecting, cx))),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(palette(cx).stopped)
                            .child(labelled("connect-error", error)),
                    ),
            )
            .into_any_element()
    }
}
