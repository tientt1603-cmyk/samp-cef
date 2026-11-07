use crate::app::{Event, ExternalBrowser};
use crate::audio::{Audio, BrowserAudioSettings};
use crate::browser::client::WebClient;
use crate::external::{BrowserReadyCallback, CallbackList};

use cef::browser::BrowserHost;
use cef::handlers::render::PaintElement;
use cef::types::list::List;
use cef::types::string::CefString;
use cef_sys::{cef_event_flags_t, cef_key_event_t, cef_mouse_button_type_t, cef_mouse_event_t};

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use winapi::um::winuser::{GetDoubleClickTime, GetSystemMetrics, SM_CXDOUBLECLK, SM_CYDOUBLECLK};

use client_api::gta::rw::rwcore::RwTexture;
use client_api::gta::rw::rwplcore::RwSurfaceProperties;

use crossbeam_channel::Sender;

#[derive(Debug, Clone, Copy, Hash, Ord, PartialOrd, Eq, PartialEq)]
pub enum MouseKey {
    Left,
    Middle,
    Right,
}

#[derive(Debug, Clone)]
struct ClickSequence {
    button: MouseKey,
    x: i32,
    y: i32,
    count: i32,
    pressed_at: Instant,
}

#[derive(Debug, Clone, Default)]
struct Mouse {
    x: i32,
    y: i32,
    keys: HashMap<MouseKey, bool>,
    last_click: Option<ClickSequence>,
}

fn windowless_frame_rate(fps: u64) -> i32 {
    fps.max(15).min(i32::MAX as u64) as i32
}

impl Mouse {
    fn click_count(
        &mut self, button: MouseKey, is_down: bool, now: Instant, max_interval: Duration,
        max_delta_x: i32, max_delta_y: i32,
    ) -> i32 {
        if !is_down {
            return self
                .last_click
                .as_ref()
                .filter(|click| click.button == button)
                .map(|click| click.count)
                .unwrap_or(1);
        }

        let count = self
            .last_click
            .as_ref()
            .filter(|click| {
                click.button == button
                    && now.duration_since(click.pressed_at) <= max_interval
                    && (self.x - click.x).abs() <= max_delta_x
                    && (self.y - click.y).abs() <= max_delta_y
            })
            .map(|click| click.count.saturating_add(1))
            .unwrap_or(1);

        self.last_click = Some(ClickSequence {
            button,
            x: self.x,
            y: self.y,
            count,
            pressed_at: now,
        });

        count
    }
}

#[derive(Clone)]
pub struct ExternalClient {
    pub browser: Arc<WebClient>,
    pub object_ids: Vec<i32>,
    pub texture: String,
    pub scale: i32,
    pub origin_texture: *mut RwTexture,
    pub origin_surface_props: RwSurfaceProperties,
    pub audio_settings: BrowserAudioSettings,
    pub prev_replacement: *mut RwTexture,
}

pub struct Manager {
    clients: HashMap<u32, Arc<WebClient>>,
    audio: Arc<Audio>,
    ready_callbacks: HashMap<u32, Vec<BrowserReadyCallback>>,
    clients_on_txd: Vec<ExternalClient>,
    pending_object_ids: HashMap<u32, Vec<i32>>,
    focused: Option<u32>,
    focused_queue: VecDeque<u32>,
    input_corrupted: bool,
    do_not_draw: bool,
    event_tx: Sender<Event>,
    mouse: Mouse,
    view_width: usize,
    view_height: usize,
    cef_running: bool,
}

impl Manager {
    pub fn new(event_tx: Sender<Event>, audio: Arc<Audio>) -> Manager {
        // init cef
        // crate::browser::cef::initialize(event_tx.clone());

        let mut keys = HashMap::new();

        keys.insert(MouseKey::Left, false);
        keys.insert(MouseKey::Middle, false);
        keys.insert(MouseKey::Right, false);

        let mouse = Mouse {
            x: 0,
            y: 0,
            keys,
            last_click: None,
        };

        Manager {
            clients: HashMap::new(),
            ready_callbacks: HashMap::new(),
            clients_on_txd: Vec::new(),
            pending_object_ids: HashMap::new(),
            view_height: 0,
            view_width: 0,
            input_corrupted: false,
            do_not_draw: false,
            cef_running: false,
            focused: None,
            focused_queue: VecDeque::new(),
            audio,
            mouse,
            event_tx,
        }
    }

