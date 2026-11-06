//! VSL Roleplay patch (2026-11-04). The VSL server draws its whole interface in CEF, so on a VSL
//! server the plugin hides what SA-MP / GTA draw by themselves and hands the keys to the pages.
//!
//! Events from the server (browser events the plugin also reads):
//!  * "vsl:chatlock" 1/0  - old SA-MP chat window kept off (mode 0); T / F6 -> "vsl:chatkey"
//!  * "vsl:uilock"   1/0  - GTA HUD + radar off (unless the player turned it back on), ESC -> "vsl:esc",
//!                          TAB -> "vsl:tab", F1 -> "vsl:f1", M -> "vsl:mapkey", live HUD values -> "vsl:hud",
//!                          own position + camera heading -> "vsl:pos"; the map page of the GTA pause menu
//!                          is closed (the map is an item on VSL servers: Owner 2026-11-07)
//!  * "vsl:escpass"  1/0  - a SA-MP textdraw selection is open: ESC must reach SA-MP (cancel)
//!  * "vsl:gameset"  key value - "hud" 0/1 (show the GTA HUD), "fps" 0/1 (frame limiter)
//!  * "vsl:gameget"        - answer "vsl:gamecfg" hud fps
//!  * "vsl:nativemenu"     - open the GTA pause menu (settings the VSL menu does not have)
//!  * "vsl:quitgame"       - leave the game now
//! Events from the page (read before they go to the server):
//!  * "vsl:beat"           - the page is alive (every 2 s, never sent to the server). Watchdog: while
//!                           the chat is locked and the page stays silent (8 s; 30 s for the first page
//!                           load) the old SA-MP chat and GTA HUD come back until the page beats again.
//!  * "vsl:ui" "quit"      - sent on to the server (it saves + closes the session), then the game exits
//!  * "vsl:say" ch hex     - a SA-MP client command typed in the VSL chat (/q, /quit, /fpslimit,
//!                           /headmove, /dl, /timestamp ...) runs in the game like in the old chat
//! Any connection reset unlocks everything and restores the player's own settings (the old chat then
//! shows SA-MP's own "server closed the connection" / ban lines): other servers are never touched.
//! Leaving with the VSL quit never unlocks (no flash of the old chat while the game closes).
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use cef::types::list::List;
use client_api::samp::version::{version, Version};

static LOCKED: AtomicBool = AtomicBool::new(false);
static SAVED_MODE: AtomicI32 = AtomicI32::new(-1);
static SWALLOW_CHAR: AtomicBool = AtomicBool::new(false);

static UI_LOCK: AtomicBool = AtomicBool::new(false);
static ESC_PASS: AtomicBool = AtomicBool::new(false);
static HUD_WANTED: AtomicBool = AtomicBool::new(false); // player choice: show the GTA HUD anyway
static SAVED_HUD: AtomicI32 = AtomicI32::new(-1);
static LAST_HUD_MS: AtomicU64 = AtomicU64::new(0);
static LAST_HUD: std::sync::Mutex<[i32; 11]> = std::sync::Mutex::new([i32::MIN; 11]);
static LAST_POS_MS: AtomicU64 = AtomicU64::new(0);
static LAST_INPUT_CLOSE_MS: AtomicU64 = AtomicU64::new(0);
static LAST_POS: std::sync::Mutex<[i32; 3]> = std::sync::Mutex::new([i32::MIN; 3]);

// watchdog + quit
static WD_OPEN: AtomicBool = AtomicBool::new(false); // page silent: SA-MP chat / HUD shown again
static LAST_BEAT_MS: AtomicU64 = AtomicU64::new(0); // 0 = the page has not beaten yet
static LOCK_SINCE_MS: AtomicU64 = AtomicU64::new(0);
static LAST_FRAME_MS: AtomicU64 = AtomicU64::new(0);
static QUITTING: AtomicBool = AtomicBool::new(false);
static QUIT_AT_MS: AtomicU64 = AtomicU64::new(0);
const BEAT_TIMEOUT_MS: u64 = 8000;
const FIRST_BEAT_TIMEOUT_MS: u64 = 30000;
const QUIT_DELAY_MS: u64 = 300; // time for "vsl:ui quit" to leave the network thread

