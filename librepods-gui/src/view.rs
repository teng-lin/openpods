// SPDX-License-Identifier: AGPL-3.0-only
use gpui::{
    actions, div, prelude::*, px, rgb, size, App, Application, Bounds, Context, Div, FocusHandle,
    FontWeight, KeyBinding, KeyDownEvent, SharedString, Stateful, Task, Timer, TitlebarOptions,
    Window, WindowBounds, WindowOptions,
};
use librepods_api::config::EarPolicy;
use librepods_core::device::state::NoiseControlMode as Mode;
use librepods_gui::{
    client::{Client, Event},
    demo,
    model::{self, Action, PanelModel},
};
use std::time::{Duration, Instant};

const BG: u32 = 0x141719;
const CARD: u32 = 0x1e2326;
const LINE: u32 = 0x30383c;
const TEXT: u32 = 0xe9eeea;
const MUTED: u32 = 0x97a49f;
const GREEN: u32 = 0xb4e4bf;
actions!(pods, [Next, Previous, Close]);
#[derive(Clone, Copy)]
enum Click {
    Control(Action),
    AskForget,
    CancelForget,
    Dismiss,
}
struct Panel {
    model: PanelModel,
    client: Option<Client>,
    demo: bool,
    started: Instant,
    focus: FocusHandle,
    last_current: bool,
    _tick: Task<()>,
}
impl Panel {
    fn now(&self) -> u64 {
        self.started.elapsed().as_millis().min(u64::MAX as u128) as u64
    }
    fn new(
        demo_mode: bool,
        client: Option<Client>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        window.set_window_title("LibrePods");
        let focus = cx.focus_handle();
        focus.focus(window);
        let mut model = PanelModel::default();
        if demo_mode {
            let s = demo::snapshot();
            let caps = s.capabilities();
            let _ = model.observe(s, caps, 0);
        }
        let tick = cx.spawn(async move |entity, cx| {
            loop {
                Timer::after(Duration::from_millis(250)).await;
                if entity
                    .update(cx, |this, cx| {
                        let mut changed = false;
                        if let Some(client) = &this.client {
                            while let Ok(event) = client.events.try_recv() {
                                changed = true;
                                match event {
                                    Event::Status(Ok(status)) => {
                                        let (s, c, received) = *status;
                                        let age =
                                            received.elapsed().as_millis().min(u64::MAX as u128)
                                                as u64;
                                        let _ = this.model.observe(
                                            s,
                                            c,
                                            this.now().saturating_sub(age),
                                        );
                                        if age > model::STATUS_TTL_MS {
                                            this.model.unavailable(
                                                "Status expired while the window was unresponsive"
                                                    .into(),
                                            );
                                        }
                                    }
                                    Event::Status(Err(e)) => this.model.unavailable(e),
                                    Event::Finished(result) => this.model.finish(result),
                                }
                            }
                        }
                        // Demo observations remain current without any IPC.
                        if this.demo {
                            if let Some(s) = this.model.snapshot.clone() {
                                let c = s.capabilities();
                                let _ = this.model.observe(s, c, this.now());
                            }
                        }
                        let current = this.model.current(this.now());
                        if changed || current != this.last_current {
                            cx.notify();
                        }
                        this.last_current = current;
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            model,
            client,
            demo: demo_mode,
            started: Instant::now(),
            focus,
            last_current: demo_mode,
            _tick: tick,
        }
    }
    fn click(&mut self, click: Click, cx: &mut Context<Self>) {
        let now = self.now();
        match click {
            Click::AskForget => self.model.confirm_forget(now),
            Click::CancelForget => self.model.forget_target = None,
            Click::Dismiss => {
                self.model.failure = None;
                self.model.message = None;
                self.model.connection_error = None;
            }
            Click::Control(action) => match self.model.prepare(action, now) {
                Err(e) => self.model.failure = Some(e),
                Ok(cmd) => {
                    if self.demo {
                        if let Some(s) = &mut self.model.snapshot {
                            demo::apply(s, cmd.action);
                        }
                        self.model.finish(Ok(()));
                    } else if let Some(client) = &self.client {
                        if client.commands.try_send(cmd).is_err() {
                            self.model.finish(Err(
                                "IPC worker unavailable or busy. No request was queued.".into(),
                            ));
                        }
                    } else {
                        self.model.finish(Err("IPC worker is unavailable".into()));
                    }
                }
            },
        }
        cx.notify();
    }
    fn button(
        &self,
        id: &'static str,
        label: impl Into<SharedString>,
        selected: bool,
        enabled: bool,
        click: Click,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .debug_selector(move || id.to_string())
            .flex()
            .items_center()
            .justify_center()
            .flex_1()
            .min_h(px(38.))
            .px_3()
            .py_2()
            .rounded_lg()
            .border_1()
            .border_color(rgb(if selected { GREEN } else { LINE }))
            .bg(rgb(if selected { 0x30483a } else { CARD }))
            .text_color(rgb(if selected { GREEN } else { TEXT }))
            .text_sm()
            .when(!enabled, |d| d.opacity(0.35))
            .when(enabled, |d| {
                d.cursor_pointer()
                    .tab_index(0)
                    .focus(|d| d.border_color(rgb(0xffffff)))
                    .hover(|d| d.bg(rgb(0x35413b)))
                    .on_click(cx.listener(move |this, _, _, cx| this.click(click, cx)))
                    .on_key_down(cx.listener(move |this, e: &KeyDownEvent, _, cx| {
                        if e.keystroke.key == "enter" || e.keystroke.key == "space" {
                            this.click(click, cx);
                            cx.stop_propagation();
                        }
                    }))
            })
            .child(label.into())
    }
    fn control(
        &self,
        id: &'static str,
        label: impl Into<SharedString>,
        selected: bool,
        a: Action,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        self.button(
            id,
            label,
            selected,
            self.model.can(a, self.now()),
            Click::Control(a),
            cx,
        )
    }
    fn toggle_row(
        &self,
        title: &'static str,
        detail: &'static str,
        id: &'static str,
        value: Option<bool>,
        a: Action,
        cx: &mut Context<Self>,
    ) -> Div {
        div()
            .flex()
            .items_center()
            .gap_4()
            .child(
                div()
                    .flex_1()
                    .child(div().text_sm().child(title))
                    .child(div().text_xs().text_color(rgb(MUTED)).child(detail)),
            )
            .child(div().w(px(78.)).child(self.control(
                id,
                match value {
                    Some(true) => "On",
                    Some(false) => "Off",
                    None => "—",
                },
                value == Some(true),
                a,
                cx,
            )))
    }
}
fn section(title: &'static str) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .bg(rgb(CARD))
        .p_4()
        .rounded_xl()
        .border_1()
        .border_color(rgb(LINE))
        .child(div().text_xs().text_color(rgb(MUTED)).child(title))
}
fn earbud(flipped: bool) -> Div {
    div()
        .relative()
        .w(px(55.))
        .h(px(72.))
        .child(
            div()
                .absolute()
                .top_0()
                .left(px(if flipped { 6. } else { 0. }))
                .w(px(43.))
                .h(px(31.))
                .rounded_full()
                .bg(rgb(0xe6eeea)),
        )
        .child(
            div()
                .absolute()
                .top(px(16.))
                .left(px(if flipped { 12. } else { 29. }))
                .w(px(14.))
                .h(px(53.))
                .rounded_full()
                .bg(rgb(0xd9e4de)),
        )
        .child(
            div()
                .absolute()
                .top(px(9.))
                .left(px(if flipped { 28. } else { 8. }))
                .w(px(9.))
                .h(px(12.))
                .rounded_full()
                .bg(rgb(0x4c5b54)),
        )
}
impl Render for Panel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = self.now();
        let current = self.model.current(now);
        let snapshot = self.model.snapshot.clone();
        let s = snapshot.as_ref();
        let ready = current && s.is_some_and(|s| s.connected);
        let title = s
            .map(|s| s.device.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "Your AirPods".into());
        let subtitle = s
            .map(|s| model::model_name(s.device.model).to_string())
            .unwrap_or_else(|| "Waiting for the daemon".into());
        let selected = s.filter(|_| current).and_then(model::mode);
        let ca = s.filter(|_| current).and_then(|s| model::toggle(s, 0x28));
        let one = s.filter(|_| current).and_then(|s| model::toggle(s, 0x1b));
        let adaptive = s
            .and_then(|s| model::control(s, 0x2e))
            .filter(|v| *v <= 100);
        let policy = s.map(|s| s.ear_policy);
        let mut content = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(div().text_xs().text_color(rgb(MUTED)).child("LIBREPODS"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(if ready { GREEN } else { MUTED }))
                            .child(if self.demo {
                                "DEMO · NO HARDWARE"
                            } else if self.model.pending {
                                "AWAITING CONFIRMATION"
                            } else if !current {
                                "STATUS UNAVAILABLE"
                            } else if ready {
                                "CONNECTED"
                            } else if s.is_some_and(|s| s.bluez_connected) {
                                "SYNCING CONTROLS"
                            } else {
                                "DISCONNECTED"
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .py_2()
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .child(earbud(false))
                            .child(earbud(true)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .child(
                                div()
                                    .text_2xl()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(title),
                            )
                            .child(
                                div()
                                    .mt_1()
                                    .text_xs()
                                    .text_color(rgb(MUTED))
                                    .child(subtitle),
                            ),
                    ),
            );
        if let Some(s) = s {
            let mut batteries = div().flex().gap_2();
            let b = &s.device.battery;
            let readings = if s.device.model.capabilities().headset {
                vec![("Headset", "battery.headset", b.headset)]
            } else {
                vec![
                    ("Left", "battery.left", b.left),
                    ("Right", "battery.right", b.right),
                    ("Case", "battery.case", b.case_),
                ]
            };
            for (label, key, b) in readings {
                let b = model::battery(s, label, key, b, current);
                let now_unix_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .min(u64::MAX as u128) as u64;
                let detail = b.detail_at(now_unix_ms);
                batteries = batteries.child(
                    div()
                        .flex_1()
                        .p_3()
                        .bg(rgb(CARD))
                        .border_1()
                        .border_color(rgb(LINE))
                        .rounded_lg()
                        .child(div().text_xs().text_color(rgb(MUTED)).child(b.label))
                        .child(
                            div()
                                .mt_1()
                                .text_2xl()
                                .text_color(rgb(if b.stale { MUTED } else { TEXT }))
                                .child(
                                    b.percent
                                        .map(|v| format!("{v}%"))
                                        .unwrap_or_else(|| "—".into()),
                                ),
                        )
                        .child(div().mt_1().text_xs().text_color(rgb(MUTED)).child(detail)),
                );
            }
            content = content.child(batteries);
        }
        let modes = [
            ("off", "Off", Mode::Off),
            ("anc", "ANC", Mode::NoiseCancellation),
            ("transparency", "Transparency", Mode::Transparency),
            ("adaptive", "Adaptive", Mode::Adaptive),
        ];
        let mut mode_rows = div().flex().flex_col().gap_2();
        for row in modes.chunks(if window.viewport_size().width < px(500.) {
            2
        } else {
            4
        }) {
            mode_rows = mode_rows.child(div().flex().gap_2().children(row.iter().map(
                |(id, label, mode)| {
                    self.control(id, *label, selected == Some(*mode), Action::Mode(*mode), cx)
                },
            )));
        }
        let mut noise = section("LISTENING MODE").child(mode_rows);
        if selected == Some(Mode::Adaptive) {
            noise = noise.child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().flex_1().text_sm().child("Adaptive strength"))
                    .child(self.control(
                        "less",
                        "−",
                        false,
                        Action::Adaptive(adaptive.unwrap_or(50).saturating_sub(10)),
                        cx,
                    ))
                    .child(
                        div().text_sm().child(
                            adaptive
                                .map(|v| format!("{v}%"))
                                .unwrap_or_else(|| "—".into()),
                        ),
                    )
                    .child(self.control(
                        "more",
                        "+",
                        false,
                        Action::Adaptive(adaptive.unwrap_or(50).saturating_add(10).min(100)),
                        cx,
                    )),
            );
        }
        content = content
            .child(noise)
            .child(
                section("PREFERENCES")
                    .child(self.toggle_row(
                        "Conversation awareness",
                        "Reduce audio while speaking",
                        "conversation",
                        ca,
                        Action::Conversation(!ca.unwrap_or(false)),
                        cx,
                    ))
                    .child(self.toggle_row(
                        "Noise cancellation with one AirPod",
                        "Keep ANC when wearing a single earbud",
                        "one-bud",
                        one,
                        Action::OneBud(!one.unwrap_or(false)),
                        cx,
                    )),
            )
            .child(
                section("PAUSE WHEN REMOVED").child(
                    div()
                        .flex()
                        .gap_2()
                        .child(self.control(
                            "ear-off",
                            "Never",
                            policy == Some(EarPolicy::Off),
                            Action::EarPolicy(EarPolicy::Off),
                            cx,
                        ))
                        .child(self.control(
                            "ear-one",
                            "One AirPod",
                            policy == Some(EarPolicy::OneRemoved),
                            Action::EarPolicy(EarPolicy::OneRemoved),
                            cx,
                        ))
                        .child(self.control(
                            "ear-both",
                            "Both AirPods",
                            policy == Some(EarPolicy::BothRemoved),
                            Action::EarPolicy(EarPolicy::BothRemoved),
                            cx,
                        )),
                ),
            );
        let is_linked = s.is_some_and(|s| s.bluez_connected);
        content = content.child(
            div()
                .flex()
                .gap_2()
                .child(self.control(
                    "link",
                    if is_linked { "Disconnect" } else { "Connect" },
                    false,
                    if is_linked {
                        Action::Disconnect
                    } else {
                        Action::Connect
                    },
                    cx,
                ))
                .child(self.button(
                    "forget",
                    "Forget device…",
                    false,
                    self.model.can(Action::Forget, now),
                    Click::AskForget,
                    cx,
                )),
        );
        if let Some(target) = &self.model.forget_target {
            content = content.child(
                section("REMOVE BLUETOOTH PAIRING?")
                    .child(div().text_sm().child(format!(
                        "Forget {}? You will need to pair again. Private keys will be removed.",
                        target.address
                    )))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(self.button(
                                "cancel",
                                "Cancel",
                                false,
                                true,
                                Click::CancelForget,
                                cx,
                            ))
                            .child(self.control(
                                "confirm-forget",
                                "Forget this device",
                                false,
                                Action::Forget,
                                cx,
                            )),
                    ),
            );
        }
        if current && !ready {
            if let Some(error) = s.and_then(|s| s.last_error.as_ref()) {
                content = content.child(
                    div()
                        .text_xs()
                        .text_color(rgb(MUTED))
                        .child(error.message.clone()),
                );
            }
        }
        if !current && !self.model.pending {
            content =
                content.child(div().text_xs().text_color(rgb(MUTED)).child(
                    "Waiting for the daemon. If it is not running, start librepods-cli serve. Retrying automatically.",
                ));
        } else if self.model.capabilities["device_management"] != true {
            content = content.child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child("Connection actions require an updated daemon."),
            );
        }
        if let Some(message) = self
            .model
            .failure
            .as_ref()
            .or(self.model.connection_error.as_ref())
            .or(self.model.message.as_ref())
        {
            content = content.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(rgb(
                                if self.model.failure.is_some()
                                    || self.model.connection_error.is_some()
                                {
                                    0xf0b7a8
                                } else {
                                    MUTED
                                },
                            ))
                            .child(message.clone()),
                    )
                    .child(div().w(px(50.)).child(self.button(
                        "dismiss",
                        "×",
                        false,
                        !self.model.pending,
                        Click::Dismiss,
                        cx,
                    ))),
            );
        }
        div()
            .id("panel")
            .track_focus(&self.focus)
            .size_full()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .font_family("sans-serif")
            .on_action(|_: &Next, w, _| w.focus_next())
            .on_action(|_: &Previous, w, _| w.focus_prev())
            .on_action(cx.listener(|this, _: &Close, w, cx| {
                if this.model.forget_target.is_some() {
                    this.model.forget_target = None;
                    cx.notify();
                } else {
                    w.remove_window();
                }
            }))
            .p_5()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .items_center()
            .child(div().flex_none().w_full().max_w(px(560.)).child(content))
    }
}
pub fn run(demo_mode: bool) -> Result<(), Box<dyn std::error::Error>> {
    let client = if demo_mode {
        None
    } else {
        Some(Client::start(librepods_gui::client::socket_from_env()?)?)
    };
    let startup_error = std::rc::Rc::new(std::cell::RefCell::new(None));
    let error = startup_error.clone();
    Application::new().run(move |cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("tab", Next, None),
            KeyBinding::new("shift-tab", Previous, None),
            KeyBinding::new("escape", Close, None),
        ]);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let bounds = Bounds::centered(None, size(px(510.), px(780.)), cx);
        if let Err(e) = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(460.), px(500.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("LibrePods".into()),
                    ..Default::default()
                }),
                app_id: Some("org.librepods.GUI".into()),
                ..Default::default()
            },
            move |window, cx| cx.new(|cx| Panel::new(demo_mode, client, window, cx)),
        ) {
            *error.borrow_mut() = Some(format!("Cannot open LibrePods window: {e}"));
            cx.quit();
        }
        cx.activate(true);
    });
    if let Some(error) = startup_error.take() {
        return Err(std::io::Error::other(error).into());
    }
    Ok(())
}