    pub fn create_browser(&mut self, id: u32, cbs: CallbackList, url: &str) {
        let render_mode = crate::utils::current_render_mode();
        tracing::trace!(browser = id, url, ?render_mode, "creating overlay browser");

        let client = WebClient::new(id, cbs, self.event_tx.clone(), self.audio.clone());

        crate::browser::cef::create_browser(client.clone(), url);
        self.append_client(id, client);
    }

    pub fn create_browser_on_texture(&mut self, ext: &ExternalBrowser, cbs: CallbackList) {
        tracing::trace!(
            browser = ext.id,
            texture = %ext.texture,
            scale = ext.scale,
            "creating object browser"
        );

        let client = WebClient::new_extern(ext.id, cbs, self.event_tx.clone(), self.audio.clone());

        crate::browser::cef::create_browser(client.clone(), &ext.url);
        self.append_client(ext.id, client.clone());

        let ext_client = ExternalClient {
            browser: client,
            texture: ext.texture.clone(),
            object_ids: Vec::new(),
            scale: ext.scale,
            origin_texture: std::ptr::null_mut(),
            origin_surface_props: RwSurfaceProperties {
                ambient: 0.0,
                diffuse: 0.0,
                specular: 0.0,
            },
            audio_settings: BrowserAudioSettings {
                max_distance: crate::audio::MAX_DISTANCE,
                reference_distance: crate::audio::REFRENCE_DISTANCE,
            },
            prev_replacement: std::ptr::null_mut(),
        };

        self.clients_on_txd.push(ext_client);

        if let Some(object_ids) = self.pending_object_ids.remove(&ext.id) {
            tracing::trace!(
                browser = ext.id,
                objects = object_ids.len(),
                "applying pending object attachments"
            );

            for object_id in object_ids {
                self.browser_append_to_object(ext.id, object_id);
            }
        }
    }

    #[inline]
    pub fn browser_append_to_object(&mut self, id: u32, object_id: i32) {
        tracing::debug!(
            browser = id,
            object = object_id,
            "attaching browser to object"
        );

        if !self
            .clients_on_txd
            .iter()
            .any(|client| client.browser.id() == id)
        {
            tracing::trace!(
                browser = id,
                object = object_id,
                "queuing object attachment until browser is ready"
            );
            self.pending_object_ids
                .entry(id)
                .or_default()
                .push(object_id);
            return;
        }

        self.audio.add_source(id, object_id);

        self.clients_on_txd
            .iter_mut()
            .filter(|cl| cl.browser.id() == id)
            .for_each(|cl| {
                cl.object_ids.push(object_id);
                cl.browser.add_object(object_id);

                if cl.object_ids.len() == 1 {
                    cl.browser.hide(false);
                }
            });
    }

    #[inline]
    pub fn browser_remove_from_object(&mut self, id: u32, object_id: i32) {
        self.audio.remove_source(id, object_id);

        self.clients_on_txd
            .iter_mut()
            .filter(|cl| cl.browser.id() == id)
            .for_each(|cl| {
                let mut idx = 0;

                while idx < cl.object_ids.len() {
                    if cl.object_ids[idx] == object_id {
                        cl.object_ids.remove(idx);
                        cl.browser.remove_object(object_id);
                    } else {
                        idx += 1;
                    }
                }

                if cl.object_ids.is_empty() {
                    cl.browser.hide(true);
                }
            });
    }

    #[inline]
    fn append_client(&mut self, id: u32, client: Arc<WebClient>) {
        if let Some(client) = self.clients.insert(id, client) {
            self.internal_close(client, true);
        }
    }