/// CChat::m_nMode (0 off, 1 light, 2 full) - same place in 0.3.7-R1, R3 and 0.3.DL-R1.
const CHAT_MODE_OFFSET: usize = 0x8;

// GTA San Andreas 1.0 US (the only exe SA-MP runs on)
const MENU_MANAGER: usize = 0xBA6748;
const MENU_HUD_ON: usize = 0x21; // bool m_bHudOn (HUD + radar)
const MENU_ACTIVATE_NEXT: usize = 0x33; // bool m_bActivateMenuNextFrame
const MENU_FRAME_LIMITER: usize = 0x4C; // bool m_bFrameLimiterOn
const MENU_ACTIVE: usize = 0x5C; // bool m_bMenuActive
const MENU_PAGE: usize = 0x15D; // u8 m_nCurrentMenuPage
const MENU_PAGE_MAP: u8 = 5; // eMenuPage MENUPAGE_MAP
const MENU_PAGE_PAUSE: u8 = 42; // eMenuPage MENUPAGE_PAUSE_MENU
const PLAYER_PED: usize = 0xB6F5F0; // CPed* of the local player
const PLAYER_MONEY: usize = 0xB7CE50; // int
const CLOCK_HOURS: usize = 0xB70153; // u8
const CLOCK_MINUTES: usize = 0xB70152; // u8
const PED_HEALTH: usize = 0x540; // f32
const PED_ARMOUR: usize = 0x548; // f32
const PED_STATE: usize = 0x530; // i32, 50 = driving
const PED_VEHICLE: usize = 0x58C; // CVehicle*
const PED_WEAPONS: usize = 0x5A0; // CWeapon[13], 0x1C each
const PED_ACTIVE_SLOT: usize = 0x718; // u8
const VEH_MOVE_SPEED: usize = 0x44; // CVector
const VEH_HEALTH: usize = 0x4C0; // f32

fn chat_ptr() -> Option<*mut u8> {
    // CChat pointer = the word right before CInput in samp.dll
    let offset = match version() {
        Version::V037 => 0x21A0E4,
        Version::V037R3 => 0x26E8C8,
        Version::V03DL => 0x2ACA10,
        _ => return None,
    };

    unsafe {
        let base = client_api::samp::handle();

        if base.is_null() {
            return None;
        }

        let chat = (base.add(offset) as *const *mut u8).read_unaligned();

        if chat.is_null() { None } else { Some(chat) }
    }
}

fn menu_byte(offset: usize) -> *mut u8 {
    (MENU_MANAGER + offset) as *mut u8
}

// ------------------------------------------------------------------ chat lock
fn restore_chat() {
    let saved = SAVED_MODE.swap(-1, Ordering::Relaxed);

    if saved > 0 {
        if let Some(chat) = chat_ptr() {
            unsafe { (chat.add(CHAT_MODE_OFFSET) as *mut i32).write_unaligned(saved) };
        }
    }
}

pub fn set_locked(lock: bool) {
    let was = LOCKED.swap(lock, Ordering::SeqCst);

    if lock && !was {
        LOCK_SINCE_MS.store(now_ms(), Ordering::Relaxed);
    }

    if was && !lock {
        restore_chat();
        WD_OPEN.store(false, Ordering::Relaxed);
        LAST_BEAT_MS.store(0, Ordering::Relaxed);
    }

    tracing::info!(locked = lock, "VSL chat lock");
}

/// locked by the server and the page is alive
pub fn is_locked() -> bool {
    LOCKED.load(Ordering::Relaxed) && !WD_OPEN.load(Ordering::Relaxed)
}

