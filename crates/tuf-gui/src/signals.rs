use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Weak};
use zbus::blocking::{connection::Builder, Connection, MessageIterator};
use zbus::message::Type;
use zbus::proxy::CacheProperties;
use zbus::zvariant::{OwnedValue, Value};
use zbus::MatchRule;

use crate::controls;
use crate::hardware::{HardwareQueue, RefreshKind};
use crate::AppWindow;

const DEBOUNCE_WINDOW: Duration = Duration::from_millis(180);
const MAX_DEBOUNCE_WAIT: Duration = Duration::from_millis(500);
const RECONNECT_INITIAL: Duration = Duration::from_millis(500);
const RECONNECT_MAX: Duration = Duration::from_secs(10);
const PORTAL_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BurstEvent {
    refresh: Option<RefreshKind>,
    power: bool,
}

impl BurstEvent {
    fn from_refresh(kind: RefreshKind) -> Self {
        Self {
            refresh: Some(kind),
            power: false,
        }
    }

    fn power_event() -> Self {
        Self {
            refresh: Some(RefreshKind::Fast),
            power: true,
        }
    }

    fn merge(self, other: Self) -> Self {
        let refresh = match (self.refresh, other.refresh) {
            (Some(a), Some(b)) => Some(a.merge(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        };
        Self {
            refresh,
            power: self.power || other.power,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ThemeSync {
    last_portal_dark: bool,
    overridden: bool,
}

impl ThemeSync {
    fn new(initial_dark: bool) -> Self {
        Self {
            last_portal_dark: initial_dark,
            overridden: false,
        }
    }

    fn next_dark_mode(&mut self, current_ui_dark: bool, portal_dark: bool) -> Option<bool> {
        if !self.overridden && current_ui_dark != self.last_portal_dark {
            self.overridden = true;
        }
        if self.overridden {
            return None;
        }
        self.last_portal_dark = portal_dark;
        Some(portal_dark)
    }
}

fn extract_portal_u32(val: &Value<'_>) -> Option<u32> {
    match val {
        Value::U32(n) => Some(*n),
        Value::Value(inner) => extract_portal_u32(inner),
        _ => None,
    }
}

/// Reads `org.freedesktop.appearance` `color-scheme` from `org.freedesktop.portal.Settings`
/// (`1 => dark`, `2 => light`, fallback `true`).
pub fn read_portal_dark_mode() -> bool {
    read_portal_dark_mode_result().unwrap_or(true)
}

fn read_portal_dark_mode_result() -> zbus::Result<bool> {
    let conn = Builder::session()?.method_timeout(PORTAL_TIMEOUT).build()?;
    let proxy: zbus::blocking::Proxy<'_> = zbus::blocking::proxy::Builder::new(&conn)
        .destination("org.freedesktop.portal.Desktop")?
        .path("/org/freedesktop/portal/desktop")?
        .interface("org.freedesktop.portal.Settings")?
        .cache_properties(CacheProperties::No)
        .build()?;
    let value: OwnedValue = match proxy.call(
        "ReadOne",
        &("org.freedesktop.appearance", "color-scheme"),
    ) {
        Ok(val) => val,
        Err(_) => proxy.call("Read", &("org.freedesktop.appearance", "color-scheme"))?,
    };
    Ok(extract_portal_u32(&value).is_none_or(controls::portal_color_scheme_to_dark))
}

#[derive(Clone, Default)]
struct SignalStop {
    inner: Arc<(Mutex<bool>, Condvar)>,
}

impl SignalStop {
    fn stop(&self) {
        let (lock, cvar) = &*self.inner;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cvar.notify_all();
    }

    fn is_stopped(&self) -> bool {
        *self.inner.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn wait_timeout(&self, duration: Duration) -> bool {
        let (lock, cvar) = &*self.inner;
        let mut stopped = lock.lock().unwrap_or_else(|e| e.into_inner());
        if *stopped {
            return true;
        }
        let (guard, _) = cvar
            .wait_timeout_while(stopped, duration, |flag| !*flag)
            .unwrap_or_else(|e| e.into_inner());
        stopped = guard;
        *stopped
    }
}

pub struct SignalHandle {
    stop: SignalStop,
}

impl SignalHandle {
    pub fn stop(&self) {
        self.stop.stop();
    }
}

impl Drop for SignalHandle {
    fn drop(&mut self) {
        self.stop.stop();
    }
}

/// Starts background D-Bus signal listeners that coalesce external hardware,
/// AC power, portal theme, and display topology changes into UI refreshes without idle polling.
pub fn spawn(queue: HardwareQueue, ui: Weak<AppWindow>, initial_dark: bool) -> SignalHandle {
    let (ev_tx, ev_rx) = mpsc::channel::<BurstEvent>();
    let stop = SignalStop::default();

    let asus_tx = ev_tx.clone();
    let asus_stop = stop.clone();
    let _ = thread::Builder::new()
        .name("tuf-sig-asus".into())
        .spawn(move || listen_asusd(&asus_tx, &asus_stop));

    let upower_tx = ev_tx.clone();
    let upower_stop = stop.clone();
    let _ = thread::Builder::new()
        .name("tuf-sig-upower".into())
        .spawn(move || listen_upower(&upower_tx, &upower_stop));

    let mutter_monitors_tx = ev_tx.clone();
    let mutter_monitors_stop = stop.clone();
    let _ = thread::Builder::new()
        .name("tuf-sig-mutter".into())
        .spawn(move || listen_mutter_monitors(&mutter_monitors_tx, &mutter_monitors_stop));

    let mutter_props_stop = stop.clone();
    let _ = thread::Builder::new()
        .name("tuf-sig-backlight".into())
        .spawn(move || listen_mutter_properties(&ev_tx, &mutter_props_stop));

    let portal_ui = ui.clone();
    let portal_stop = stop.clone();
    let _ = thread::Builder::new()
        .name("tuf-sig-portal".into())
        .spawn(move || listen_portal_theme(portal_ui, initial_dark, &portal_stop));

    let dispatch_stop = stop.clone();
    let _ = thread::Builder::new()
        .name("tuf-sig-dispatch".into())
        .spawn(move || debounce_loop(ev_rx, queue, ui, dispatch_stop));

    SignalHandle { stop }
}

fn collect_burst(
    first: BurstEvent,
    rx: &Receiver<BurstEvent>,
    window: Duration,
    max_wait: Duration,
) -> Option<BurstEvent> {
    let mut pending = first;
    let deadline = Instant::now() + max_wait;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Some(pending);
        }
        match rx.recv_timeout(window.min(remaining)) {
            Ok(next) => pending = pending.merge(next),
            Err(RecvTimeoutError::Timeout) => return Some(pending),
            Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

fn debounce_loop(
    rx: Receiver<BurstEvent>,
    queue: HardwareQueue,
    ui: Weak<AppWindow>,
    stop: SignalStop,
) {
    while !stop.is_stopped() && !queue.is_shutdown() {
        let Ok(first) = rx.recv() else {
            return;
        };
        if stop.is_stopped() || queue.is_shutdown() {
            return;
        }
        let Some(pending) = collect_burst(first, &rx, DEBOUNCE_WINDOW, MAX_DEBOUNCE_WAIT) else {
            return;
        };
        if stop.is_stopped() || queue.is_shutdown() {
            return;
        }
        let q = queue.clone();
        if ui
            .upgrade_in_event_loop(move |app| {
                if !app.window().is_visible() {
                    if pending.power && q.is_auto_refresh_enabled() && !app.get_busy() {
                        q.request_poll();
                    }
                    return;
                }
                match pending.refresh {
                    Some(RefreshKind::Full) => app.invoke_refresh_requested(),
                    Some(RefreshKind::Fast) | None => {
                        if !app.get_busy() && !app.get_refreshing() {
                            q.request_poll();
                        }
                    }
                }
            })
            .is_err()
        {
            return;
        }
    }
}

fn listen_asusd(tx: &Sender<BurstEvent>, stop: &SignalStop) {
    run_listener(
        stop,
        Connection::system,
        || {
            MatchRule::builder()
                .msg_type(Type::Signal)
                .path_namespace("/xyz/ljones")
                .and_then(|b| b.interface("org.freedesktop.DBus.Properties"))
                .and_then(|b| b.member("PropertiesChanged"))
                .map(|b| b.build())
        },
        tx,
        BurstEvent::from_refresh(RefreshKind::Fast),
    );
}

fn listen_upower(tx: &Sender<BurstEvent>, stop: &SignalStop) {
    run_listener(
        stop,
        Connection::system,
        || {
            MatchRule::builder()
                .msg_type(Type::Signal)
                .path("/org/freedesktop/UPower")
                .and_then(|b| b.interface("org.freedesktop.DBus.Properties"))
                .and_then(|b| b.member("PropertiesChanged"))
                .map(|b| b.build())
        },
        tx,
        BurstEvent::power_event(),
    );
}

fn listen_mutter_monitors(tx: &Sender<BurstEvent>, stop: &SignalStop) {
    run_listener(
        stop,
        Connection::session,
        || {
            MatchRule::builder()
                .msg_type(Type::Signal)
                .interface("org.gnome.Mutter.DisplayConfig")
                .and_then(|b| b.member("MonitorsChanged"))
                .map(|b| b.build())
        },
        tx,
        BurstEvent::from_refresh(RefreshKind::Full),
    );
}

fn listen_mutter_properties(tx: &Sender<BurstEvent>, stop: &SignalStop) {
    run_listener(
        stop,
        Connection::session,
        || {
            MatchRule::builder()
                .msg_type(Type::Signal)
                .path("/org/gnome/Mutter/DisplayConfig")
                .and_then(|b| b.interface("org.freedesktop.DBus.Properties"))
                .and_then(|b| b.member("PropertiesChanged"))
                .map(|b| b.build())
        },
        tx,
        BurstEvent::from_refresh(RefreshKind::Fast),
    );
}

fn listen_portal_theme(ui: Weak<AppWindow>, initial_dark: bool, stop: &SignalStop) {
    let sync = Arc::new(Mutex::new(ThemeSync::new(initial_dark)));
    let mut backoff = RECONNECT_INITIAL;
    let mut had_disconnect = false;
    while !stop.is_stopped() {
        let Ok(rule) = MatchRule::builder()
            .msg_type(Type::Signal)
            .path("/org/freedesktop/portal/desktop")
            .and_then(|b| b.interface("org.freedesktop.portal.Settings"))
            .and_then(|b| b.member("SettingChanged"))
            .map(|b| b.build())
        else {
            return;
        };
        match Connection::session() {
            Ok(conn) => {
                if let Ok(iter) = MessageIterator::for_match_rule(rule, &conn, Some(8)) {
                    if std::mem::take(&mut had_disconnect) {
                        let dark = read_portal_dark_mode();
                        if apply_portal_theme(&ui, &sync, dark).is_err() {
                            return;
                        }
                    }
                    backoff = RECONNECT_INITIAL;
                    for msg in iter {
                        if stop.is_stopped() {
                            return;
                        }
                        match msg {
                            Ok(message) => {
                                if let Some(dark) = parse_portal_setting_changed(&message) {
                                    if apply_portal_theme(&ui, &sync, dark).is_err() {
                                        return;
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
                had_disconnect = true;
            }
            Err(_) => {
                had_disconnect = true;
            }
        }
        if stop.wait_timeout(backoff) {
            return;
        }
        backoff = (backoff * 2).min(RECONNECT_MAX);
    }
}

fn apply_portal_theme(
    ui: &Weak<AppWindow>,
    sync: &Arc<Mutex<ThemeSync>>,
    portal_dark: bool,
) -> Result<(), slint::EventLoopError> {
    let sync = Arc::clone(sync);
    ui.upgrade_in_event_loop(move |app| {
        let mut guard = sync.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(next_dark) = guard.next_dark_mode(app.get_dark_mode(), portal_dark) {
            app.set_dark_mode(next_dark);
        }
    })
}

fn parse_portal_setting_changed(msg: &zbus::Message) -> Option<bool> {
    let (namespace, key, value): (String, String, OwnedValue) = msg.body().deserialize().ok()?;
    if namespace == "org.freedesktop.appearance" && key == "color-scheme" {
        let scheme = extract_portal_u32(&value)?;
        Some(controls::portal_color_scheme_to_dark(scheme))
    } else {
        None
    }
}

fn run_listener<'a>(
    stop: &SignalStop,
    connect: impl Fn() -> zbus::Result<Connection>,
    build_rule: impl Fn() -> zbus::Result<MatchRule<'a>>,
    tx: &Sender<BurstEvent>,
    event: BurstEvent,
) {
    let mut backoff = RECONNECT_INITIAL;
    let mut had_disconnect = false;
    while !stop.is_stopped() {
        let Ok(rule) = build_rule() else {
            return;
        };
        match connect() {
            Ok(conn) => {
                if let Ok(iter) = MessageIterator::for_match_rule(rule, &conn, Some(8)) {
                    if std::mem::take(&mut had_disconnect) && tx.send(event).is_err() {
                        return;
                    }
                    backoff = RECONNECT_INITIAL;
                    for msg in iter {
                        if stop.is_stopped() {
                            return;
                        }
                        match msg {
                            Ok(_) => {
                                if tx.send(event).is_err() {
                                    return;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
                had_disconnect = true;
            }
            Err(_) => {
                had_disconnect = true;
            }
        }
        if stop.wait_timeout(backoff) {
            return;
        }
        backoff = (backoff * 2).min(RECONNECT_MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_kind_prefers_full_when_burst_mixes_signals() {
        assert_eq!(RefreshKind::Fast.merge(RefreshKind::Fast), RefreshKind::Fast);
        assert_eq!(RefreshKind::Fast.merge(RefreshKind::Full), RefreshKind::Full);
        assert_eq!(RefreshKind::Full.merge(RefreshKind::Fast), RefreshKind::Full);
    }

    #[test]
    fn debounce_burst_respects_max_wait_cap_under_continuous_signals() {
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            for idx in 0..12 {
                let kind = if idx == 1 {
                    BurstEvent::from_refresh(RefreshKind::Full)
                } else if idx == 2 {
                    BurstEvent::power_event()
                } else {
                    BurstEvent::from_refresh(RefreshKind::Fast)
                };
                if tx.send(kind).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(25));
            }
        });

        let first = rx.recv().unwrap();
        let start = Instant::now();
        let merged = collect_burst(
            first,
            &rx,
            Duration::from_millis(60),
            Duration::from_millis(120),
        );
        let elapsed = start.elapsed();
        assert_eq!(
            merged,
            Some(BurstEvent {
                refresh: Some(RefreshKind::Full),
                power: true
            })
        );
        assert!(elapsed < Duration::from_millis(220));
        let _ = handle.join();
    }

    #[test]
    fn portal_theme_sync_respects_in_session_manual_override() {
        let mut sync = ThemeSync::new(true);
        assert_eq!(sync.next_dark_mode(true, false), Some(false));
        assert_eq!(sync.next_dark_mode(false, true), Some(true));

        // User manually flipped UI dark_mode to false while portal was true.
        assert_eq!(sync.next_dark_mode(false, false), None);
        // Subsequent portal updates stay ignored after manual override.
        assert_eq!(sync.next_dark_mode(false, true), None);
    }

    #[test]
    fn extract_portal_u32_handles_direct_and_nested_variants() {
        let direct = Value::U32(2);
        assert_eq!(extract_portal_u32(&direct), Some(2));
        let nested = Value::Value(Box::new(Value::U32(1)));
        assert_eq!(extract_portal_u32(&nested), Some(1));
        let wrong = Value::Bool(true);
        assert_eq!(extract_portal_u32(&wrong), None);
    }
}