    #[inline]
    pub fn draw(&self) {
        for client in self.clients.values().filter(|client| !client.is_hidden()) {
            client.update_view();
        }

        if self.do_not_draw {
            return;
        }

        if let Some(&focus) = self.focused.as_ref() {
            for client in self
                .clients
                .values()
                .filter(|client| client.id() != focus && !client.is_extern() && !client.is_hidden())
            {
                client.draw();
            }

            if let Some(focused) = self
                .clients
                .get(&focus)
                .filter(|client| !client.is_extern() && !client.is_hidden())
            {
                focused.draw();
            }
        } else {
            for client in self
                .clients
                .values()
                .filter(|client| !client.is_extern() && !client.is_hidden())
            {
                client.draw();
            }
        }
    }

    #[inline]
    pub fn on_lost_device(&mut self) {
        tracing::debug!("Direct3D device lost");

        for browser in self.clients.values() {
            browser.on_lost_device();

            if browser.is_extern() {
                self.clients_on_txd
                    .iter_mut()
                    .filter(|client| client.browser.id() == browser.id())
                    .for_each(|client| client.origin_texture = std::ptr::null_mut());
            }
        }
    }

    #[inline]
    pub fn on_reset_device(&self) {
        tracing::debug!("Direct3D device reset");

        for client in self.clients.values() {
            client.on_reset_device();

            if let Some(host) = client.browser().map(|browser| browser.host()) {
                host.invalidate(PaintElement::View)
            }
        }
    }

    #[inline]
    pub fn resize(&mut self, width: usize, height: usize) {
        tracing::trace!(
            width,
            height,
            previous_width = self.view_width,
            previous_height = self.view_height,
            "resizing browsers"
        );

        if width == self.view_width && height == self.view_height {
            return;
        }

        self.view_width = width;
        self.view_height = height;

        for browser in self.clients.values() {
            if !browser.is_extern() {
                browser.resize(width, height);
            }
        }
    }

    /// VSL: where the mouse goes - the focused page; else, in the VSL pointer mode (Mắt thứ ba, "vsl:pointer"),
    /// the page that always listens (the VSL interface) while the keyboard stays with the game.
    fn mouse_host(&self) -> Option<BrowserHost> {
        if let Some(client) = self.focused.as_ref().and_then(|id| self.clients.get(id)) {
            return client.browser().map(|browser| browser.host());
        }

        if crate::vsl::pointer_mode() > 0 {
            return self
                .clients
                .values()
                .find(|client| client.always_listen_keys())
                .and_then(|client| client.browser().map(|browser| browser.host()));
        }

        None
    }

    #[inline]
    pub fn send_mouse_move_event(&mut self, x: i32, y: i32) {
        if self.input_corrupted {
            return;
        }

        if let Some(host) = self.mouse_host() {
            self.mouse.x = x;
            self.mouse.y = y;

            let keys = &self.mouse.keys;

            let mut event = cef_mouse_event_t { x, y, modifiers: 0 };

            if keys.get(&MouseKey::Left).cloned().unwrap_or(false) {
                event.modifiers |= cef_event_flags_t::EVENTFLAG_LEFT_MOUSE_BUTTON as u32;
            }

            if keys.get(&MouseKey::Middle).cloned().unwrap_or(false) {
                event.modifiers |= cef_event_flags_t::EVENTFLAG_MIDDLE_MOUSE_BUTTON as u32;
            }

            if keys.get(&MouseKey::Right).cloned().unwrap_or(false) {
                event.modifiers |= cef_event_flags_t::EVENTFLAG_RIGHT_MOUSE_BUTTON as u32;
            }

            host.send_mouse_move(event);
        }
    }