// ------------------------------------------------------------------ UI lock
fn restore_hud() {
    ESC_PASS.store(false, Ordering::Relaxed);
    let saved = SAVED_HUD.swap(-1, Ordering::Relaxed);

    if saved >= 0 {
        unsafe { menu_byte(MENU_HUD_ON).write(saved as u8) };
    }

    if let Ok(mut last) = LAST_HUD.lock() {
        *last = [i32::MIN; 11];
    }

    if let Ok(mut last) = LAST_POS.lock() {
        *last = [i32::MIN; 3];
    }
}

pub fn set_ui_locked(lock: bool) {
    let was = UI_LOCK.swap(lock, Ordering::SeqCst);

    if was && !lock {
        restore_hud();
    }

    tracing::info!(locked = lock, "VSL UI lock");
}

pub fn is_ui_locked() -> bool {
    UI_LOCK.load(Ordering::Relaxed) && !WD_OPEN.load(Ordering::Relaxed)
}

// ------------------------------------------------------------------ watchdog
/// "vsl:beat" from the page
pub fn beat() {
    LAST_BEAT_MS.store(now_ms(), Ordering::Relaxed);
}

fn watchdog(now: u64) {
    let last_frame = LAST_FRAME_MS.swap(now, Ordering::Relaxed);

    // the game itself stood still (loading, alt-tab): give the page a fresh window
    if last_frame != 0 && now.saturating_sub(last_frame) > 2000 {
        LOCK_SINCE_MS.store(now, Ordering::Relaxed);

        if LAST_BEAT_MS.load(Ordering::Relaxed) != 0 {
            LAST_BEAT_MS.store(now, Ordering::Relaxed);
        }
    }

    if !LOCKED.load(Ordering::Relaxed) {
        WD_OPEN.store(false, Ordering::Relaxed);
        return;
    }

    let beat = LAST_BEAT_MS.load(Ordering::Relaxed);
    let dead = if beat == 0 {
        now.saturating_sub(LOCK_SINCE_MS.load(Ordering::Relaxed)) > FIRST_BEAT_TIMEOUT_MS
    } else {
        now.saturating_sub(beat) > BEAT_TIMEOUT_MS
    };
    let was = WD_OPEN.swap(dead, Ordering::Relaxed);

    if dead && !was {
        restore_chat();
        restore_hud();
        tracing::warn!("VSL page is silent: SA-MP chat and GTA HUD shown again");
    } else if !dead && was {
        tracing::info!("VSL page is back: locks active again");
    }
}

// ------------------------------------------------------------------ quit
/// leave the game: the network thread gets a moment to send "vsl:ui quit", then the process ends
pub fn begin_quit() {
    if QUITTING.swap(true, Ordering::SeqCst) {
        return;
    }

    QUIT_AT_MS.store(now_ms() + QUIT_DELAY_MS, Ordering::Relaxed);
    tracing::info!("VSL quit requested");
}

pub fn is_quitting() -> bool {
    QUITTING.load(Ordering::Relaxed)
}

pub fn set_esc_pass(pass: bool) {
    ESC_PASS.store(pass, Ordering::Relaxed);
}

pub fn esc_pass() -> bool {
    ESC_PASS.load(Ordering::Relaxed)
}

/// "vsl:gameset" key value
pub fn game_set(key: &str, value: i32) {
    match key {
        "hud" => HUD_WANTED.store(value == 1, Ordering::Relaxed),
        "fps" => unsafe { menu_byte(MENU_FRAME_LIMITER).write(if value == 1 { 1 } else { 0 }) },
        _ => (),
    }
}

/// "vsl:gamecfg" hud fps
pub fn game_cfg() -> List {
    let list = List::new();
    list.set_integer(0, if HUD_WANTED.load(Ordering::Relaxed) { 1 } else { 0 });
    list.set_integer(1, unsafe { menu_byte(MENU_FRAME_LIMITER).read() } as i32);
    list
}