#[cfg(all(test, feature = "ui-test"))]
mod tests {
    use super::*;
    use gpui::{Modifiers, TestAppContext, VisualTestContext};
    fn click(cx: &mut VisualTestContext, id: &'static str) {
        cx.run_until_parked();
        let bounds = cx
            .debug_bounds(id)
            .unwrap_or_else(|| panic!("missing {id}"));
        cx.simulate_click(bounds.center(), Modifiers::default());
        cx.run_until_parked();
    }
    #[gpui::test]
    fn demo_buttons_change_reported_modes_and_preferences(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|window, cx| Panel::new(true, None, window, cx));
        cx.simulate_resize(size(px(510.), px(900.)));
        click(cx, "adaptive");
        view.read_with(cx, |p, _| {
            assert_eq!(
                p.model.snapshot.as_ref().unwrap().device.listening_mode,
                Some(Mode::Adaptive)
            )
        });
        click(cx, "more");
        view.read_with(cx, |p, _| {
            assert_eq!(
                model::control(p.model.snapshot.as_ref().unwrap(), 0x2e),
                Some(60)
            )
        });
        click(cx, "conversation");
        view.read_with(cx, |p, _| {
            assert_eq!(
                model::toggle(p.model.snapshot.as_ref().unwrap(), 0x28),
                Some(true)
            )
        });
        click(cx, "one-bud");
        view.read_with(cx, |p, _| {
            assert_eq!(
                model::toggle(p.model.snapshot.as_ref().unwrap(), 0x1b),
                Some(false)
            )
        });
        click(cx, "ear-both");
        view.read_with(cx, |p, _| {
            assert_eq!(
                p.model.snapshot.as_ref().unwrap().ear_policy,
                EarPolicy::BothRemoved
            )
        });
    }
    #[gpui::test]
    fn forget_requires_confirmation_and_escape_cancels_it(cx: &mut TestAppContext) {
        cx.update(|cx| cx.bind_keys([KeyBinding::new("escape", Close, None)]));
        let (view, cx) = cx.add_window_view(|w, c| Panel::new(true, None, w, c));
        cx.simulate_resize(size(px(510.), px(1000.)));
        click(cx, "forget");
        view.read_with(cx, |p, _| assert!(p.model.forget_target.is_some()));
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        view.read_with(cx, |p, _| assert!(p.model.forget_target.is_none()));
        click(cx, "forget");
        click(cx, "confirm-forget");
        view.read_with(cx, |p, _| {
            assert!(p.model.snapshot.as_ref().unwrap().device.address.is_empty())
        });
    }
    #[gpui::test]
    fn keyboard_can_activate_controls_and_offline_window_is_safe(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.bind_keys([
                KeyBinding::new("tab", Next, None),
                KeyBinding::new("shift-tab", Previous, None),
            ])
        });
        let (view, cx) = cx.add_window_view(|w, c| Panel::new(true, None, w, c));
        cx.simulate_resize(size(px(510.), px(900.)));
        cx.simulate_keystrokes("tab enter");
        cx.run_until_parked();
        view.read_with(cx, |p, _| {
            assert_eq!(
                p.model.snapshot.as_ref().unwrap().device.listening_mode,
                Some(Mode::Off)
            )
        });
        view.update(cx, |p, cx| {
            p.demo = false;
            p.model.unavailable("not running".into());
            cx.notify();
        });
        cx.run_until_parked();
        click(cx, "anc");
        view.read_with(cx, |p, _| {
            assert_eq!(
                p.model.snapshot.as_ref().unwrap().device.listening_mode,
                Some(Mode::Off)
            )
        });
    }
    #[gpui::test]
    fn narrow_windows_wrap_modes_and_scroll_to_connection_controls(cx: &mut TestAppContext) {
        let (_, cx) = cx.add_window_view(|w, c| Panel::new(true, None, w, c));
        cx.simulate_resize(size(px(460.), px(500.)));
        cx.run_until_parked();
        let off = cx.debug_bounds("off").unwrap();
        let adaptive = cx.debug_bounds("adaptive").unwrap();
        assert!(adaptive.origin.y > off.origin.y);
        let before = cx.debug_bounds("forget").unwrap();
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: gpui::point(px(230.), px(450.)),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-1000.))),
            ..Default::default()
        });
        cx.run_until_parked();
        let after = cx.debug_bounds("forget").unwrap();
        assert!(
            after.origin.y < before.origin.y,
            "small windows must actually scroll"
        );
        assert!(after.bottom() <= px(500.));
    }
}