    #[inline]
    pub fn send_mouse_click_event(&mut self, button: MouseKey, is_down: bool) {
        if self.input_corrupted {
            return;
        }

        if let Some(host) = self.mouse_host() {
            self.mouse.keys.insert(button, is_down);

            let event = cef_mouse_event_t {
                x: self.mouse.x,
                y: self.mouse.y,
                modifiers: 0,
            };

            let key = match button {
                MouseKey::Left => cef_mouse_button_type_t::MBT_LEFT,
                MouseKey::Middle => cef_mouse_button_type_t::MBT_MIDDLE,
                MouseKey::Right => cef_mouse_button_type_t::MBT_RIGHT,
            };

            let double_click_time = unsafe { GetDoubleClickTime() };
            let double_click_width = unsafe { GetSystemMetrics(SM_CXDOUBLECLK) };
            let double_click_height = unsafe { GetSystemMetrics(SM_CYDOUBLECLK) };
            let click_count = self.mouse.click_count(
                button,
                is_down,
                Instant::now(),
                Duration::from_millis(double_click_time.into()),
                (double_click_width / 2).max(1),
                (double_click_height / 2).max(1),
            );

            host.send_mouse_click(key, event, is_down, click_count);
        }
    }

    #[inline]
    pub fn send_mouse_wheel(&self, delta: i32) {
        if self.input_corrupted {
            return;
        }

        if let Some(host) = self.mouse_host() {
            host.send_mouse_wheel(self.mouse.x, self.mouse.y, delta);
        }
    }

    #[inline]
    pub fn send_keyboard_event(&self, event: cef_key_event_t) {
        if self.input_corrupted {
            return;
        }

        // отправлять события клавиш ТОЛЬКО сфокусированному браузеру. иначе можно и другим запросившим
        if let Some(client) = self.focused.as_ref().and_then(|id| self.clients.get(id))
            && let Some(host) = client.browser().map(|browser| browser.host())
        {
            host.send_keyboard_event(event);
        } else {
            for host in self
                .clients
                .values()
                .filter(|client| client.always_listen_keys())
                .filter_map(|client| client.browser().map(|browser| browser.host()))
            {
                host.send_keyboard_event(event);
            }
        }
    }

    pub fn trigger_event(&self, event_name: &str, list: List) {
        for client in self.clients.values() {
            if let Some(frame) = client.browser().map(|browser| browser.main_frame()) {
                let name = CefString::new(event_name);
                let msg = cef::process_message::ProcessMessage::create("trigger_event");

                let args = msg.argument_list();
                args.set_string(0, &name);
                args.set_list(1, list.clone());

                frame.send_process_message(cef::ProcessId::Renderer, msg);
            }
        }
    }

    pub fn close_browser(&mut self, id: u32, force_close: bool) {
        self.pending_object_ids.remove(&id);

        if let Some(client) = self.clients.remove(&id) {
            self.internal_close(client, force_close);
        }

        self.cleanup_focus_after_close(id);
    }

    #[inline]
    pub fn hide_browser(&self, id: u32, hide: bool) {
        if let Some(browser) = self.clients.get(&id) {
            browser.hide(hide);
        }
    }

    pub fn browser_focus(&mut self, id: u32, focus: bool) {
        if self.clients.contains_key(&id) {
            if focus {
                if let Some(&cur_id) = self.focused.as_ref() {
                    if cur_id != id {
                        self.focused_queue.push_back(id);
                    }
                } else {
                    self.focused = Some(id);
                }
            } else if self
                .focused
                .as_ref()
                .filter(|focused| **focused == id)
                .is_some()
            {
                self.focused = self.focused_queue.pop_front();
            } else {
                self.focused_queue
                    .iter()
                    .position(|&queue| queue == id)
                    .map(|idx| self.focused_queue.remove(idx));
            }
        }
    }

    #[inline(always)]
    pub fn is_input_blocked(&self) -> bool {
        self.focused.is_some()
    }

    #[inline(always)]
    pub fn is_input_available(&self, browser: u32) -> bool {
        if self.input_corrupted {
            return false;
        }

        if self.is_input_blocked() {
            self.focused.as_ref().filter(|&&id| id == browser).is_some()
        } else {
            true
        }
    }

    #[inline(always)]
    pub fn set_corrupted(&mut self, corrupted: bool) {
        self.input_corrupted = corrupted;
    }

    #[inline(always)]
    pub fn is_input_corrupted(&self) -> bool {
        self.input_corrupted
    }