pub fn open_native_menu() {
    unsafe { menu_byte(MENU_ACTIVATE_NEXT).write(1) };
}

/// SA-MP client command (the table SA-MP's own chat uses); true = found and run
pub fn run_client_command(name: &str, args: &str) -> bool {
    use client_api::samp::inputs::Input;

    if name.is_empty() || name.len() > 32 {
        return false;
    }

    if let Some(input) = Input::get() {
        unsafe {
            let count = std::ptr::addr_of!(input.m_nCommandCount).read_unaligned().max(0) as usize;
            let names = std::ptr::addr_of!(input.m_szCommandName).read_unaligned();
            let procs = std::ptr::addr_of!(input.m_pCommandProc).read_unaligned();

            for i in 0..count.min(procs.len()) {
                let raw = &names[i];
                let len = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
                let cmd: Vec<u8> = raw[..len].iter().map(|&c| c as u8).collect();

                if cmd.eq_ignore_ascii_case(name.as_bytes()) {
                    if let Some(proc_) = procs[i] {
                        // SA-MP passes the text after the command (ANSI, max 128)
                        let mut bytes: Vec<u8> = args
                            .bytes()
                            .filter(|&b| b >= 0x20 && b < 0x7F)
                            .take(128)
                            .collect();
                        bytes.push(0);
                        proc_(bytes.as_ptr() as *const std::os::raw::c_char);
                        return true;
                    }

                    return false;
                }
            }
        }
    }

    false
}

/// what to do with a line from the VSL chat ("vsl:say" "<channel> <hex text>")
pub enum SayAction {
    Forward,
    Handled,
    Quit,
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    let t = text.as_bytes();

    if t.len() % 2 != 0 {
        return None;
    }

    let val = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };

    let mut out = Vec::with_capacity(t.len() / 2);

    for pair in t.chunks(2) {
        out.push((val(pair[0])? << 4) | val(pair[1])?);
    }

    Some(out)
}

pub fn client_say(arguments: &str) -> SayAction {
    let mut parts = arguments.splitn(2, ' ');
    let _channel = parts.next();
    let hex = parts.next().unwrap_or("").trim();

    let bytes = match unhex(hex) {
        Some(b) => b,
        None => return SayAction::Forward,
    };

    let text = String::from_utf8_lossy(&bytes);
    let text = text.trim();

    if !text.starts_with('/') {
        return SayAction::Forward;
    }

    let body = &text[1..];
    let (name, args) = match body.find(' ') {
        Some(at) => (&body[..at], body[at + 1..].trim()),
        None => (body, ""),
    };

    if name.eq_ignore_ascii_case("q") || name.eq_ignore_ascii_case("quit") {
        return SayAction::Quit;
    }

    if run_client_command(name, args) {
        SayAction::Handled
    } else {
        SayAction::Forward
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// called once per frame from the main loop; returns new HUD values when something changed
pub fn frame() -> Option<List> {
    let now = now_ms();
    let quit_at = QUIT_AT_MS.load(Ordering::Relaxed);

    if quit_at != 0 && now >= quit_at {
        tracing::info!("VSL quit: closing the game");
        unsafe { winapi::um::processthreadsapi::ExitProcess(0) };
    }

    watchdog(now);

    if is_locked() {
        close_samp_input(now);

        if let Some(chat) = chat_ptr() {
            unsafe {
                let mode = chat.add(CHAT_MODE_OFFSET) as *mut i32;
                let current = mode.read_unaligned();

                if current != 0 {
                    if SAVED_MODE.load(Ordering::Relaxed) < 0 {
                        SAVED_MODE.store(current, Ordering::Relaxed);
                    }

                    mode.write_unaligned(0);
                }
            }
        }
    }

    if !is_ui_locked() {
        return None;
    }

    // the GTA pause menu stays (graphics, sound, keys) but its map page is not: back to the pause page
    unsafe {
        if menu_byte(MENU_ACTIVE).read() != 0 && menu_byte(MENU_PAGE).read() == MENU_PAGE_MAP {
            menu_byte(MENU_PAGE).write(MENU_PAGE_PAUSE);
            tracing::info!("VSL: GTA map page closed");
        }
    }

    unsafe {
        let hud = menu_byte(MENU_HUD_ON);
        let want = if HUD_WANTED.load(Ordering::Relaxed) { 1u8 } else { 0u8 };
        let current = hud.read();

        if SAVED_HUD.load(Ordering::Relaxed) < 0 {
            SAVED_HUD.store(current as i32, Ordering::Relaxed);
        }

        if current != want {
            hud.write(want);
        }
    }

    // live HUD values, at most ~7 times per second and only when something changed
    if now.saturating_sub(LAST_HUD_MS.load(Ordering::Relaxed)) < 150 {
        return None;
    }

    LAST_HUD_MS.store(now, Ordering::Relaxed);

    unsafe {
        let ped = (PLAYER_PED as *const usize).read_unaligned();

        if ped == 0 {
            return None;
        }

        let health = ((ped + PED_HEALTH) as *const f32).read_unaligned();
        let armour = ((ped + PED_ARMOUR) as *const f32).read_unaligned();
        let slot = ((ped + PED_ACTIVE_SLOT) as *const u8).read_unaligned().min(12) as usize;
        let weapon = ped + PED_WEAPONS + slot * 0x1C;
        let w_type = (weapon as *const i32).read_unaligned();
        let w_clip = ((weapon + 0x8) as *const u32).read_unaligned();
        let w_total = ((weapon + 0xC) as *const u32).read_unaligned();
        let money = (PLAYER_MONEY as *const i32).read_unaligned();
        let hours = (CLOCK_HOURS as *const u8).read_unaligned();
        let minutes = (CLOCK_MINUTES as *const u8).read_unaligned();

        let state = ((ped + PED_STATE) as *const i32).read_unaligned();
        let vehicle = ((ped + PED_VEHICLE) as *const usize).read_unaligned();
        let (in_vehicle, speed, veh_health) = if state == 50 && vehicle != 0 {
            let vx = ((vehicle + VEH_MOVE_SPEED) as *const f32).read_unaligned();
            let vy = ((vehicle + VEH_MOVE_SPEED + 4) as *const f32).read_unaligned();
            let vz = ((vehicle + VEH_MOVE_SPEED + 8) as *const f32).read_unaligned();
            let kmh = (vx * vx + vy * vy + vz * vz).sqrt() * 180.0;
            let hp = ((vehicle + VEH_HEALTH) as *const f32).read_unaligned();
            (1, kmh as i32, hp as i32)
        } else {
            (0, 0, 0)
        };

        let values = [
            health.max(0.0).min(1000.0) as i32,
            armour.max(0.0).min(1000.0) as i32,
            money,
            w_type,
            w_clip.min(99999) as i32,
            w_total.min(99999) as i32,
            hours as i32,
            minutes as i32,
            in_vehicle,
            speed.clamp(0, 999),
            veh_health.clamp(0, 1000),
        ];

        if let Ok(mut last) = LAST_HUD.lock() {
            if *last == values {
                return None;
            }

            *last = values;
        }

        let list = List::new();

        for (i, v) in values.iter().enumerate() {
            list.set_integer(i, *v);
        }

        Some(list)
    }
}

/// own position (metres) + camera heading (degrees, 0 = north, clockwise) for the VSL map, compass and GPS:
/// ~7 times per second, only when it moved >= 1 m or turned >= 2 degrees. Data of the player himself only
/// (the server decides what the page may show: map / GPS / compass items).
pub fn pos_frame(position: client_api::gta::matrix::CVector, camera: &client_api::gta::matrix::RwMatrix) -> Option<List> {
    if !is_ui_locked() {
        return None;
    }

    let now = now_ms();

    if now.saturating_sub(LAST_POS_MS.load(Ordering::Relaxed)) < 150 {
        return None;
    }

    LAST_POS_MS.store(now, Ordering::Relaxed);

    // where the camera looks = from the camera to the player (the GTA camera stands behind him): no guess
    // about matrix axes (Owner test 2026-11-07: the 'at' axis gave a wrong heading). Camera inside the
    // player (first person, close aim): keep the last heading.
    let (dx, dy) = (position.x - camera.pos.x, position.y - camera.pos.y);
    let last_heading = LAST_POS.lock().map(|l| l[2]).unwrap_or(0);
    let heading = if dx * dx + dy * dy < 0.25 {
        if last_heading == i32::MIN { 0 } else { last_heading }
    } else {
        let mut h = dx.atan2(dy).to_degrees();   // 0 = north (+y), 90 = east (+x): clockwise

        if h < 0.0 {
            h += 360.0;
        }

        h.round() as i32 % 360
    };

    let values = [position.x.round() as i32, position.y.round() as i32, heading];

    if let Ok(mut last) = LAST_POS.lock() {
        let same = last[0] != i32::MIN
            && values[0] == last[0]
            && values[1] == last[1]
            && (values[2] - last[2]).rem_euclid(360).min((last[2] - values[2]).rem_euclid(360)) < 2;

        if same {
            return None;
        }

        *last = values;
    }

    let list = List::new();

    for (i, v) in values.iter().enumerate() {
        list.set_integer(i, *v);
    }

    Some(list)
}

// ------------------------------------------------------------------ SA-MP chat input: never opens while locked
// Root fix (Owner 2026-11-07, "giải pháp triệt để"): SA-MP's CInput::Open is hooked; under the chat lock it
// does nothing, whatever calls it (T, F6, SA-MP's own re-open paths). Address found in samp.dll 0.3.DL by
// the only code that writes m_bEnabled = 1 (offset 0x14E0); the first bytes are checked before hooking, so a
// different samp.dll is never patched (the state closer below stays as the safety net).
type InputOpenFn = extern "thiscall" fn(*mut std::ffi::c_void);

static mut INPUT_OPEN_HOOK: Option<retour::GenericDetour<InputOpenFn>> = None;

const DL_INPUT_OPEN: usize = 0x68EC0;
const DL_INPUT_OPEN_BYTES: [u8; 12] = [0x83, 0xEC, 0x10, 0x56, 0x8B, 0xF1, 0x8B, 0x86, 0xE0, 0x14, 0x00, 0x00];

/// called once from App::initialize_hooks (SA-MP is loaded)
pub fn install_hooks() {
    if version() != Version::V03DL {
        tracing::info!("VSL: no SA-MP input hook for this SA-MP version (state closer only)");
        return;
    }

    unsafe {
        let base = client_api::samp::handle();

        if base.is_null() {
            return;
        }

        let addr = base.add(DL_INPUT_OPEN);
        let head = std::slice::from_raw_parts(addr as *const u8, DL_INPUT_OPEN_BYTES.len());

        if head != DL_INPUT_OPEN_BYTES {
            tracing::warn!("VSL: samp.dll is not the expected 0.3.DL build: SA-MP input hook skipped");
            return;
        }

        let open: InputOpenFn = std::mem::transmute(addr);

        match retour::GenericDetour::new(open, input_open) {
            Ok(hook) => {
                if hook.enable().is_ok() {
                    *std::ptr::addr_of_mut!(INPUT_OPEN_HOOK) = Some(hook);
                    tracing::info!("VSL: SA-MP input hook installed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "VSL: SA-MP input hook failed"),
        }
    }
}

extern "thiscall" fn input_open(this: *mut std::ffi::c_void) {
    if is_locked() {
        return;   // the VSL chat replaces it: never open the old input
    }

    unsafe {
        if let Some(hook) = (*std::ptr::addr_of!(INPUT_OPEN_HOOK)).as_ref() {
            hook.call(this);
        }
    }
}

/// The old SA-MP chat input must never be open while the chat is locked (Owner tests 2026-11-07: it was
/// already open at the first T after joining, and an open SA-MP input takes every key). Whatever opened it,
/// it is closed in the same frame: its enabled flag off, then SA-MP's own cursor / controls restore
/// (an Escape sent to the window did NOT close it on 0.3.DL: it flashed on and off).
fn close_samp_input(now: u64) {
    use client_api::samp::inputs::{show_cursor, Input};

    if !Input::is_active() {
        return;
    }

    if let Some(input) = Input::get() {
        input.m_bEnabled = 0;
    }

    show_cursor(false);   // SA-MP: no cursor, player controls back (CGame::ProcessInputEnabling)

    if now.saturating_sub(LAST_INPUT_CLOSE_MS.load(Ordering::Relaxed)) >= 2000 {
        LAST_INPUT_CLOSE_MS.store(now, Ordering::Relaxed);
        tracing::info!("VSL: old SA-MP chat input was open while the chat is locked: closed");
    }
}

/// keys that open VSL pages instead of SA-MP / GTA screens
pub enum VslKey {
    Chat,
    Esc,
    Tab,
    Help,
    Map,
}

pub fn key_for(wparam: usize) -> Option<VslKey> {
    match wparam {
        0x54 | 0x75 if is_locked() => Some(VslKey::Chat), // T, F6
        0x1B if is_ui_locked() && !esc_pass() => Some(VslKey::Esc), // ESC
        0x09 if is_ui_locked() => Some(VslKey::Tab),       // TAB
        0x70 if is_ui_locked() => Some(VslKey::Help),      // F1
        0x4D if is_ui_locked() => Some(VslKey::Map),       // M
        _ => None,
    }
}

/// GetAsyncKeyState hook: SA-MP also POLLS some keys (chat T / F6, scoreboard TAB, help F1) instead of
/// reading window messages, so swallowing the message alone is not enough (Owner test 2026-11-06: T opened
/// the old chat input, F1 the SA-MP key list). While the lock that owns a key is on and no SA-MP input,
/// SA-MP dialog or GTA menu is using the keyboard, SA-MP sees that key as "not pressed".
pub fn hides_async_key(key: i32) -> bool {
    use client_api::gta::menu_manager::CMenuManager;
    use client_api::samp::inputs::{Dialog, Input};

    let owned = match key {
        0x54 | 0x75 => is_locked(),                  // T, F6
        0x09 | 0x70 => is_ui_locked(),               // TAB, F1
        _ => false,
    };

    owned && !Input::is_active() && !Dialog::is_input_focused() && !CMenuManager::is_menu_active()
}

/// diagnostics for the key locks (one line per press of a VSL key, info level)
pub fn log_key(wparam: usize, input_blocked: bool, routed: bool) {
    use client_api::gta::menu_manager::CMenuManager;
    use client_api::samp::inputs::{Dialog, Input};

    if !matches!(wparam, 0x54 | 0x75 | 0x1B | 0x09 | 0x70 | 0x4D) {
        return;
    }

    tracing::info!(
        key = wparam,
        routed,
        chat_lock = is_locked(),
        ui_lock = is_ui_locked(),
        page_focus = input_blocked,
        samp_input = Input::is_active(),
        samp_dialog = Dialog::is_input_focused(),
        gta_menu = CMenuManager::is_menu_active(),
        "VSL key"
    );
}

pub fn event_name(key: &VslKey) -> &'static str {
    match key {
        VslKey::Chat => "vsl:chatkey",
        VslKey::Esc => "vsl:esc",
        VslKey::Tab => "vsl:tab",
        VslKey::Help => "vsl:f1",
        VslKey::Map => "vsl:mapkey",
    }
}

pub fn swallow_next_char() {
    SWALLOW_CHAR.store(true, Ordering::Relaxed);
}

pub fn take_swallowed_char() -> bool {
    SWALLOW_CHAR.swap(false, Ordering::Relaxed)
}