    #[inline(always)]
    pub fn do_not_draw(&mut self, donot: bool) {
        if self.do_not_draw != donot {
            self.do_not_draw = donot;
            self.temporary_hide(donot);
        }
    }

    #[inline(always)]
    pub fn browser_exists(&self, browser_id: u32) -> bool {
        self.clients.contains_key(&browser_id)
    }

    #[inline(always)]
    pub fn browser_ready(&self, browser_id: u32) -> bool {
        self.clients
            .get(&browser_id)
            .and_then(|client| client.browser())
            .map(|browser| !browser.is_loading())
            .unwrap_or(false)
    }

    pub fn toggle_dev_tools(&self, browser_id: u32, enabled: bool) {
        if let Some(client) = self.clients.get(&browser_id) {
            client.toggle_dev_tools(enabled)
        }
    }

    pub fn always_listen_keys(&self, browser_id: u32, listen: bool) {
        if let Some(client) = self.clients.get(&browser_id) {
            client.set_always_listen_keys(listen)
        }
    }

    pub fn set_audio_settings(&mut self, browser_id: u32, audio_settings: BrowserAudioSettings) {
        self.clients_on_txd
            .iter_mut()
            .filter(|cl| cl.browser.id() == browser_id)
            .for_each(|cl| {
                cl.audio_settings = audio_settings;
            });
    }

    /// VSL: every page loads again (the watchdog found the page silent)
    pub fn reload_all(&self) {
        for client in self.clients.values() {
            if let Some(browser) = client.browser() {
                browser.reload();
            }
        }
    }

    /// VSL: a page is open (on a disconnect it stays, showing "connection lost")
    pub fn has_browsers(&self) -> bool {
        !self.clients.is_empty()
    }

    pub fn load_url(&self, browser_id: u32, url: &str) {
        if let Some(client) = self.clients.get(&browser_id) {
            client.load_url(url)
        }
    }

    pub fn call_browser_ready(&self, browser_id: u32) {
        if let Some(callbacks) = self.ready_callbacks.get(&browser_id) {
            callbacks.iter().for_each(|cb| cb(browser_id))
        }
    }

    pub fn add_browser_ready(&mut self, browser_id: u32, callback: BrowserReadyCallback) {
        if self.browser_ready(browser_id) {
            callback(browser_id);
            return;
        }

        self.ready_callbacks
            .entry(browser_id)
            .or_default()
            .push(callback);
    }

    #[inline(always)]
    pub fn external_browsers(&mut self) -> &mut [ExternalClient] {
        &mut self.clients_on_txd
    }

    #[inline(always)]
    pub fn update_fps(&mut self, fps: u64) {
        let fps_small = windowless_frame_rate(fps);
        for client in self.clients.values() {
            client.set_windowless_frame_rate(fps_small);
        }
    }

    pub fn close_all_browsers(&mut self) {
        self.clients_on_txd.clear();
        self.pending_object_ids.clear();
        let audio = self.audio.clone();

        self.clients
            .drain()
            .for_each(|(_, client)| Self::internal_close_client(client, &audio, true));

        self.focused = None;
        self.focused_queue.clear();
    }

    pub fn initialize_cef(&mut self) {
        if self.cef_running {
            tracing::debug!("CEF runtime is already initialized");
            return;
        }

        let initialized = crate::browser::cef::initialize(self.event_tx.clone());

        self.cef_running = initialized;
        if !initialized {
            tracing::error!("CEF runtime initialization failed");
        }
    }

    pub fn shutdown_cef(&mut self) {
        if !self.cef_running {
            return;
        }

        tracing::debug!("shutting down CEF runtime");
        crate::browser::cef::shutdown();
        self.cef_running = false;
        tracing::debug!("CEF runtime shut down");
    }

    pub fn remove_views(&mut self) {
        tracing::trace!(browsers = self.clients.len(), "removing browser views");

        for client in self.clients.values() {
            client.remove_view();
        }
    }

    #[inline]
    fn temporary_hide(&self, hide: bool) {
        for client in self.clients.values() {
            if hide {
                client.internal_hide(true, true);
            } else {
                client.restore_hide_status();

                if let Some(host) = client.browser().map(|browser| browser.host()) {
                    host.invalidate(PaintElement::View);
                }
            }
        }
    }

    fn internal_close(&mut self, client: Arc<WebClient>, force_close: bool) {
        if client.is_extern()
            && let Some(idx) = self
                .clients_on_txd
                .iter()
                .position(|cl| cl.browser.id() == client.id())
        {
            self.clients_on_txd.remove(idx);
        }

        Self::internal_close_client(client, &self.audio, force_close);
    }

    fn internal_close_client(client: Arc<WebClient>, audio: &Arc<Audio>, force_close: bool) {
        let browser = client.id();
        tracing::debug!(browser, force = force_close, "closing browser");

        client.close(force_close);
        client.remove_view();
        audio.remove_all_streams(browser);
    }

    fn cleanup_focus_after_close(&mut self, closed_id: u32) {
        self.focused_queue
            .retain(|&id| id != closed_id && self.clients.contains_key(&id));

        let focused_stale = self
            .focused
            .as_ref()
            .filter(|id| !self.clients.contains_key(id))
            .is_some();

        if self.focused == Some(closed_id) || focused_stale {
            self.focused = None;

            while let Some(next_id) = self.focused_queue.pop_front() {
                if self.clients.contains_key(&next_id) {
                    self.focused = Some(next_id);
                    break;
                }
            }
        }

        if self.clients.is_empty() {
            self.focused = None;
            self.focused_queue.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOUBLE_CLICK_TIME: Duration = Duration::from_millis(500);

    #[test]
    fn windowless_frame_rate_has_no_practical_upper_fps_cap() {
        assert_eq!(windowless_frame_rate(0), 15);
        assert_eq!(windowless_frame_rate(240), 240);
        assert_eq!(windowless_frame_rate(u64::MAX), i32::MAX);
    }

    #[test]
    fn click_sequence_counts_matching_clicks() {
        let mut mouse = Mouse::default();
        mouse.x = 100;
        mouse.y = 200;
        let start = Instant::now();

        assert_eq!(
            mouse.click_count(MouseKey::Left, true, start, DOUBLE_CLICK_TIME, 2, 2),
            1
        );
        assert_eq!(
            mouse.click_count(MouseKey::Left, false, start, DOUBLE_CLICK_TIME, 2, 2),
            1
        );
        assert_eq!(
            mouse.click_count(
                MouseKey::Left,
                true,
                start + Duration::from_millis(100),
                DOUBLE_CLICK_TIME,
                2,
                2,
            ),
            2
        );
        assert_eq!(
            mouse.click_count(
                MouseKey::Left,
                false,
                start + Duration::from_millis(110),
                DOUBLE_CLICK_TIME,
                2,
                2,
            ),
            2
        );
        assert_eq!(
            mouse.click_count(
                MouseKey::Left,
                true,
                start + Duration::from_millis(200),
                DOUBLE_CLICK_TIME,
                2,
                2,
            ),
            3
        );
    }

    #[test]
    fn click_sequence_resets_after_timeout_movement_or_button_change() {
        let mut mouse = Mouse::default();
        let start = Instant::now();

        assert_eq!(
            mouse.click_count(MouseKey::Left, true, start, DOUBLE_CLICK_TIME, 2, 2),
            1
        );
        assert_eq!(
            mouse.click_count(
                MouseKey::Left,
                true,
                start + Duration::from_millis(501),
                DOUBLE_CLICK_TIME,
                2,
                2,
            ),
            1
        );

        mouse.x = 3;
        assert_eq!(
            mouse.click_count(
                MouseKey::Left,
                true,
                start + Duration::from_millis(600),
                DOUBLE_CLICK_TIME,
                2,
                2,
            ),
            1
        );
        assert_eq!(
            mouse.click_count(
                MouseKey::Right,
                true,
                start + Duration::from_millis(650),
                DOUBLE_CLICK_TIME,
                2,
                2,
            ),
            1
        );
    }
}
